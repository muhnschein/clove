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
//!   `CLOVE_SAM_PORT=7656` ...       # SAM control port (default 7656)
//!   `CLOVE_STRESS_DEADLINE=360` ... # seconds for the whole run (default 360)

use std::env;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use i2pnet::sam::{DEFAULT_SAM_PORT, SamConfig, SamListener, SamSession, unique_nickname};
use i2pnet::{DestHash, I2pDialer, I2pListener};

/// Bytes each stream sends and expects echoed back — enough to exercise a
/// real round-trip through tunnels, small enough not to dominate timing.
const PAYLOAD_LEN: usize = 64 * 1024;

/// Per-attempt dial timeout passed to the trait (yosemite ignores it; the
/// router's own `CANT_REACH_PEER` timeout governs — see `PROTOCOL.i2p-bt` 2.3).
const STREAM_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a dialer keeps retrying while the target's leaseSet is still
/// propagating (a fresh destination is briefly unreachable — `CantReachPeer`).
const WARMUP_DEADLINE: Duration = Duration::from_secs(240);

/// Pause between dial retries during warmup.
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Default budget for the whole run, from "sessions up" to the report.
///
/// Comfortably longer than [`WARMUP_DEADLINE`] so a legitimately slow warmup
/// still finishes, and short enough that a wedged router costs minutes rather
/// than an afternoon. Override with `CLOVE_STRESS_DEADLINE` (seconds).
const RUN_DEADLINE: Duration = Duration::from_secs(360);

/// How long an echo handler waits on its half of an exchange. Handlers hold a
/// loopback socket, so unlike the dial side this is enforceable.
const ECHO_TIMEOUT: Duration = Duration::from_secs(120);

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
fn setup_hint<T>(result: io::Result<T>) -> io::Result<T> {
    result.inspect_err(|_| {
        eprintln!(
            "sam-stress: could not bring up a SAM session on 127.0.0.1:{}. \
             Is a router running with SAM enabled there? (CLOVE_SAM_PORT changes the port)",
            sam_port()
        );
    })
}

fn run() -> io::Result<()> {
    let n = env::args()
        .nth(1)
        .map_or(Ok(32), |a| a.parse::<usize>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "N must be a positive integer"))?
        .max(1);
    let port = sam_port();

    eprintln!("sam-stress: connecting to SAM on 127.0.0.1:{port} (two sessions)…");
    let listen = setup_hint(SamSession::connect(&SamConfig {
        samv3_tcp_port: port,
        nickname: unique_nickname("clove-stress-listen"),
        ..Default::default()
    }))?;
    let dial = Arc::new(setup_hint(SamSession::connect(&SamConfig {
        samv3_tcp_port: port,
        nickname: unique_nickname("clove-stress-dial"),
        ..Default::default()
    }))?);

    let listener = setup_hint(SamListener::forward(Arc::new(listen)))?;
    let target = listener.local_dest();
    let listen_port = listener.local_port();
    let deadline_budget = run_deadline();
    eprintln!(
        "sam-stress: sessions up; driving {n} concurrent streams (deadline {}s)…",
        deadline_budget.as_secs()
    );

    // Listener side: accept up to N streams and echo on each, until told to
    // stop. It is told to stop rather than counting to N, because when a dial
    // fails there is no Nth stream to accept and counting would wait for it
    // forever — the original bug this harness had.
    let stop = Arc::new(AtomicBool::new(false));
    let acceptor_stop = Arc::clone(&stop);
    let acceptor = thread::spawn(move || echo_server(&listener, n, &acceptor_stop));

    // Dialer side: N threads, each dials the listener's destination once and
    // reports through the channel. Results are collected by deadline, not by
    // joining: a thread stuck in a read on a yosemite stream cannot be
    // interrupted (no socket to time out), so it must not be waited on.
    let start = Instant::now();
    let deadline = start + deadline_budget;
    // Bounded at n: every dialer sends exactly once, so this never blocks a
    // sender, and SCOPE §4 has no unbounded channels.
    let (tx, rx) = mpsc::sync_channel(n);
    for _ in 0..n {
        let dial = Arc::clone(&dial);
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = tx.send(dial_once(&dial, target));
        });
    }
    drop(tx);

    let mut connects = Vec::with_capacity(n);
    let mut rtts = Vec::with_capacity(n);
    let mut failures = Vec::new();
    let mut attempts = 0u32;
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
                attempts += sample.attempts;
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
    let _ = acceptor.join();

    report(
        n,
        wall,
        attempts,
        unfinished,
        &mut connects,
        &mut rtts,
        &failures,
    );
    // Unfinished first: it is the more specific diagnosis. "Everything failed"
    // when nothing actually returned an error would send the reader looking
    // for error text that does not exist.
    if unfinished > 0 {
        return Err(io::Error::other(format!(
            "{unfinished} of {n} streams had not finished after {}s. The router \
             accepted the session but is not completing dials — check it has \
             peers and built tunnels. Raise CLOVE_STRESS_DEADLINE if it is \
             merely slow.",
            deadline_budget.as_secs()
        )));
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

/// The SAM control port from `CLOVE_SAM_PORT`, or the `SAMv3` default.
fn sam_port() -> u16 {
    env::var("CLOVE_SAM_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SAM_PORT)
}

/// One dial + echo exchange, timed. `attempts` counts dial tries (>1 means the
/// target was still warming up); `connect`/`rtt` are measured from the
/// successful attempt, so warmup retries do not inflate the reported latency.
struct Sample {
    connect: Duration,
    rtt: Duration,
    attempts: u32,
}

fn dial_once(dialer: &SamSession, target: DestHash) -> io::Result<Sample> {
    let deadline = Instant::now() + WARMUP_DEADLINE;
    let mut attempts = 0u32;
    let (mut stream, connect, start) = loop {
        attempts += 1;
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
    Ok(Sample {
        connect,
        rtt,
        attempts,
    })
}

/// Accept up to `n` inbound streams and echo `PAYLOAD_LEN` bytes on each, one
/// handler thread per stream, until `stop` is raised.
///
/// Stops on the flag rather than on a count: if a dial fails there is no
/// corresponding inbound stream, and an accept loop counting to `n` waits for
/// one that is never coming.
fn echo_server(listener: &SamListener, n: usize, stop: &AtomicBool) {
    let mut handlers = Vec::with_capacity(n);
    let mut accepted = 0usize;
    while accepted < n && !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _from)) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                accepted += 1;
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
