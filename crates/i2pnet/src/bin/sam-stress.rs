//! `sam-stress` — the R2 stress harness (`docs/LIVE-TESTING.md` §4,
//! `docs/PROTOCOL.i2p-bt` §2.6).
//!
//! Opens two SAM sessions on one live router and drives N concurrent streams
//! between them, reporting connect-latency distribution and failure counts as
//! N climbs. This is the instrument for R2: whether one session handles many
//! concurrent streams, the suspected root of XD-style flakiness. One session
//! listens (SAM `STREAM FORWARD`) and echoes; the other dials its destination
//! N times at once.
//!
//! Needs a live router — this is a manual tool, not a CI test. It prints a
//! clear message and exits non-zero when SAM is unreachable.
//!
//! **Everything here is on a deadline.** A stress harness whose failure mode is
//! "sits there" is worse than useless: it burns the operator's session and
//! tells them nothing. Streams that do not finish are counted as unfinished and
//! reported as such, rather than parked on forever — which is what an
//! unbounded `join` on a wedged dial amounts to. Raise the budget with
//! `CLOVE_STRESS_DEADLINE` when testing a slow router.
//!
//! Usage:
//!   sam-stress [N]                  # N concurrent streams (default 32)
//!   `CLOVE_SAM_PORT=7656` ...       # SAM port the listener uses (default 7656)
//!   `CLOVE_SAM_PORT_DIAL=7666` ...  # SAM port the dialer uses (default: same)
//!   `CLOVE_STRESS_DEADLINE=360` ... # seconds for the whole run (default 360)
//!
//! Setting `CLOVE_SAM_PORT_DIAL` to a *different* router makes this a
//! cross-router test — one destination on router A dialed from router B, which
//! is the path a real swarm peer takes. `make cross` sweeps the pairs.

use std::env;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use i2pnet::sam::{DEFAULT_SAM_PORT, SamConfig, SamListener, SamSession, unique_nickname};
use i2pnet::{DestHash, I2pDialer, I2pListener, I2pNamingLookup};

/// Bytes each stream sends and expects echoed back — enough to exercise a
/// real round-trip through tunnels, small enough not to dominate timing.
///
/// "Small enough not to dominate timing" turned out to be false on a busy
/// router: at 32 concurrent streams a 64 KiB echo took a median of 78 seconds,
/// so the payload, not the concurrency, was what the run measured. R2 asks
/// about the *control plane* — whether one session's dial path degrades as
/// streams pile up — and that question is answered by a payload small enough
/// to prove the stream carries bytes at all. Override with
/// `CLOVE_STRESS_PAYLOAD` (bytes); the default is unchanged so old and new
/// reports stay comparable.
const PAYLOAD_LEN: usize = 64 * 1024;

/// Payload size for this run, from `CLOVE_STRESS_PAYLOAD` (bytes).
///
/// Floored at one byte: a zero-length echo would "succeed" without the stream
/// ever carrying anything, which is precisely the claim the echo exists to
/// make.
fn payload_len() -> usize {
    env::var("CLOVE_STRESS_PAYLOAD")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(PAYLOAD_LEN, |n: usize| n.max(1))
}

/// Per-attempt dial timeout passed to the trait (yosemite ignores it; the
/// router's own `CANT_REACH_PEER` timeout governs — see `PROTOCOL.i2p-bt` 2.3).
const STREAM_TIMEOUT: Duration = Duration::from_secs(60);

/// Pause between dial retries during warmup.
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Default budget for the whole run, from "sessions up" to the report.
/// Override with `CLOVE_STRESS_DEADLINE` (seconds).
const RUN_DEADLINE: Duration = Duration::from_secs(360);

/// Slice of the run budget reserved for the echo exchange, so a dialer that
/// spends its whole life retrying still leaves time to prove a stream works
/// once it finally connects.
const ECHO_RESERVE: Duration = Duration::from_secs(60);

/// How long a dialer keeps retrying while the target's leaseSet is still
/// propagating (a fresh destination is briefly unreachable — `CantReachPeer`),
/// **derived from the run budget** rather than fixed.
///
/// These two numbers used to be independent constants, and the readiness probe
/// set the run budget to 90s while the warmup budget stayed at 240s. The result
/// was a probe that could not pass: it killed the run a third of the way into
/// the retry loop it had just configured, and reported "not completing dials"
/// for a router that had simply not been given the time the harness itself said
/// it needed. Deriving one from the other makes that class of mistake
/// unrepresentable — ask for 90 seconds and you get 90 seconds of retrying, not
/// 240 seconds of it truncated to 90.
fn warmup_deadline(run: Duration) -> Duration {
    // Clamped back under `run` last: a budget smaller than the reserve would
    // otherwise be widened by the `max` to something the run cannot survive —
    // reintroducing, in miniature, the exact bug this function exists to
    // prevent. The dial loop attempts once before it consults this deadline, so
    // even a zero window still produces one honest attempt.
    run.saturating_sub(ECHO_RESERVE).max(RETRY_BACKOFF).min(run)
}

