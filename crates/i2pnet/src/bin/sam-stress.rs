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
const PAYLOAD_LEN: usize = 64 * 1024;

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

/// How long an echo handler waits on its half of an exchange. Handlers hold a
/// loopback socket, so unlike the dial side this is enforceable.
const ECHO_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the run waits for the echo listener to wind down once the deadline
/// has passed, before giving up on it and reporting anyway.
const TEARDOWN_GRACE: Duration = Duration::from_secs(10);

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
    let acceptor =
        thread::spawn(move || echo_server(&listener, n, &acceptor_stop, &acceptor_accepted));

    // Dialer side: N threads, each dials the listener's destination once and
    // reports through the channel. Results are collected by deadline, not by
    // joining: a thread stuck in a read on a yosemite stream cannot be
    // interrupted (no socket to time out), so it must not be waited on.
    let start = Instant::now();
    let deadline = start + deadline_budget;
    // Bounded at n: every dialer sends exactly once, so this never blocks a
    // sender, and SCOPE §4 has no unbounded channels.
    let (tx, rx) = mpsc::sync_channel(n);
    let tries = Arc::new(AtomicU32::new(0));
    for _ in 0..n {
        let dial = Arc::clone(&dial);
        let tries = Arc::clone(&tries);
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = tx.send(dial_once(&dial, target, &tries));
        });
    }
    drop(tx);

    let mut connects = Vec::with_capacity(n);
    let mut rtts = Vec::with_capacity(n);
    let mut failures = Vec::new();
    let mut finished = 0usize;
    while finished < n {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok(Ok(sample)) => {
                connects.push(sample.connect);
                rtts.push(sample.rtt);
                finished += 1;
            }
            Ok(Err(e)) => {
                failures.push(e.to_string());
                finished += 1;
            }
            // Out of time, or every sender is gone and nothing further can
            // arrive. Either way there is nothing left to wait for.
            Err(_) => break,
        }
    }
    let wall = start.elapsed();
    let unfinished = n - finished;

    // Unblock the acceptor: raise the flag, then poke its loopback port so a
    // blocking accept() returns and sees it.
    stop.store(true, Ordering::Relaxed);
    let _ = i2pnet::sam::poke_listener(listen_port);
    join_before(acceptor, TEARDOWN_GRACE);

    report(
        n,
        wall,
        tries.load(Ordering::Relaxed),
        unfinished,
        &mut connects,
        &mut rtts,
        &failures,
    );
    // Printed under the numbers rather than in place of them: the table is the
    // measurement, this is the reading of it.
    if accepted.load(Ordering::Relaxed) == 0 && (unfinished > 0 || connects.is_empty()) {
        explain_silent_forward();
    }
    // Unfinished first: it is the more specific diagnosis. "Everything failed"
    // when nothing actually returned an error would send the reader looking
    // for error text that does not exist.
    if unfinished > 0 {
        return Err(unfinished_error(
            unfinished,
            n,
            tries.load(Ordering::Relaxed),
            deadline_budget,
            &connects,
        ));
    }
    if connects.is_empty() {
        return Err(io::Error::other(
            "every stream failed — see the failures above",
        ));
    }
    Ok(())
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

/// One dial + echo exchange, timed. `connect`/`rtt` are measured from the
/// successful attempt, so warmup retries do not inflate the reported latency.
struct Sample {
    connect: Duration,
    rtt: Duration,
}

