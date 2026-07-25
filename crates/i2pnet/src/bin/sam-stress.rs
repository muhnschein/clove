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
//! clear message and exits non-zero when SAM is unreachable, and never hangs
//! silently. See `docs/LIVE-TESTING.md` §6.1 for how its output feeds M1.
//!
//! Usage:
//!   sam-stress [N]            # N concurrent streams (default 32)
//!   `CLOVE_SAM_PORT=7656` ... # SAM control port (default 7656)

use std::env;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use i2pnet::sam::{DEFAULT_SAM_PORT, SamConfig, SamListener, SamSession};
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

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sam-stress: {e}");
            eprintln!(
                "sam-stress: is a router running with SAM enabled on \
                 127.0.0.1:{}? (set CLOVE_SAM_PORT to change)",
                sam_port()
            );
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let n = env::args()
        .nth(1)
        .map_or(Ok(32), |a| a.parse::<usize>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "N must be a positive integer"))?
        .max(1);
    let port = sam_port();

    eprintln!("sam-stress: connecting to SAM on 127.0.0.1:{port} (two sessions)…");
    let listen = SamSession::connect(&SamConfig {
        samv3_tcp_port: port,
        nickname: "clove-stress-listen".to_owned(),
        ..Default::default()
    })?;
    let dial = Arc::new(SamSession::connect(&SamConfig {
        samv3_tcp_port: port,
        nickname: "clove-stress-dial".to_owned(),
        ..Default::default()
    })?);

    let listener = SamListener::forward(Arc::new(listen))?;
    let target = listener.local_dest();
    eprintln!("sam-stress: sessions up; driving {n} concurrent streams…");

    // Listener side: accept N streams, echo PAYLOAD_LEN bytes on each.
    let acceptor = thread::spawn(move || echo_server(&listener, n));

    // Dialer side: N threads, each dials the listener's destination once.
    let start = Instant::now();
    let mut dialers = Vec::with_capacity(n);
    for _ in 0..n {
        let dial = Arc::clone(&dial);
        dialers.push(thread::spawn(move || dial_once(&dial, target)));
    }

    let mut connects = Vec::with_capacity(n);
    let mut rtts = Vec::with_capacity(n);
    let mut failures = Vec::new();
    let mut attempts = 0u32;
    for handle in dialers {
        match handle.join() {
            Ok(Ok(sample)) => {
                connects.push(sample.connect);
                rtts.push(sample.rtt);
                attempts += sample.attempts;
            }
            Ok(Err(e)) => failures.push(e.to_string()),
            Err(_) => failures.push("dialer thread panicked".to_owned()),
        }
    }
    let wall = start.elapsed();
    let _ = acceptor.join();

    report(n, wall, attempts, &mut connects, &mut rtts, &failures);
    if connects.is_empty() {
        return Err(io::Error::other(
            "every stream failed — see the failures above",
        ));
    }
    Ok(())
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

/// Accept `n` inbound streams and echo `PAYLOAD_LEN` bytes on each, one
/// handler thread per stream.
fn echo_server(listener: &SamListener, n: usize) {
    let mut handlers = Vec::with_capacity(n);
    for _ in 0..n {
        match listener.accept() {
            Ok((mut stream, _from)) => {
                handlers.push(thread::spawn(move || {
                    let mut buf = vec![0u8; PAYLOAD_LEN];
                    if stream.read_exact(&mut buf).is_ok() {
                        let _ = stream.write_all(&buf);
                    }
                }));
            }
            Err(e) => {
                eprintln!("sam-stress: accept failed ({e}); listener stopping");
                break;
            }
        }
    }
    for h in handlers {
        let _ = h.join();
    }
}

/// Print the run summary: successes, failures, and connect/RTT percentiles.
fn report(
    n: usize,
    wall: Duration,
    attempts: u32,
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