/// How long an echo handler waits on its half of an exchange, **derived from
/// the run budget** rather than fixed. Handlers hold a loopback socket, so
/// unlike the dial side this is enforceable.
///
/// It was a flat 120 seconds, chosen when a 64 KiB echo was assumed quick. It
/// is a *per-read* timeout, so it never cut a slow-but-steady transfer — but
/// under real congestion a 120-second gap between reads is reachable, and when
/// it fires the handler drops the stream without echoing. The dialer then sees
/// EOF and reports `failed to fill whole buffer`, which reads as the router
/// dropping the stream and is in fact us hanging up on ourselves. A stall
/// bound inside the measurement's own range does not measure, it censors.
///
/// The run's deadline is the bound that matters — nothing outlives it, and
/// teardown is separately bounded by [`TEARDOWN_GRACE`] — so the handler is
/// given the whole run and the stall it reports is a real one.
fn echo_timeout(run: Duration) -> Duration {
    run.max(RETRY_BACKOFF)
}

/// How long the run waits for the echo listener to wind down once the deadline
/// has passed, before giving up on it and reporting anyway.
const TEARDOWN_GRACE: Duration = Duration::from_secs(10);

/// How many descriptor-exhausted accepts to ride out before the listener gives
/// up. Each retry costs [`RETRY_BACKOFF`], and handlers hand descriptors back
/// as they finish, so a few retries clear a transient squeeze; a persistent one
/// means N is simply past what `ulimit -n` allows.
const FD_RETRY_LIMIT: u32 = 5;

/// Is this the process (`EMFILE`) or the system (`ENFILE`) out of file
/// descriptors?
///
/// `std::io::ErrorKind` has no stable variant for either, so the raw errno is
/// the only way to ask. Both values are the same across every Linux ABI clove
/// targets, and a wrong answer here costs a misleading hint rather than a
/// wrong result.
fn is_fd_exhaustion(e: &io::Error) -> bool {
    const EMFILE: i32 = 24;
    const ENFILE: i32 = 23;
    matches!(e.raw_os_error(), Some(EMFILE | ENFILE))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sam-stress: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Attach the "is a router even there?" hint to a session-setup failure.
///
/// Only to setup failures: once the sessions are up the router is plainly
/// running, and repeating the question there sends the reader to check
/// something they have already proved.
///
/// The port is passed rather than re-read from the environment, because in a
/// cross-router run the two sessions sit on different routers and a hint that
/// always named the listener's port would send the reader to the wrong one.
fn setup_hint<T>(port: u16, result: io::Result<T>) -> io::Result<T> {
    result.inspect_err(|_| {
        eprintln!(
            "sam-stress: could not bring up a SAM session on 127.0.0.1:{port}. \
             Is a router running with SAM enabled there? \
             (CLOVE_SAM_PORT / CLOVE_SAM_PORT_DIAL change the ports)"
        );
    })
}

/// One SAM session on `port`, named so it is findable in the router's log.
fn session_on(port: u16, prefix: &str) -> io::Result<SamSession> {
    setup_hint(
        port,
        SamSession::connect(&SamConfig {
            samv3_tcp_port: port,
            nickname: unique_nickname(prefix),
            ..Default::default()
        }),
    )
}

fn run() -> io::Result<()> {
    let n = env::args()
        .nth(1)
        .map_or(Ok(32), |a| a.parse::<usize>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "N must be a positive integer"))?
        .max(1);
    let port = sam_port();
    let dialing = dial_port();
    announce_topology(port, dialing);

    let listen = session_on(port, "clove-stress-listen")?;
    let dial = Arc::new(session_on(dialing, "clove-stress-dial")?);

    // Captured before `listen` is consumed by the listener.
    let listen_b64_head: String = listen.local_dest_b64().chars().take(24).collect();
    let dialer_dest = dial.local_dest();

    let listener = setup_hint(port, SamListener::forward(Arc::new(listen)))?;
    let target = listener.local_dest();
    let listen_port = listener.local_port();
    let deadline_budget = run_deadline();

    describe_endpoints(&dial, target, dialer_dest, &listen_b64_head);

    eprintln!(
        "sam-stress: driving {n} concurrent streams (deadline {}s, warmup retries {}s)…",
        deadline_budget.as_secs(),
        warmup_deadline(deadline_budget).as_secs()
    );

    // Listener side: accept up to N streams and echo on each, until told to
    // stop. It is told to stop rather than counting to N, because when a dial
    // fails there is no Nth stream to accept and counting would wait for it
    // forever — the original bug this harness had.
    let stop = Arc::new(AtomicBool::new(false));
    let acceptor_stop = Arc::clone(&stop);
    // Counted through a shared cell rather than returned, because the join
    // below is bounded and may give up before the thread has a value to give.
    let accepted = Arc::new(AtomicU32::new(0));
    let acceptor_accepted = Arc::clone(&accepted);
    // Streams the *listening* side abandoned. Without this, a handler that
    // times out mid-echo shows up only as the dialer's "failed to fill whole
    // buffer" — an error that reads like the router dropped the stream when in
    // fact we hung up on ourselves.
    let echo_gave_up = Arc::new(AtomicU32::new(0));
    let acceptor_gave_up = Arc::clone(&echo_gave_up);
    let echo_budget = echo_timeout(deadline_budget);
    let acceptor = thread::spawn(move || {
        echo_server(
            &listener,
            n,
            &acceptor_stop,
            &acceptor_accepted,
            &acceptor_gave_up,
            echo_budget,
        );
    });

    // Dialer side: N threads, each dials the listener's destination once and
    // reports through the channel. Results are collected by deadline, not by
    // joining: a thread stuck in a read on a yosemite stream cannot be
    // interrupted (no socket to time out), so it must not be waited on.
    let start = Instant::now();
    let deadline = start + deadline_budget;
    // Two slots per dialer: each reports its connect the moment the dial
    // returns and its outcome later, so a thread never blocks on a full
    // channel. Bounded, per SCOPE §4.
    let (tx, rx) = mpsc::sync_channel(2 * n);
    let tries = Arc::new(AtomicU32::new(0));
    for _ in 0..n {
        let dial = Arc::clone(&dial);
        let tries = Arc::clone(&tries);
        let tx = tx.clone();
        thread::spawn(move || dial_once(&dial, target, &tries, &tx));
    }
    drop(tx);

    let (mut connects, mut rtts, failures) = collect_events(&rx, n, deadline);
    let wall = start.elapsed();
    let unfinished = n - (rtts.len() + failures.len());
    let dialed = connects.len();

    // Unblock the acceptor: raise the flag, then poke its loopback port so a
    // blocking accept() returns and sees it.
    stop.store(true, Ordering::Relaxed);
    let _ = i2pnet::sam::poke_listener(listen_port);
    join_before(acceptor, TEARDOWN_GRACE);

    let gave_up = echo_gave_up.load(Ordering::Relaxed);
    report(
        &Run {
            n,
            wall,
            tried: tries.load(Ordering::Relaxed),
            dialed,
            unfinished,
            gave_up,
        },
        &mut connects,
        &mut rtts,
        &failures,
    );
    // Printed under the numbers rather than in place of them: the table is the
    // measurement, this is the reading of it.
    if accepted.load(Ordering::Relaxed) == 0 && (unfinished > 0 || rtts.is_empty()) {
        explain_silent_forward();
    }
    // Unfinished first: it is the more specific diagnosis. "Everything failed"
    // when nothing actually returned an error would send the reader looking
    // for error text that does not exist.
    if unfinished > 0 {
        return Err(unfinished_error(
            unfinished,
            n,
            dialed,
            tries.load(Ordering::Relaxed),
            deadline_budget,
            &rtts,
        ));
    }
    if rtts.is_empty() {
        return Err(io::Error::other(
            "every stream failed — see the failures above",
        ));
    }
    Ok(())
}