/// `attempts` is a shared counter rather than a field of [`Sample`], because
/// the runs worth diagnosing are the ones that never produce a `Sample`. When
/// it was per-sample and summed on the success arm only, a run where every
/// dial was still retrying at the deadline reported `dial tries: 0` — which
/// reads as "clove never dialed" while the router's own log showed ten
/// attempts, and sends the reader looking in entirely the wrong place. A
/// counter bumped before each attempt is visible whatever the thread goes on
/// to do, including nothing.
fn dial_once(dialer: &SamSession, target: DestHash, tries: &AtomicU32) -> io::Result<Sample> {
    let deadline = Instant::now() + warmup_deadline(run_deadline());
    let (mut stream, connect, start) = loop {
        tries.fetch_add(1, Ordering::Relaxed);
        let start = Instant::now();
        match dialer.dial(target, STREAM_TIMEOUT) {
            Ok(stream) => break (stream, start.elapsed(), start),
            Err(_) if Instant::now() < deadline => thread::sleep(RETRY_BACKOFF),
            Err(e) => return Err(e),
        }
    };

    let payload = vec![0xA5u8; PAYLOAD_LEN];
    stream.write_all(&payload)?;
    let mut back = vec![0u8; PAYLOAD_LEN];
    stream.read_exact(&mut back)?;
    let rtt = start.elapsed();

    if back != payload {
        return Err(io::Error::other("echoed bytes did not match what was sent"));
    }
    Ok(Sample { connect, rtt })
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
fn echo_server(listener: &SamListener, n: usize, stop: &AtomicBool, arrived: &AtomicU32) {
    let mut handlers = Vec::with_capacity(n);
    let mut accepted = 0usize;
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
                let _ = stream.set_timeouts(Some(ECHO_TIMEOUT));
                handlers.push(thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf = vec![0u8; PAYLOAD_LEN];
                    if stream.read_exact(&mut buf).is_ok() {
                        let _ = stream.write_all(&buf);
                    }
                }));
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
fn report(
    n: usize,
    wall: Duration,
    attempts: u32,
    unfinished: usize,
    connects: &mut [Duration],
    rtts: &mut [Duration],
    failures: &[String],
) {
    connects.sort_unstable();
    rtts.sort_unstable();
    let ok = connects.len();

    println!("── sam-stress: {n} concurrent streams ──");
    println!("  succeeded : {ok}/{n}");
    println!("  failed    : {}", failures.len());
    println!("  unfinished: {unfinished} (still running when the deadline hit)");
    println!("  dial tries: {attempts} (> succeeded ⇒ leaseSet-warmup retries)");
    println!("  wall clock: {:.2}s", wall.as_secs_f64());
    if !connects.is_empty() {
        println!(
            "  connect ms: min {} · p50 {} · p90 {} · p99 {} · max {}",
            millis(pct(connects, 0.0)),
            millis(pct(connects, 50.0)),
            millis(pct(connects, 90.0)),
            millis(pct(connects, 99.0)),
            millis(pct(connects, 100.0)),
        );
        println!(
            "  rtt ms    : min {} · p50 {} · p90 {} · p99 {} · max {}",
            millis(pct(rtts, 0.0)),
            millis(pct(rtts, 50.0)),
            millis(pct(rtts, 90.0)),
            millis(pct(rtts, 99.0)),
            millis(pct(rtts, 100.0)),
        );
    }
    for (i, f) in failures.iter().enumerate() {
        if i < 10 {
            println!("  fail[{i}] : {f}");
        }
    }
    if failures.len() > 10 {
        println!("  … {} more failures", failures.len() - 10);
    }
}

fn millis(d: Duration) -> u128 {
    d.as_millis()
}

/// Why streams were still running when the clock ran out, with the arithmetic
/// that usually explains it.
///
/// Dial *setup* serializes per session (`PROTOCOL.i2p-bt` §2.6a: yosemite's
/// sync connect takes `&mut self`, so `SamSession::dial` holds the session
/// mutex for the whole connect). A run therefore needs roughly N times one
/// connect, which turns any fixed budget into a silent ceiling on N — at N=128
/// even a brisk 3s connect wants 384s, more than the default allows. Spelling
/// that out here stops the reader diagnosing a router for what is the harness's
/// own arithmetic.
#[allow(
    clippy::cast_precision_loss,
    reason = "stream counts are small; this is a printed estimate"
)]
fn unfinished_error(
    unfinished: usize,
    n: usize,
    tried: u32,
    budget: Duration,
    connects: &[Duration],
) -> io::Error {
    let arithmetic = median(connects).map_or_else(String::new, |c| {
        let need = c.as_secs_f64() * n as f64;
        format!(
            " Dial setup serializes per session (PROTOCOL.i2p-bt 2.6a), so {n} \
             streams need about {need:.0}s at the observed connect time — {}the \
             budget was {}s.",
            if need > budget.as_secs_f64() {
                "more than "
            } else {
                ""
            },
            budget.as_secs()
        )
    });
    io::Error::other(format!(
        "{unfinished} of {n} streams had not finished after {}s ({tried} dial \
         attempts made). The router accepted the session but is not completing \
         dials — check it has peers and built tunnels, and see the leaseSet \
         lookup above: if that failed, the target is unreachable rather than \
         slow. Raise CLOVE_STRESS_DEADLINE if it is merely slow.{arithmetic}",
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