/// Drain dialer events until every stream reaches a terminal state or the
/// deadline passes, returning connects, round-trips, and failure texts.
///
/// Collected by deadline rather than by joining the dialer threads: a thread
/// blocked in a read on a live stream cannot be interrupted, so waiting on one
/// is how a harness hangs. A stream still in flight when the clock runs out is
/// simply absent from the terminal counts, and the caller reports it as
/// unfinished — with its connect time already banked here.
fn collect_events(
    rx: &mpsc::Receiver<Event>,
    n: usize,
    deadline: Instant,
) -> (Vec<Duration>, Vec<Duration>, Vec<String>) {
    let mut connects = Vec::with_capacity(n);
    let mut rtts = Vec::with_capacity(n);
    let mut failures = Vec::new();
    while rtts.len() + failures.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            // Not terminal: the stream dialed and is now echoing. Banked here
            // so the connect distribution covers every dial that happened, not
            // only those whose echo also beat the deadline.
            Ok(Event::Connected(connect)) => connects.push(connect),
            Ok(Event::Echoed(rtt)) => rtts.push(rtt),
            Ok(Event::Failed(e)) => failures.push(e),
            // Out of time, or every sender is gone and nothing further can
            // arrive. Either way there is nothing left to wait for.
            Err(_) => break,
        }
    }
    (connects, rtts, failures)
}

/// The whole-run budget from `CLOVE_STRESS_DEADLINE` (seconds), or
/// [`RUN_DEADLINE`].
fn run_deadline() -> Duration {
    env::var("CLOVE_STRESS_DEADLINE")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(RUN_DEADLINE, Duration::from_secs)
}

/// The SAM control port the *listening* session uses: `CLOVE_SAM_PORT`, or the
/// `SAMv3` default.
fn sam_port() -> u16 {
    env::var("CLOVE_SAM_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SAM_PORT)
}

/// The SAM control port the *dialing* session uses — `CLOVE_SAM_PORT_DIAL`,
/// defaulting to the listener's, which is the original one-router behaviour.
///
/// Two routers is the more faithful test, and possibly the easier one. Both
/// sessions on one router means a destination dialing a sibling it shares a
/// netDb with, and at least emissary resolves that through a full lookup and
/// times out rather than short-circuiting to the leaseSet it already holds
/// (`PROTOCOL.i2p-bt` 2.6c). A swarm peer is never in that position: it is on
/// somebody else's router, reached the ordinary way. Splitting the ports lets
/// the same harness measure the path clove will actually use, and tells us
/// whether a same-router failure is about clove at all.
fn dial_port() -> u16 {
    env::var("CLOVE_SAM_PORT_DIAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(sam_port)
}

/// Say which routers the two sessions live on, before anything can fail.
fn announce_topology(listen: u16, dialing: u16) {
    if listen == dialing {
        eprintln!("sam-stress: connecting to SAM on 127.0.0.1:{listen} (two sessions)…");
        eprintln!(
            "sam-stress: both sessions share one router — see PROTOCOL.i2p-bt 2.6c \
             on why that may be the harder case. CLOVE_SAM_PORT_DIAL points the \
             dialer at another router."
        );
    } else {
        eprintln!(
            "sam-stress: listening on 127.0.0.1:{listen}, dialing from \
             127.0.0.1:{dialing} (cross-router — the swarm path)…"
        );
    }
}

/// Name both endpoints, then check the target resolves before dialing it.
///
/// **Who is who.** Reading a router's log against a previous run, it was not
/// possible to tell whether a `connect to destination <id>` line named the
/// listener or something else entirely — the local and remote ids emissary
/// prints are not obviously the same encoding, so the two sides of a dial
/// looked like different destinations when they may well have been one. The
/// harness knows the answer for certain; printing it here means that question
/// is never again settled by squinting at two logs.
///
/// **Does a b32 resolve at all.** The lookup was added to separate "the
/// leaseSet is not visible" from "it resolves but will not carry a stream".
/// First contact with three routers showed it cannot carry that claim on its
/// own: i2pd answered `InvalidKey`, Java I2P `KeyNotFound` after 3.6s, and
/// emissary `KeyNotFound` in 0.0s — and a lookup that fails in no time did not
/// consult the network. SAM `NAMING LOOKUP` is specified for address-book names
/// and `ME`; whether it resolves a `.b32.i2p` label is router-dependent, and at
/// least one router rejects the form outright. So the probe now *reports* what
/// the router said and leaves the diagnosis alone.
///
/// The dialer's own destination is looked up as a control. Both are local
/// destinations on the same router queried the same way, so if the dialer
/// cannot resolve *itself* the answer says nothing about the listener's
/// publication — it says this router does not answer b32 lookups for its own
/// session destinations, which is a fact about the router's naming service.
/// Divergence between the two is the interesting case and the only one that
/// implicates the target.
fn describe_endpoints(dial: &SamSession, target: DestHash, dialer: DestHash, listen_b64: &str) {
    eprintln!("sam-stress: listener dest {}", target.to_b32());
    eprintln!("sam-stress:   (b64 head)  {listen_b64}…");
    eprintln!("sam-stress: dialer  dest  {}", dialer.to_b32());
    eprintln!(
        "sam-stress: dialing       {} (== listener)",
        target.to_b32()
    );

    let listener_ok = report_lookup(dial, "listener", target);
    let control_ok = report_lookup(dial, "dialer (control)", dialer);
    match (listener_ok, control_ok) {
        (false, false) => eprintln!(
            "sam-stress: neither b32 resolves, including the dialer's own — this \
             router does not answer NAMING LOOKUP for its own session \
             destinations, so the failures say nothing about publication. Judge \
             the dials below on their own."
        ),
        (false, true) => eprintln!(
            "sam-stress: the dialer resolves but the listener does not — b32 \
             lookup works on this router, so the listener's leaseSet really is \
             not visible. Chase publication."
        ),
        (true, _) => {}
    }
}

/// One `NAMING LOOKUP`, reported as fact. Returns whether it resolved.
fn report_lookup(dial: &SamSession, what: &str, expect: DestHash) -> bool {
    let probe = Instant::now();
    match dial.lookup(&expect.to_b32()) {
        Ok(found) if found == expect => {
            eprintln!(
                "sam-stress: lookup {what}: resolved in {:.1}s",
                probe.elapsed().as_secs_f64()
            );
            true
        }
        Ok(found) => {
            eprintln!(
                "sam-stress: lookup {what}: resolved to a DIFFERENT destination {} \
                 (expected {}) — a router or harness bug, not a slow network",
                found.to_b32(),
                expect.to_b32()
            );
            false
        }
        Err(e) => {
            eprintln!(
                "sam-stress: lookup {what}: router said {e} after {:.1}s",
                probe.elapsed().as_secs_f64()
            );
            false
        }
    }
}

/// What a dialer thread reports, as it happens.
///
/// Two messages rather than one result, because the single-result shape hid
/// the measurement R2 actually wants. A stream that dialed in 400 ms and was
/// still echoing at the deadline used to be counted "unfinished" with its
/// connect time discarded, so the connect percentiles described only streams
/// whose echo *also* finished in time — a survivorship filter that made a
/// congested router look like it had a tight, healthy dial path. Reporting the
/// connect the moment it happens makes the dial distribution cover every dial.
enum Event {
    /// The dial returned; the stream now exists and is echoing.
    Connected(Duration),
    /// The echo came back and matched, `Duration` measured from dial start.
    Echoed(Duration),
    /// Terminal failure, already rendered for the report.
    Failed(String),
}

/// Counts and timings for one whole run, grouped so [`report`] takes an
/// argument list a reader can hold in their head.
struct Run {
    n: usize,
    wall: Duration,
    tried: u32,
    dialed: usize,
    unfinished: usize,
    gave_up: u32,
}

/// `attempts` is a shared counter rather than a field of [`Sample`], because
/// the runs worth diagnosing are the ones that never produce a `Sample`. When
/// it was per-sample and summed on the success arm only, a run where every
/// dial was still retrying at the deadline reported `dial tries: 0` — which
/// reads as "clove never dialed" while the router's own log showed ten
/// attempts, and sends the reader looking in entirely the wrong place. A
/// counter bumped before each attempt is visible whatever the thread goes on
/// to do, including nothing.
fn dial_once(
    dialer: &SamSession,
    target: DestHash,
    tries: &AtomicU32,
    tx: &mpsc::SyncSender<Event>,
) {
    let deadline = Instant::now() + warmup_deadline(run_deadline());
    let (mut stream, start) = loop {
        tries.fetch_add(1, Ordering::Relaxed);
        let start = Instant::now();
        match dialer.dial(target, STREAM_TIMEOUT) {
            Ok(stream) => {
                // Reported before the echo is attempted: this is the fact R2
                // turns on, and it must survive a stream that never echoes.
                let _ = tx.send(Event::Connected(start.elapsed()));
                break (stream, start);
            }
            Err(_) if Instant::now() < deadline => thread::sleep(RETRY_BACKOFF),
            Err(e) => {
                let _ = tx.send(Event::Failed(e.to_string()));
                return;
            }
        }
    };

    let len = payload_len();
    let payload = vec![0xA5u8; len];
    let mut back = vec![0u8; len];
    let outcome = stream
        .write_all(&payload)
        .and_then(|()| stream.read_exact(&mut back))
        .map_err(|e| e.to_string())
        .and_then(|()| {
            if back == payload {
                Ok(start.elapsed())
            } else {
                Err("echoed bytes did not match what was sent".to_owned())
            }
        });
    let _ = tx.send(match outcome {
        Ok(rtt) => Event::Echoed(rtt),
        Err(e) => Event::Failed(e),
    });
}

/// Accept up to `n` inbound streams and echo `PAYLOAD_LEN` bytes on each, one
/// handler thread per stream, until `stop` is raised.
///
/// Stops on the flag rather than on a count: if a dial fails there is no
/// corresponding inbound stream, and an accept loop counting to `n` waits for
/// one that is never coming.
///
/// `arrived` counts inbound streams for the caller. It is a shared cell rather
/// than a return value because the caller's join is bounded: a run that ends
/// with this thread still blocked must still be able to say whether the router
/// ever forwarded anything, and that answer is the whole diagnosis when it did
/// not (see [`explain_silent_forward`]).
fn echo_server(
    listener: &SamListener,
    n: usize,
    stop: &AtomicBool,
    arrived: &AtomicU32,
    gave_up: &Arc<AtomicU32>,
    echo_budget: Duration,
) {
    let mut handlers = Vec::with_capacity(n);
    let mut accepted = 0usize;
    let mut exhausted = 0u32;
    while accepted < n && !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            // A connection that never sent its destination header is not an
            // inbound stream; keep waiting for one that is.
            Ok(None) => {}
            Ok(Some((stream, _from))) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                accepted += 1;
                arrived.store(
                    u32::try_from(accepted).unwrap_or(u32::MAX),
                    Ordering::Relaxed,
                );
                // A handler holds a loopback socket, so its waits are
                // genuinely boundable — do it, or a peer that connects and
                // says nothing parks this thread for the run.
                let _ = stream.set_timeouts(Some(echo_budget));
                let gave_up = Arc::clone(gave_up);
                handlers.push(thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf = vec![0u8; payload_len()];
                    if stream.read_exact(&mut buf).is_ok() {
                        if stream.write_all(&buf).is_err() {
                            gave_up.fetch_add(1, Ordering::Relaxed);
                        }
                    } else {
                        gave_up.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            // Running out of descriptors is the operator's `ulimit -n`, not
            // the router's verdict, and it is recoverable: handlers finishing
            // hand their descriptors back. Stopping the whole listener on the
            // first one turned a partial limit into a total loss — every
            // remaining dial then had nothing to accept it, and a run reported
            // 1024 identical failures about a machine that ran out of files.
            Err(e) if is_fd_exhaustion(&e) => {
                exhausted += 1;
                if exhausted == 1 {
                    eprintln!(
                        "sam-stress: out of file descriptors at {accepted} accepted stream(s) \
                         ({e}). Each stream costs two — raise `ulimit -n` (N={n} wants about \
                         {} plus headroom) or lower N. Continuing with what fits.",
                        2 * n + 16
                    );
                }
                if exhausted > FD_RETRY_LIMIT {
                    eprintln!(
                        "sam-stress: still out of descriptors after {FD_RETRY_LIMIT} retries; \
                         listener stopping."
                    );
                    break;
                }
                thread::sleep(RETRY_BACKOFF);
            }
            Err(e) => {
                if !stop.load(Ordering::Relaxed) {
                    eprintln!("sam-stress: accept failed ({e}); listener stopping");
                }
                break;
            }
        }
    }
    for h in handlers {
        let _ = h.join();
    }
}

/// Wait up to `grace` for the echo listener to wind down, then report anyway
/// and leave it to the process exit.
///
/// Bounded for the same reason the dial results are collected by deadline
/// rather than by joining (see the module header): an echo handler blocked on
/// a socket outlives the run's budget, and an unbounded join here quietly
/// extended a 240-second run past 300 — long enough for the wrapper in
/// `ci/live-report.sh` to kill the process before it could print the report it
/// had already computed. Overshooting a deadline by seconds is a nuisance;
/// overshooting it far enough to lose the numbers is the failure mode this
/// harness exists to prevent, and it had reintroduced it in its own teardown.
fn join_before(handle: thread::JoinHandle<()>, grace: Duration) {
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(grace).is_err() {
        eprintln!(
            "sam-stress: the echo listener did not stop within {}s; reporting without it \
             (its threads go with the process).",
            grace.as_secs()
        );
    }
}

/// Explain a run in which the router forwarded nothing at all.
///
/// [`SamListener::forward`] binds to `127.0.0.1` and issues `STREAM FORWARD`
/// with no `HOST=`, so the router connects back to whatever address it sees our
/// SAM control connection arriving from. Those are the same address only when
/// the router shares this network namespace. A router in a container dials the
/// peer correctly, accepts the stream, and then cannot hand it over — which
/// reads from here as a router that will not carry a stream, and is nothing of
/// the sort.
///
/// Printed only when *nothing* arrived. A run where some streams landed has
/// already disproved it, and a hint that fired on a partial failure would send
/// the reader somewhere the evidence does not point.
fn explain_silent_forward() {
    eprintln!(
        "sam-stress: the router accepted STREAM FORWARD and then forwarded nothing — not one \
         inbound stream arrived, so every dial above failed on the listening side."
    );
    eprintln!(
        "sam-stress: our forwarded listener is bound to 127.0.0.1 and the FORWARD carries no \
         HOST=, so the router connects back to wherever it sees our SAM control connection \
         coming from. Those agree only when the router shares this network namespace. If this \
         router runs in a container, the inbound half cannot work here and this result says \
         nothing about clove — see docs/LIVE-TESTING.md §3.1."
    );
}

/// Print the run summary: successes, failures, and connect/RTT percentiles.
///
/// `unfinished` is reported separately from `failures` on purpose. A stream
/// that errored told us something; a stream still sitting in a read when the
/// deadline arrived told us nothing except that it was sitting there, and
/// folding the two together would hide exactly the symptom worth chasing.
#[allow(clippy::too_many_arguments, reason = "a report of eight numbers")]
fn report(run: &Run, connects: &mut [Duration], rtts: &mut [Duration], failures: &[String]) {
    connects.sort_unstable();
    rtts.sort_unstable();
    let n = run.n;
    let ok = rtts.len();

    println!("── sam-stress: {n} concurrent streams ──");
    // Dialed first: it is the control-plane number, and the one that stays
    // meaningful when the echoes are still in flight.
    println!(
        "  dialed    : {}/{n} (the stream was established)",
        run.dialed
    );
    println!("  echoed    : {ok}/{n} (…and carried {} bytes back)", {
        payload_len()
    });
    println!("  failed    : {}", failures.len());
    println!(
        "  unfinished: {} (dialed, echo still in flight at the deadline)",
        run.unfinished
    );
    if run.gave_up > 0 {
        println!(
            "  we hung up: {} (our echo handler timed out mid-stream — these \
             show as the dialer's `failed to fill whole buffer`)",
            run.gave_up
        );
    }
    println!(
        "  dial tries: {} (> dialed ⇒ leaseSet-warmup retries)",
        run.tried
    );
    println!("  wall clock: {:.2}s", run.wall.as_secs_f64());
    if !connects.is_empty() {
        println!(
            "  connect ms: min {} · p50 {} · p90 {} · p99 {} · max {}",
            millis(pct(connects, 0.0)),
            millis(pct(connects, 50.0)),
            millis(pct(connects, 90.0)),
            millis(pct(connects, 99.0)),
            millis(pct(connects, 100.0)),
        );
    }
    if !rtts.is_empty() {
        println!(
            "  rtt ms    : min {} · p50 {} · p90 {} · p99 {} · max {}",
            millis(pct(rtts, 0.0)),
            millis(pct(rtts, 50.0)),
            millis(pct(rtts, 90.0)),
            millis(pct(rtts, 99.0)),
            millis(pct(rtts, 100.0)),
        );
    }
    report_failures(failures);
    println!("{}", result_line(run, connects, rtts, failures.len()));
}

/// Failures, grouped by text with a count.
///
/// A run that hits the descriptor limit produces a thousand identical lines,
/// of which the report used to print ten and then "… 1014 more failures" —
/// hiding whether those 1014 were the same thing or fourteen different things.
/// The distinct causes are what a reader needs; the multiplicity is one number.
fn report_failures(failures: &[String]) {
    let mut grouped: Vec<(&str, usize)> = Vec::new();
    for f in failures {
        if let Some(entry) = grouped.iter_mut().find(|(text, _)| *text == f.as_str()) {
            entry.1 += 1;
        } else {
            grouped.push((f.as_str(), 1));
        }
    }
    grouped.sort_by(|a, b| b.1.cmp(&a.1));
    for (text, count) in grouped.iter().take(10) {
        if *count == 1 {
            println!("  fail      : {text}");
        } else {
            println!("  fail ×{count:<4}: {text}");
        }
    }
    if grouped.len() > 10 {
        println!("  … {} further distinct failures", grouped.len() - 10);
    }
}

/// One stable, greppable line per run, for `ci/sam-stress-sweep.sh`.
///
/// A sweep that scraped the pretty table would break the first time a column
/// was reworded — and this table has been reworded twice. Keys are explicit so
/// a field can be added without shifting the ones already there.
fn result_line(run: &Run, connects: &[Duration], rtts: &[Duration], failed: usize) -> String {
    // An empty sample reports -1 rather than 0: a run where nothing dialed has
    // no median, and a 0 there would average into a sweep's table as if it
    // were an impossibly fast connect.
    let stat = |xs: &[Duration], p: f64| -> String {
        if xs.is_empty() {
            "-1".to_owned()
        } else {
            millis(pct(xs, p)).to_string()
        }
    };
    format!(
        "sam-stress-result\tn={}\tdialed={}\techoed={}\tfailed={}\tunfinished={}\tgave_up={}\t\
         tries={}\tconnect_p50_ms={}\tconnect_p99_ms={}\trtt_p50_ms={}\twall_s={:.2}\tpayload={}",
        run.n,
        run.dialed,
        rtts.len(),
        failed,
        run.unfinished,
        run.gave_up,
        run.tried,
        stat(connects, 50.0),
        stat(connects, 99.0),
        stat(rtts, 50.0),
        run.wall.as_secs_f64(),
        payload_len(),
    )
}

fn millis(d: Duration) -> u128 {
    d.as_millis()
}

/// Why streams were still running when the clock ran out — and, first, *which
/// half* ran out of time, because the two have opposite answers.
///
/// Streams that never dialed point at the router: no peers, no tunnels, an
/// unresolvable target. Streams that dialed and are still moving payload point
/// at the budget and the payload size, and say nothing about the router's
/// willingness to carry streams. Reporting both as "not completing dials" sent
/// an operator to check tunnels on a router that had established all 128.
#[allow(
    clippy::cast_precision_loss,
    reason = "stream counts are small; this is a printed estimate"
)]
fn unfinished_error(
    unfinished: usize,
    n: usize,
    dialed: usize,
    tried: u32,
    budget: Duration,
    rtts: &[Duration],
) -> io::Error {
    // Which half ran out of time decides where the reader should look, and the
    // two have opposite answers. This used to say "is not completing dials"
    // unconditionally and then append arithmetic from PROTOCOL 2.6a's
    // per-session dial serialization — an entry marked [superseded by §2.12]
    // since dials stopped going through yosemite. It was the *second* piece of
    // code left shaped around that removed constraint, which is the exact cost
    // §2.6a stayed in the file to warn about. The observation that caught it:
    // thirty concurrent dials landing within a 5 ms band, which no serialized
    // queue produces.
    let diagnosis = if dialed >= n {
        let pace = median(rtts).map_or_else(String::new, |r| {
            format!(
                " Completed echoes took a median of {:.0}s each, so the payload \
                 is the cost here, not the dialing — shrink it with \
                 CLOVE_STRESS_PAYLOAD to measure the dial path alone.",
                r.as_secs_f64()
            )
        });
        format!(
            "every stream dialed — all {n} established — and {unfinished} were \
             still moving payload when the deadline hit. That is throughput, \
             not a router refusing streams: raise CLOVE_STRESS_DEADLINE, or \
             lower the payload.{pace}"
        )
    } else {
        format!(
            "only {dialed} of {n} streams established. The router accepted the \
             session but is not completing dials — check it has peers and built \
             tunnels, and see the leaseSet lookup above: if that failed, the \
             target is unreachable rather than slow."
        )
    };
    io::Error::other(format!(
        "{unfinished} of {n} streams had not finished after {}s ({tried} dial \
         attempts made): {diagnosis}",
        budget.as_secs()
    ))
}

/// Median of a pre-sorted slice, or `None` when nothing succeeded.
fn median(sorted: &[Duration]) -> Option<Duration> {
    (!sorted.is_empty()).then(|| pct(sorted, 50.0))
}

/// Nearest-rank percentile of a pre-sorted slice. `p` is 0.0–100.0.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "index arithmetic on a bounded sample count; rounding is intended"
)]
fn pct(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let last = sorted.len() - 1;
    let idx = ((p / 100.0) * last as f64).round() as usize;
    sorted[idx.min(last)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression that cost an entire interop run: the readiness probe set
    /// the run budget to 90s while the retry loop kept its own 240s constant,
    /// so the harness killed the run a third of the way through the retrying it
    /// had just asked for and blamed the router. The two numbers must not be
    /// independently settable — retrying longer than the run can last is never
    /// a coherent thing to ask for.
    #[test]
    fn the_retry_budget_never_outlives_the_run_it_belongs_to() {
        for secs in [1u64, 5, 30, 60, 90, 240, 360, 3_600] {
            let run = Duration::from_secs(secs);
            let warmup = warmup_deadline(run);
            assert!(
                warmup <= run,
                "a {secs}s run allows {}s of retrying, which it cannot survive",
                warmup.as_secs()
            );
        }
    }

    /// A generous budget must actually reach the dialer, or the probe is slow
    /// and still useless.
    #[test]
    fn a_long_run_spends_most_of_itself_retrying() {
        let warmup = warmup_deadline(Duration::from_secs(360));
        assert_eq!(warmup, Duration::from_secs(300));
    }

    /// A budget below the echo reserve collapses to the budget itself rather
    /// than being rounded up past it. The dial loop tries once before checking
    /// this deadline, so a zero window still yields one attempt — and one
    /// attempt that reports honestly beats a loop that promises more time than
    /// the run has.
    #[test]
    fn a_budget_under_the_reserve_collapses_to_the_budget() {
        assert_eq!(
            warmup_deadline(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(warmup_deadline(Duration::ZERO), Duration::ZERO);
    }

    /// Teardown is on a deadline too. A listener thread that will not stop —
    /// the live case is an echo handler parked on a socket read — used to be
    /// joined without a bound, so the process outlived the budget it had just
    /// reported against and `ci/live-report.sh` killed it before it could
    /// print. The wait must end whether or not the thread does.
    #[test]
    fn a_wedged_listener_cannot_outlast_the_teardown_grace() {
        let wedged = thread::spawn(|| thread::sleep(Duration::from_secs(30)));
        let grace = Duration::from_millis(50);
        let start = Instant::now();
        join_before(wedged, grace);
        let waited = start.elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "join_before waited {waited:?} on a thread that sleeps for 30s"
        );
    }

    /// The sweep greps this line, so its keys are a contract. Adding a field is
    /// fine; renaming or reordering one silently empties a column of
    /// `ci/sam-stress-sweep.sh`'s table, which is the kind of break that looks
    /// like a bad run rather than a bad parse.
    #[test]
    fn the_result_line_carries_every_key_the_sweep_reads() {
        let run = Run {
            n: 32,
            wall: Duration::from_secs_f64(360.0),
            tried: 33,
            dialed: 32,
            unfinished: 2,
            gave_up: 1,
        };
        let connects = [Duration::from_millis(419), Duration::from_millis(423)];
        let rtts = [Duration::from_millis(78_394)];
        let line = result_line(&run, &connects, &rtts, 0);

        for key in [
            "n=32",
            "dialed=32",
            "echoed=1",
            "failed=0",
            "unfinished=2",
            "gave_up=1",
            "tries=33",
            "connect_p50_ms=",
            "connect_p99_ms=",
            "rtt_p50_ms=78394",
            "wall_s=360.00",
            "payload=",
        ] {
            assert!(line.contains(key), "result line is missing {key}: {line}");
        }
        assert!(line.starts_with("sam-stress-result\t"));
    }

    /// A run with nothing to measure must not report a zero, which a sweep
    /// would average in as an impossibly fast connect.
    #[test]
    fn an_empty_sample_reports_minus_one_not_zero() {
        let run = Run {
            n: 4,
            wall: Duration::from_secs(1),
            tried: 4,
            dialed: 0,
            unfinished: 0,
            gave_up: 0,
        };
        let line = result_line(&run, &[], &[], 4);
        assert!(line.contains("connect_p50_ms=-1"), "{line}");
        assert!(line.contains("rtt_p50_ms=-1"), "{line}");
    }

    /// …and it still returns promptly when the thread does stop, rather than
    /// always spending the whole grace period.
    #[test]
    fn a_listener_that_stops_is_not_waited_out() {
        let quick = thread::spawn(|| {});
        let start = Instant::now();
        join_before(quick, Duration::from_secs(30));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "join_before sat on the full grace for a thread that had already finished"
        );
    }
}
