//! `cloved(8)` — the clove daemon.
//!
//! Loads config, opens the data dir, hosts the engine (a [`registry::Registry`]
//! of live torrents over the SAM backend), and serves the local `/v1/` HTTP
//! API (hand-rolled HTTP/1.1 + JSON, Q6) over a unix socket with token auth.
//! The SAM session comes up in the background on the supervisor's backoff;
//! until then torrents wait in "waiting-for-router". Once initialisation is
//! done the daemon restricts itself (Landlock/seccomp, [`sandbox`]).

mod registry;
mod sandbox;
mod transmission;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use clove_core::config::{Config, Defaults};
use clove_core::http::{self, Response};
use clove_core::json::Value;
use clove_core::swarm::{InboundDemux, SwarmConfig};
use i2pnet::DestHash;
use i2pnet::api::{ApiListener, ApiStream};
use i2pnet::sam::{SamConfig, SamListener, SamSession};
use i2pnet::supervisor::ReconnectPolicy;

use crate::registry::{ActionError, AddError, Registry, RemoveError};

/// How often live progress is snapshotted to resume files.
const PERSIST_INTERVAL: Duration = Duration::from_secs(30);

/// Cap on an API request body (a `.torrent` or magnet; generous for status).
const MAX_REQUEST_BODY: usize = 2 * 1024 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cloved: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parsed command line: `cloved [-C|--check] [-c <config>]`.
struct Args {
    check: bool,
    config_path: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

/// The argument parser proper, over any iterator so tests can drive it.
/// `--help` still exits the process: it is a terminal action either way, and
/// pretending otherwise would mean a success path that prints usage and then
/// carries on.
fn parse_args_from<I: Iterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut check = false;
    let mut config_path = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-C" | "--check" => check = true,
            "-c" | "--config" => {
                let path = args
                    .next()
                    .ok_or_else(|| format!("{arg} needs a path argument"))?;
                config_path = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                println!("usage: cloved [-C|--check] [-c <config>]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(Args { check, config_path })
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let defaults = Defaults::from_env().map_err(|e| e.to_string())?;
    // An explicit -c must exist; the default path may simply be absent, in
    // which case the built-in defaults are the whole configuration.
    let text = match &args.config_path {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?
        }
        None => std::fs::read_to_string(defaults.config_path()).unwrap_or_default(),
    };
    let config = Config::parse(&text, &defaults).map_err(|e| e.to_string())?;

    if args.check {
        let from = args
            .config_path
            .clone()
            .unwrap_or_else(|| defaults.config_path());
        println!("cloved: configuration OK");
        println!(
            "  config     {} {}",
            from.display(),
            if from.exists() {
                ""
            } else {
                "(absent; using defaults)"
            }
        );
        println!("  data_dir   {}", config.data_dir.display());
        println!("  api_socket {}", config.api_socket.display());
        println!("  sam_address {}", config.sam_address);
        return Ok(());
    }

    // `0700`, not the umask's opinion. The token and the destination key live
    // here and are `0600` themselves, but a traversable directory is one
    // `chmod` accident away from mattering, and nothing else needs to look
    // inside it. Applied on an existing directory too: an install that was
    // created before this, or by a permissive umask, is the case worth fixing.
    std::fs::create_dir_all(&config.data_dir)
        .map_err(|e| format!("creating data dir {}: {e}", config.data_dir.display()))?;
    if let Err(e) = std::fs::set_permissions(
        &config.data_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    ) {
        eprintln!(
            "cloved: could not restrict {} to 0700: {e}",
            config.data_dir.display()
        );
    }
    if let Some(parent) = config.api_socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating socket dir {}: {e}", parent.display()))?;
    }
    let token = load_or_create_token(&config.data_dir).map_err(|e| e.to_string())?;

    let registry = Registry::open(&config.data_dir, registry::Limits::from(&config))
        .map_err(|e| format!("opening registry in {}: {e}", config.data_dir.display()))?;
    eprintln!("cloved: {} torrent(s) loaded", registry.count());

    let (listener, tcp_listener) = bind_listeners(&config)?;

    eprintln!("cloved: {}", restrict_self(&config));

    let rpc = build_rpc(&config)?;

    let daemon = Arc::new(Daemon {
        start: Instant::now(),
        sam_address: config.sam_address.clone(),
        token,
        peer_id: build_peer_id().map_err(|e| e.to_string())?,
        registry: Mutex::new(registry),
        config: config.clone(),
        rpc,
        router: Mutex::new("connecting"),
    });

    // Resume metadata fetches for magnets loaded from disk. Collect first:
    // a `for .. in lock(..)` holds the guard for the whole loop body, and
    // spawn_metadata_fetch re-locks it (deadlock).
    let pending = lock(&daemon.registry).pending_hashes();
    for info_hash in pending {
        spawn_metadata_fetch(&daemon, info_hash);
    }

    // Torrents whose resume file claimed more than it could confirm — a crash,
    // a truncated write, a bad sector, an edited state file. Each is already
    // marked as scanning, so nothing starts it until its pass publishes. Same
    // shape as the magnet loop above, and for the same reason: the job runs
    // with the registry unlocked, and `run_scan` re-locks it to report.
    let scans = lock(&daemon.registry).pending_scans();
    for job in scans {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || {
            if let Err(e) = run_scan(&daemon, &job) {
                eprintln!("cloved: re-verifying after an unclean stop: {e}");
            }
        });
    }

    spawn_sam_supervisor(
        &daemon,
        &config.sam_address,
        Identity::new(&config.data_dir, config.ephemeral),
    );
    spawn_persist_loop(&daemon);
    if let Some(dir) = config.watch_dir.clone() {
        spawn_watch_dir(&daemon, dir);
    }
    // The TCP listener, when there is one, gets its own accept loop on its own
    // thread; the unix socket keeps the main one. Two loops rather than a
    // select over both: each is a blocking accept, `serve` already spawns per
    // connection, and a poll loop here would be a second concurrency model in
    // a daemon that has exactly one (Q5).
    if let Some(tcp) = tcp_listener {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || {
            if let Err(e) = serve(&tcp, &daemon) {
                // Not fatal to the daemon: the unix socket is still serving,
                // and taking every torrent down because a TCP accept loop died
                // would be a worse outcome than losing the extra way in.
                eprintln!("cloved: TCP API listener stopped: {e}");
            }
        });
    }
    serve(&listener, &daemon)
}

/// How often the watch directory is looked at.
///
/// Polling, not `inotify`: a directory listing on a timer is a `read_dir` and
/// no new dependency, against a watch API that would be one. Nobody notices
/// five seconds on a torrent that is about to spend minutes building tunnels.
const WATCH_DIR_INTERVAL: Duration = Duration::from_secs(5);

/// Watch a directory for `.torrent` files and add each one it finds.
///
/// The file is renamed to `<name>.added` once it has been taken, which is what
/// stops the same torrent being offered every five seconds forever. A file
/// that fails to parse becomes `<name>.rejected` for the same reason — it is
/// not going to start parsing, and retrying it every tick would fill the log
/// with one complaint.
///
/// This is how external tooling drives clove without an API of its own, and it
/// is deliberately the dumbest thing that works.
fn spawn_watch_dir(daemon: &Arc<Daemon>, dir: PathBuf) {
    let daemon = Arc::clone(daemon);
    std::thread::spawn(move || {
        loop {
            scan_watch_dir(&daemon, &dir);
            std::thread::sleep(WATCH_DIR_INTERVAL);
        }
    });
}

/// One pass over the watch directory.
fn scan_watch_dir(daemon: &Arc<Daemon>, dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        // Not readable, or not there yet. Said once per tick would be noise;
        // an operator who set the key and mistyped it sees nothing added,
        // which is the same information.
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("torrent") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let added = lock(&daemon.registry).add_torrent(&bytes, registry::AddOptions::default());
        let suffix = match added {
            Ok((info_hash, job)) => {
                // Same as the API path: the initial hash of whatever is
                // already on disk runs with the registry unlocked.
                let _ = run_scan(daemon, &job);
                eprintln!(
                    "cloved: added {} from {}",
                    registry::hex(&info_hash),
                    path.display()
                );
                "added"
            }
            // A duplicate is not a failure: the operator dropped in something
            // already hosted, and the file has still been dealt with.
            Err(AddError::Duplicate) => "added",
            Err(e) => {
                eprintln!("cloved: not adding {}: {e}", path.display());
                "rejected"
            }
        };
        let taken = path.with_extension(format!("torrent.{suffix}"));
        if let Err(e) = fs::rename(&path, &taken) {
            eprintln!("cloved: leaving {} where it is: {e}", path.display());
        }
    }
}

/// Daemon state shared across connection threads.
struct Daemon {
    start: Instant,
    sam_address: String,
    token: String,
    /// Our wire identity for this run, decided before anything can need it.
    peer_id: [u8; 20],
    registry: Mutex<Registry<Arc<SamSession>>>,
    /// The configuration this run was started with, kept whole rather than
    /// copied out field by field. Two surfaces now report the operator's
    /// settings back — `clove show` reads some, `session-get` reads most — and
    /// one value with two homes is how they drift apart.
    ///
    /// Note what it is *not*: a live control surface. The daemon applies its
    /// configuration at start and nothing here rewrites it, which is why
    /// `session-set` refuses rather than pretending
    /// (`crates/cloved/src/transmission.rs`).
    config: Config,
    /// The Transmission-compatible surface, when `transmission_rpc yes` asked
    /// for one. `None` is what makes its routing and its second authentication
    /// scheme unreachable.
    rpc: Option<transmission::Rpc>,
    /// Router connection state shown in `/v1/status`.
    router: Mutex<&'static str>,
}

/// The TCP port of a `host:port` SAM address, or `None` for a unix-socket
/// path or anything unparseable. Used both to dial the bridge and to tell the
/// sandbox which port is the only one worth connecting to.
fn sam_tcp_port(sam_address: &str) -> Option<u16> {
    sam_address
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
}

/// The daemon's I2P identity across restarts (Q4).
///
/// One destination keypair per client, kept as SAM's own base64 private key
/// blob in `<data_dir>/destination.key` at `0600`. `ephemeral yes` in
/// `clove.conf` turns this off and every start gets a fresh transient
/// destination instead.
///
/// Why it matters beyond "nice to have a stable name": a destination that
/// changes is a peer nobody can reach. Trackers hand our destination to other
/// clients, PEX propagates it, and peers that dialled us hold it — all of which
/// point at an identity that no longer exists the moment the session is
/// rebuilt. With the session tree rebuilding on router loss, clove was
/// announcing a *new* identity every time, seeding the swarm with dead
/// destinations and guaranteeing the inbound half could never work.
struct Identity {
    /// `None` under `ephemeral yes` — nothing is read and nothing is written.
    path: Option<PathBuf>,
}

impl Identity {
    fn new(data_dir: &Path, ephemeral: bool) -> Identity {
        Identity {
            path: (!ephemeral).then(|| data_dir.join("destination.key")),
        }
    }

    /// The stored key, or `None` for a transient destination this run.
    ///
    /// A file that is present but not a key is refused rather than sent to the
    /// router. The failure it prevents is specific and nasty: `SESSION CREATE`
    /// with a malformed `DESTINATION=` is refused, the supervisor backs off and
    /// retries into the same refusal, and the daemon never connects again —
    /// §2.9's "back off into the same refusal forever", reached this time by a
    /// truncated file rather than a stale session id.
    fn load(&self) -> Option<String> {
        let path = self.path.as_ref()?;
        let text = match read_private_file(path) {
            Ok(Some(text)) => text,
            Ok(None) => return None,
            Err(e) => {
                eprintln!(
                    "cloved: not using the destination key in {}: {e}; starting with \
                     a new identity",
                    path.display()
                );
                return None;
            }
        };
        let key = text.trim();
        if is_private_key_blob(key) {
            return Some(key.to_owned());
        }
        eprintln!(
            "cloved: {} is not a usable destination key (truncated or corrupt?); \
             starting with a new identity",
            path.display()
        );
        None
    }

    /// Persist `key` if we do not already have it stored.
    ///
    /// Failure is logged, not fatal: a daemon that cannot write its key still
    /// works, it just comes back as somebody else next time. Saying so is the
    /// point — this silently not happening is exactly the bug being fixed.
    fn remember(&self, key: &str) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if std::fs::read_to_string(path).is_ok_and(|stored| stored.trim() == key) {
            return;
        }
        match write_private_file(path, key.as_bytes()) {
            Ok(()) => eprintln!("cloved: identity saved to {}", path.display()),
            Err(e) => eprintln!(
                "cloved: could not save the destination key to {}: {e}; this run's \
                 identity will not survive a restart",
                path.display()
            ),
        }
    }
}

/// Smallest private key material that can follow a destination in a SAM
/// key blob: a 256-byte private encryption key, then a private signing key,
/// of which the shortest defined is DSA-SHA1's 20 bytes.
///
/// A lower bound rather than an exact size on purpose. The exact figure depends
/// on the signing type named in the destination's certificate — 32 bytes for
/// Ed25519, 20 for DSA, more for the ECDSA and `RedDSA` families — and a table of
/// those is a thing to get out of date, which for a check like this means
/// refusing a key that was perfectly good. The bound catches what actually goes
/// wrong (truncation, and a destination stored where a key belongs) without
/// pretending to know more than it does.
const MIN_PRIVATE_KEY_BYTES: usize = 256 + 20;

/// Whether `text` is a SAM private key blob: I2P base64 that decodes to a
/// complete destination *plus* the private key material behind it.
///
/// A destination alone is a perfectly well-formed thing to find in this file and
/// completely useless as a `DESTINATION=` — it is the public half, and a router
/// cannot sign with it. So was "a destination and one byte", which is what this
/// used to accept: any trailing byte at all passed, and the resulting blob was
/// handed to `SESSION CREATE` on every attempt, refused every time, and retried
/// for the life of the process — §2.9's back-off-into-the-same-refusal, reached
/// by a file that was checked and found wanting-but-acceptable.
fn is_private_key_blob(text: &str) -> bool {
    let Some(bytes) = i2pnet::addr::i2p_base64_decode(text) else {
        return false;
    };
    i2pnet::addr::destination_len(&bytes)
        .and_then(|dest| bytes.len().checked_sub(dest))
        .is_some_and(|private| private >= MIN_PRIVATE_KEY_BYTES)
}

/// Supervise the SAM session in the background: connect on the reconnect
/// policy's backoff, attach the network, then wait for the session to end; on
/// session loss, tear the session tree down (detach the registry, stop and
/// poke the demux's accept loop) and rebuild — the SCOPE §4 reconnect
/// discipline.
fn spawn_sam_supervisor(daemon: &Arc<Daemon>, sam_address: &str, identity: Identity) {
    // The SAM backend dials 127.0.0.1:<port> by construction (Layer 1's
    // loopback rule); a unix-socket SAM path cannot be used by it.
    let Some(port) = sam_tcp_port(sam_address) else {
        eprintln!("cloved: sam_address {sam_address:?} is not host:port; running without a router");
        *lock(&daemon.router) = "unsupported-sam-address";
        return;
    };
    let daemon = Arc::clone(daemon);
    std::thread::spawn(move || {
        let policy = ReconnectPolicy::default();
        loop {
            let mut failures = 0u32;
            // Phase 1: bring the session tree up, backing off on failure.
            let (session, listener) = loop {
                match connect_session(port, identity.load()) {
                    Ok(pair) => break pair,
                    Err(e) => {
                        if failures == 0 {
                            eprintln!("cloved: waiting for router (SAM at 127.0.0.1:{port}): {e}");
                        }
                        failures = failures.saturating_add(1);
                        *lock(&daemon.router) = "waiting-for-router";
                        // Jittered, so several daemons on one host (or one
                        // daemon and one router restart script) do not retry in
                        // lockstep and hammer the bridge the moment it answers.
                        let base = policy.base_delay(failures);
                        std::thread::sleep(policy.jittered(base, random_roll()));
                    }
                }
            };
            let dest = session.local_dest();
            let forward_port = listener.local_port();
            eprintln!("cloved: router connected; we are {}", dest.to_b32());
            // Only now, with the router having accepted it: a key that failed
            // SESSION CREATE is not one worth keeping.
            identity.remember(session.private_key_b64());
            // The per-torrent ceiling reaches the engine by these two paths and
            // no other — the dial sweep's and the inbound demux's. Both come
            // from the same configured number, so raising it in clove.conf
            // raises it for dialling and accepting alike.
            let swarm_config = SwarmConfig {
                max_peers: daemon.config.torrent_peer_limit,
                ..SwarmConfig::default()
            };
            let demux = InboundDemux::new(swarm_config.max_peers);
            let _accept = demux.run(listener);
            lock(&daemon.registry).attach_network(
                Arc::clone(&session),
                Arc::clone(&demux),
                daemon.peer_id,
                swarm_config,
                session.local_dest_b64().to_owned(),
            );
            *lock(&daemon.router) = "connected";

            // Phase 2: wait for the session to end.
            //
            // This used to be a 30-second `PING` probe, which was wrong twice
            // over: it could not see a dead session for up to 90 seconds
            // (end-of-file on the control connection read as success), and
            // when it finally did, it had thrown away everything the router
            // had said about why. The session now watches its own control
            // connection, so this returns the moment the router hangs up and
            // returns its account of it (`PROTOCOL.i2p-bt` §2.13).
            let reason = session.wait_until_lost();

            // Phase 3: teardown, then rebuild from phase 1.
            eprintln!("cloved: router lost: {reason}");
            eprintln!("cloved: torrents wait while the session tree rebuilds");
            *lock(&daemon.router) = "waiting-for-router";
            demux.stop();
            let _ = i2pnet::sam::poke_listener(forward_port);
            lock(&daemon.registry).detach_network();
        }
    });
}

/// One session bring-up: connect and establish the forwarded listener.
///
/// `key` is the persisted identity when there is one (Q4); `None` asks the
/// router for a fresh transient destination, whose key the caller then stores.
fn connect_session(
    port: u16,
    key: Option<String>,
) -> std::io::Result<(Arc<SamSession>, SamListener)> {
    let session = Arc::new(SamSession::connect(&SamConfig {
        samv3_tcp_port: port,
        // Unique per attempt: a router that has not yet released the previous
        // session would refuse a fixed id with DuplicateId, and the
        // supervisor would back off into the same refusal forever. The session
        // *id* is not the identity — that is the destination key below — so it
        // can vary freely per attempt while the destination stays put.
        nickname: i2pnet::sam::unique_nickname("clove"),
        persistent_key: key,
        ..Default::default()
    })?);
    let listener = SamListener::forward(Arc::clone(&session))?;
    Ok((session, listener))
}

/// Periodically snapshot live progress into resume files.
fn spawn_persist_loop(daemon: &Arc<Daemon>) {
    let daemon = Arc::clone(daemon);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(PERSIST_INTERVAL);
            lock(&daemon.registry).persist_progress();
        }
    });
}

/// Pause between metadata-fetch rounds (announce + peer attempts).
const FETCH_ROUND_WAIT: Duration = Duration::from_secs(30);

/// Spawn the metadata-fetch thread for a pending magnet, at most once per
/// entry. The thread runs rounds — announce to the magnet's trackers for
/// peers, then try each peer for BEP 9 metadata — until the magnet resolves
/// or is removed; without a network it just sleeps and retries.
fn spawn_metadata_fetch(daemon: &Arc<Daemon>, info_hash: [u8; 20]) {
    if !lock(&daemon.registry).claim_fetch(&info_hash) {
        return;
    }
    let daemon = Arc::clone(daemon);
    std::thread::spawn(move || {
        let mut first_round = true;
        let mut rounds = 0u32;
        loop {
            // Outer None: the magnet resolved or was removed — stop.
            let Some(context) = lock(&daemon.registry).fetch_context(&info_hash) else {
                return;
            };
            if let Some(ctx) = context {
                rounds += 1;
                let (bytes, round) = try_fetch_round(&ctx, info_hash, first_round);
                eprintln!(
                    "cloved: magnet {} round {rounds}: {}",
                    registry::hex(&info_hash),
                    round.summary()
                );
                // Publish before acting on the result: a round that failed is
                // exactly the one whose report has to reach `clove list`.
                lock(&daemon.registry).note_fetch_round(&info_hash, rounds, &round);
                if let Some(bytes) = bytes {
                    let completed = lock(&daemon.registry).complete_magnet(&info_hash, &bytes);
                    match completed {
                        Ok(job) => {
                            // Unlocked, like every other scan; a magnet's files
                            // were only just laid out, so this one is quick.
                            let _ = run_scan(&daemon, &job);
                            eprintln!(
                                "cloved: magnet {} resolved; torrent added",
                                registry::hex(&info_hash)
                            );
                        }
                        Err(e) => eprintln!(
                            "cloved: magnet {} fetched but add failed: {e}",
                            registry::hex(&info_hash)
                        ),
                    }
                    return;
                }
            }
            first_round = false;
            std::thread::sleep(FETCH_ROUND_WAIT);
        }
    });
}

/// What one fetch round did, so the caller can log it and publish it.
///
/// This type exists because the round used to be six `let … else { continue }`
/// arms that dropped every error on the floor. A magnet that never resolved
/// therefore produced *no output at all* — no log line, no state beyond
/// `fetching-metadata`, nothing to distinguish "the tracker's name will not
/// resolve" from "the tracker returned no peers" from "thirty peers were
/// dialed and none served the metadata". The first live swarm run spent nine
/// minutes in exactly that hole. Every arm now records its reason
/// (`SCOPE.md` §9: error text written for the operator reading a log at 2am).
#[derive(Default)]
pub(crate) struct FetchRound {
    /// Trackers that answered with a peer list.
    pub(crate) trackers_ok: usize,
    /// Trackers that could not be built, resolved, dialed or announced to.
    pub(crate) trackers_failed: usize,
    /// Distinct peer destinations the trackers returned.
    pub(crate) peers_returned: usize,
    /// Peers dialed for metadata this round.
    pub(crate) peers_tried: usize,
    /// The most recent failure, with the stage that produced it. This is the
    /// line an operator needs; it is kept rather than merely logged so
    /// `clove list` can show it without anyone reading the daemon's stderr.
    pub(crate) last_error: Option<String>,
}

impl FetchRound {
    /// Record a failure against something safe to name — a tracker hostname is
    /// published in the torrent that carried it and is nobody's identity.
    fn fail(&mut self, stage: &str, what: &str, e: &dyn std::fmt::Display) {
        let text = format!("{stage} {what}: {e}");
        eprintln!("cloved: metadata fetch: {text}");
        self.last_error = Some(text);
    }

    /// Record a failure against a *peer*, without saying which one.
    ///
    /// The obvious thing to write here is the peer's b32, and it is what this
    /// did. `SECURITY.md` puts a peer's destination reaching "logs, error
    /// messages, or the local API" in the leak class, and this reached all
    /// three: straight to stderr, and kept in `last_error`, which `clove list`
    /// serves. A tracker chooses which peers we dial, so a tracker chooses
    /// which destinations end up recorded next to our torrent.
    ///
    /// Truncating or hashing it would not help — a b32 *is* the hash of the
    /// destination, and a stable tag stays linkable. So the peer is not named
    /// at all. The stage and the error survive, which is the part that says
    /// what to do next, and `peers_tried` already says how many there were.
    fn fail_peer(&mut self, stage: &str, e: &dyn std::fmt::Display) {
        let text = format!("{stage}: {e}");
        eprintln!("cloved: metadata fetch: {text}");
        self.last_error = Some(text);
    }

    /// One line summarising the round, or `None` when there is nothing worth
    /// saying — a round that resolved the magnet speaks for itself.
    fn summary(&self) -> String {
        format!(
            "{} tracker(s) answered, {} failed; {} peer(s) known, {} dialed for metadata",
            self.trackers_ok, self.trackers_failed, self.peers_returned, self.peers_tried
        )
    }
}

/// One fetch round: announce to each tracker for peers, then ask each peer
/// for the metadata. Returns synthesized `.torrent` bytes on success, plus a
/// report of what happened either way.
/// Generic so the mock network proves it in tests.
fn try_fetch_round<D>(
    ctx: &registry::FetchContext<D>,
    info_hash: [u8; 20],
    first_round: bool,
) -> (Option<Vec<u8>>, FetchRound)
where
    D: i2pnet::I2pDialer + i2pnet::I2pNamingLookup + Clone + Send + Sync + 'static,
{
    use clove_core::tracker;

    let mut round = FetchRound::default();
    let mut peers: Vec<DestHash> = Vec::new();
    for url in &ctx.trackers {
        let params = tracker::AnnounceParams {
            info_hash,
            peer_id: ctx.peer_id,
            uploaded: 0,
            downloaded: 0,
            left: 1, // metadata unknown: report as leeching
            event: if first_round {
                tracker::Event::Started
            } else {
                tracker::Event::Periodic
            },
            numwant: 30,
            our_dest_b64: &ctx.dest_b64,
        };
        let (host, request) = match tracker::build_announce(url, &params) {
            Ok(built) => built,
            Err(e) => {
                round.trackers_failed += 1;
                round.fail("tracker URL", url, &e);
                continue;
            }
        };
        // The stage that most often stalls a magnet, and the one that used to
        // be invisible: an address-book name the router has never heard of
        // fails here, and the naming cache then declines to ask again for up
        // to half an hour (i2pnet::naming) — so the symptom is not even a
        // stream of lookups, it is silence.
        let dest = match i2pnet::I2pNamingLookup::lookup(&ctx.naming, &host) {
            Ok(dest) => dest,
            Err(e) => {
                round.trackers_failed += 1;
                round.fail("resolving tracker", &host, &e);
                continue;
            }
        };
        let mut stream = match i2pnet::I2pDialer::dial(&ctx.dialer, dest, Duration::from_secs(120))
        {
            Ok(stream) => stream,
            Err(e) => {
                round.trackers_failed += 1;
                round.fail("dialing tracker", &host, &e);
                continue;
            }
        };
        // A magnet's rounds are sequential, so one tracker that accepts the
        // stream and then says nothing stops this magnet resolving at all —
        // not just against that tracker.
        let _ = i2pnet::I2pStream::set_timeouts(&stream, Some(tracker::ANNOUNCE_IO_TIMEOUT));
        match tracker::announce_over(&mut stream, &request) {
            Ok(response) => {
                round.trackers_ok += 1;
                peers.extend(response.peers);
            }
            Err(e) => {
                round.trackers_failed += 1;
                round.fail("announcing to", &host, &e);
                // The literal URL, for pasting into a browser aimed at the
                // same tracker. A tracker that refuses an announce as a
                // policy violation will not say which part it objected to,
                // and deleting parameters one at a time is the only thing
                // that finds out.
                eprintln!(
                    "cloved: the announce that failed was {}",
                    clove_core::tracker::announced_url(&host, &request)
                );
            }
        }
    }

    let peers = distinct(peers);
    round.peers_returned = peers.len();
    if round.trackers_ok > 0 && peers.is_empty() {
        // Not an error from anyone's point of view, and precisely the state
        // that looks identical to a failed announce from outside.
        round.last_error =
            Some("trackers answered but returned no peers for this info-hash".to_owned());
    }
    for peer in peers {
        round.peers_tried += 1;
        let stream = match i2pnet::I2pDialer::dial(&ctx.dialer, peer, Duration::from_secs(120)) {
            Ok(stream) => stream,
            Err(e) => {
                round.fail_peer("dialing a peer", &e);
                continue;
            }
        };
        match clove_core::torrent::fetch_metadata(stream, info_hash, ctx.peer_id) {
            Ok(meta) => {
                let bytes = clove_core::magnet::torrent_bytes(&meta.raw_info, &ctx.trackers);
                return (Some(bytes), round);
            }
            Err(e) => round.fail_peer("fetching metadata from a peer", &e),
        }
    }
    (None, round)
}

/// A jitter roll in `[0, 1)` for [`ReconnectPolicy::jittered`].
///
/// Randomness is the whole point — a fixed roll de-synchronises nothing — but a
/// system that will not give us any is not a reason to stop reconnecting, so the
/// fallback is "no jitter" rather than an error.
#[allow(
    clippy::cast_precision_loss,
    reason = "53 bits into an f64 mantissa is exact"
)]
fn random_roll() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        return 0.0;
    }
    let mantissa = u64::from_le_bytes(bytes) >> 11; // 53 bits
    mantissa as f64 / (1u64 << 53) as f64
}

/// Distinct destinations, in no particular order.
///
/// `Vec::dedup` only removes *neighbouring* duplicates, so the same peer
/// arriving from two trackers survives it and gets dialled twice.
fn distinct(mut peers: Vec<DestHash>) -> Vec<DestHash> {
    peers.sort_unstable_by_key(|d| d.0);
    peers.dedup();
    peers
}

/// The daemon's wire identity: the Q7 `-CV0001-` prefix plus 12 random bytes.
///
/// # Errors
///
/// The system refused to give us randomness. Fatal on purpose: the fallback
/// used to be a fixed string, which would have every instance that hit it
/// announcing under one peer id.
fn build_peer_id() -> std::io::Result<[u8; 20]> {
    let mut id = *b"-CV0001-............";
    let mut tail = [0u8; 12];
    getrandom::getrandom(&mut tail)
        .map_err(|e| std::io::Error::other(format!("getrandom for the peer id: {e}")))?;
    id[8..].copy_from_slice(&tail);
    Ok(id)
}

/// Accept loop: one thread per connection (Q5; API load is tiny). Only a fatal
/// accept error returns.
fn serve(listener: &ApiListener, daemon: &Arc<Daemon>) -> Result<(), String> {
    loop {
        match listener.accept() {
            Ok(stream) => {
                let daemon = Arc::clone(daemon);
                std::thread::spawn(move || {
                    if let Err(e) = handle(stream, &daemon) {
                        eprintln!("cloved: connection error: {e}");
                    }
                });
            }
            Err(e) => return Err(format!("accept failed: {e}")),
        }
    }
}

/// Serve one request: parse, authenticate, route, respond.
fn handle(mut stream: ApiStream, daemon: &Arc<Daemon>) -> std::io::Result<()> {
    let Ok(request) = http::read_request(&mut stream, MAX_REQUEST_BODY) else {
        return write_response(&mut stream, &error(400, "malformed request"));
    };

    // Which authentication scheme this path answers to. Two surfaces speak
    // different dialects — `/v1/` wants clove's token header, Transmission
    // wants Basic plus a CSRF id — so the choice is made once, here, and
    // **defaults to `/v1/`**. That default is what matters: an unrecognised
    // path falls through to the stricter scheme rather than to none, so a path
    // added later cannot be served unauthenticated by omission.
    let transmission = daemon
        .rpc
        .as_ref()
        .filter(|_| transmission::is_rpc_path(request.path()));

    if let Some(rpc) = transmission {
        if let Err(refusal) = transmission::authorize(rpc, &request, &daemon.token) {
            return write_response(&mut stream, &refusal);
        }
        let response = transmission::dispatch(daemon, &request);
        return write_response(&mut stream, &response);
    }

    // Token auth on every request, unix socket included (SCOPE §3).
    //
    // The shape check on our *own* token is the important half: an empty
    // expected value would match an empty `x-clove-token:` header and
    // authenticate every local caller. `load_or_create_token` will not hand us
    // one, and this makes that a belt as well as braces — the authentication
    // path should not depend on a loader elsewhere getting it right.
    let ok = is_well_formed_token(&daemon.token)
        && request
            .header("x-clove-token")
            .is_some_and(|got| constant_time_eq(got.as_bytes(), daemon.token.as_bytes()));
    if !ok {
        return write_response(&mut stream, &error(401, "missing or invalid API token"));
    }

    let response = route(&request, daemon);
    write_response(&mut stream, &response)
}

fn route(request: &http::ServerRequest, daemon: &Arc<Daemon>) -> Response {
    let method = request.method.as_str();
    let path = request.path();
    match (method, path) {
        ("GET", "/v1/status") => Response::new(200, "application/json", status_json(daemon)),
        ("GET", "/v1/torrents") => {
            let body = lock(&daemon.registry).list().encode().into_bytes();
            Response::new(200, "application/json", body)
        }
        ("POST", "/v1/torrents") => add_torrent(request, daemon),
        (_, p) if p.starts_with("/v1/torrents/") => {
            torrent_action(method, request, daemon, &p["/v1/torrents/".len()..])
        }
        ("GET", _) => error(404, "no such resource"),
        _ => error(405, "method not allowed"),
    }
}

fn add_torrent(request: &http::ServerRequest, daemon: &Arc<Daemon>) -> Response {
    // `?paused` and `?sequential` are what an operator wanted *of this add*,
    // rather than a second command issued after the torrent already started
    // doing the other thing.
    let query = request.query().unwrap_or("");
    let options = registry::AddOptions {
        paused: query_flag(query, "paused"),
        sequential: query_flag(query, "sequential"),
    };
    match add_from_body(daemon, &request.body, options) {
        Ok(Added::Magnet(info_hash)) => {
            let body = Value::Object(vec![
                (
                    "info_hash".to_owned(),
                    Value::from(registry::hex(&info_hash)),
                ),
                ("state".to_owned(), Value::from("fetching-metadata")),
            ])
            .encode()
            .into_bytes();
            Response::new(201, "application/json", body)
        }
        Ok(Added::Torrent(info_hash)) => {
            let body = Value::Object(vec![(
                "info_hash".to_owned(),
                Value::from(registry::hex(&info_hash)),
            )])
            .encode()
            .into_bytes();
            Response::new(201, "application/json", body)
        }
        Err(e @ (AddError::Parse(_) | AddError::Magnet(_))) => error(400, &e.to_string()),
        Err(AddError::Duplicate) => error(409, "torrent already added"),
        Err(AddError::Io(e)) => error(500, &format!("adding torrent: {e}")),
    }
}

/// Drop everything initialisation needed and the running daemon does not
/// (SCOPE §5 Layer 2), returning what the sandbox managed to apply.
///
/// Must be called before any thread is spawned: a Landlock domain covers the
/// calling thread and its descendants, not siblings that already exist.
fn restrict_self(config: &Config) -> String {
    let mut read_write: Vec<&Path> = vec![&config.data_dir];
    if let Some(parent) = config.api_socket.parent() {
        read_write.push(parent);
    }
    // The watch directory, if there is one, is the one path an operator names
    // that is deliberately *outside* the data directory — and the daemon both
    // reads it and renames within it. Granted here, before the domain closes,
    // which is the whole discipline this layer runs on: what is not unveiled
    // before the restriction cannot be reached after it.
    //
    // Getting this wrong is silent in the worst way. Landlock is best-effort,
    // so on a kernel without it the watcher works and on a kernel with it the
    // directory simply reads as empty — nothing added, nothing logged, no
    // error anywhere. CI found it; a local run had not.
    if let Some(dir) = &config.watch_dir {
        read_write.push(dir);
    }
    // Note what is *not* here: the API listeners. Both are already bound by
    // this point (`bind_listeners`), and a bound listener keeps accepting
    // through a Landlock domain and a seccomp filter that would refuse to
    // create it — which is the whole shape of this layer, and the reason the
    // TCP one has to be acquired before the door shuts rather than lazily on
    // first use.
    sandbox::enter_post_init(&sandbox::Limits {
        read_write: &read_write,
        read_only: &[Path::new("/dev/urandom")],
        connect_tcp: sam_tcp_port(&config.sam_address),
    })
}

/// Bind the API listeners: the unix socket, and the loopback TCP one when
/// `api_listen` asked for it.
///
/// Both happen here, before the sandbox closes, because that is the rule
/// Layer 2 runs on: what is not acquired before self-restriction cannot be
/// acquired after it.
///
/// The unix socket is not optional and does not become so — `clove` resolves it
/// from the config and would have nothing to talk to — so the second listener
/// adds a way in rather than replacing one. It exists because a Transmission
/// client cannot speak to a unix socket, which is most of the point of having
/// one.
fn bind_listeners(config: &Config) -> Result<(ApiListener, Option<ApiListener>), String> {
    let unix = ApiListener::bind_unix(&config.api_socket)
        .map_err(|e| format!("binding {}: {e}", config.api_socket.display()))?;
    eprintln!("cloved: listening on {}", config.api_socket.display());

    let Some(addr) = &config.api_listen else {
        return Ok((unix, None));
    };
    let tcp = ApiListener::bind_loopback_tcp(addr).map_err(|e| format!("binding {addr}: {e}"))?;
    eprintln!("cloved: also listening on {addr}");
    Ok((unix, Some(tcp)))
}

/// Build the Transmission surface if the operator asked for one.
///
/// Done here rather than lazily on first request because generating the session
/// id can fail: a machine with no random source should stop the start, where a
/// lazy one would surface it hours later as an authentication failure nobody
/// could explain.
fn build_rpc(config: &Config) -> Result<Option<transmission::Rpc>, String> {
    if !config.transmission_rpc {
        return Ok(None);
    }
    let downloads = config.data_dir.join("downloads");
    let rpc = transmission::Rpc::new(&downloads).map_err(|e| e.to_string())?;
    eprintln!(
        "cloved: Transmission RPC at {} (username ignored, password is the API token)",
        transmission::PATH
    );
    Ok(Some(rpc))
}

/// What an add turned out to be.
pub(crate) enum Added {
    /// A `.torrent`: hosted, scanned, and started if the queue has room.
    Torrent([u8; 20]),
    /// A magnet: registered, with a metadata-fetch thread now running for it.
    Magnet([u8; 20]),
}

/// Add a `.torrent` or a `magnet:` URI, doing whichever kind's follow-up work
/// the daemon owes it.
///
/// Shared by `/v1/` and the Transmission surface so that "add" means exactly
/// one thing however it was asked for — including the two pieces of follow-up
/// that are easy to forget and silently break a torrent: a magnet needs its
/// fetch thread spawned, and a `.torrent` needs its initial disk scan run with
/// the registry *unlocked*.
///
/// # Errors
///
/// [`AddError`] as the registry reports it.
pub(crate) fn add_from_body(
    daemon: &Arc<Daemon>,
    body: &[u8],
    options: registry::AddOptions,
) -> Result<Added, AddError> {
    if body.starts_with(b"magnet:") {
        let uri = String::from_utf8_lossy(body).into_owned();
        // Bind first: a `match lock(..)` would hold the registry guard across
        // the arms, and spawn_metadata_fetch re-locks it (deadlock).
        let added = lock(&daemon.registry).add_magnet(uri.trim());
        let info_hash = added?;
        spawn_metadata_fetch(daemon, info_hash);
        return Ok(Added::Magnet(info_hash));
    }
    let (info_hash, job) = lock(&daemon.registry).add_torrent(body, options)?;
    // The initial pass over whatever is already on disk runs with the registry
    // unlocked: on a re-add over a finished download it hashes the whole
    // torrent, and every other request would otherwise wait for it. The torrent
    // is registered and marked "verifying" already, so it shows up in
    // `clove list` while this happens.
    //
    // The add already succeeded; a scan that fails only means the torrent shows
    // nothing on disk, which `clove verify` can retry.
    let _ = run_scan(daemon, &job);
    Ok(Added::Torrent(info_hash))
}

/// Run a scan with the registry unlocked, then publish it.
///
/// Reporting back is not optional: a torrent whose scan never finishes stays
/// marked as scanning and never starts, so the result goes in whether the pass
/// succeeded or failed.
fn run_scan(daemon: &Daemon, job: &registry::ScanJob) -> Result<u32, ActionError> {
    let scanned = job.run();
    lock(&daemon.registry).finish_scan(job, scanned)
}

/// Route a request against a specific torrent: `<info-hash>` or
/// `<info-hash>/<action>`.
fn torrent_action(
    method: &str,
    request: &http::ServerRequest,
    daemon: &Daemon,
    rest: &str,
) -> Response {
    let (reference, action) = match rest.split_once('/') {
        Some((reference, action)) => (reference, Some(action)),
        None => (rest, None),
    };
    let info_hash = match lock(&daemon.registry).resolve(reference) {
        Ok(info_hash) => info_hash,
        Err(e) => return resolve_error(&e),
    };

    // A magnet whose metadata has not arrived is listed and can be removed,
    // but it has no engine, no files and no trackers of its own to act on.
    // Every such operation used to answer "no such torrent", which is false
    // about something `clove list` is showing at that moment — and sends the
    // operator looking for a torrent they can see.
    if lock(&daemon.registry).is_pending(&info_hash) && !(method == "DELETE" && action.is_none()) {
        return error(
            400,
            "this magnet is still fetching its metadata; until it arrives there is \
             nothing to act on but removing it",
        );
    }

    match (method, action) {
        ("GET", None) => match lock(&daemon.registry).detail(&info_hash) {
            Some(value) => Response::new(200, "application/json", value.encode().into_bytes()),
            None => error(404, "no such torrent"),
        },
        ("DELETE", None) => {
            let delete_data = request.query().is_some_and(query_has_data);
            match lock(&daemon.registry).remove(&info_hash, delete_data) {
                Ok(()) => ok_json(),
                Err(RemoveError::NotFound) => error(404, "no such torrent"),
                Err(RemoveError::Io(e)) => error(500, &format!("removing torrent: {e}")),
            }
        }
        ("POST", Some("pause")) => {
            action_result(lock(&daemon.registry).set_paused(&info_hash, true))
        }
        ("POST", Some("resume")) => {
            action_result(lock(&daemon.registry).set_paused(&info_hash, false))
        }
        ("POST", Some("start")) => action_result(lock(&daemon.registry).force_start(&info_hash)),
        ("PUT", Some("seed-ratio")) => match parse_ratio_body(&request.body) {
            Some(milli) => action_result(lock(&daemon.registry).set_seed_ratio(&info_hash, milli)),
            None => error(
                400,
                "body must be a ratio like 2 or 1.75, or 0 to follow the daemon's seed_ratio",
            ),
        },
        ("POST", Some("peers")) => {
            let text = String::from_utf8_lossy(&request.body);
            let Some(peer) = DestHash::from_b32(&text) else {
                return error(
                    400,
                    "body must be a peer's b32 address (52 chars, .b32.i2p optional)",
                );
            };
            action_result(lock(&daemon.registry).add_peer(&info_hash, peer))
        }
        ("POST", Some("announce")) => {
            action_result(lock(&daemon.registry).announce_now(&info_hash))
        }
        ("PUT", Some("sequential")) => match parse_bool_body(&request.body) {
            Some(on) => action_result(lock(&daemon.registry).set_sequential(&info_hash, on)),
            None => error(400, "body must be \"true\" or \"false\""),
        },
        ("POST", Some("verify")) => {
            let job = match lock(&daemon.registry).begin_verify(&info_hash) {
                Ok(job) => job,
                Err(e) => return action_error(&e),
            };
            // Hashing happens here, with nothing locked. This request waits for
            // it — the operator asked — but nothing else does.
            match run_scan(daemon, &job) {
                Ok(verified) => {
                    let body = Value::Object(vec![(
                        "verified".to_owned(),
                        Value::UInt(u64::from(verified)),
                    )])
                    .encode()
                    .into_bytes();
                    Response::new(200, "application/json", body)
                }
                Err(e) => action_error(&e),
            }
        }
        ("PUT", Some("priorities")) => match parse_priorities(&request.body) {
            Some(priorities) => action_result(
                lock(&daemon.registry)
                    .set_priorities(&info_hash, priorities)
                    .map(|_| ()),
            ),
            None => error(
                400,
                "priorities body must be comma-separated values of 0, 1, or 2",
            ),
        },
        _ => error(405, "method not allowed"),
    }
}

fn ok_json() -> Response {
    Response::new(200, "application/json", b"{\"ok\":true}".to_vec())
}

fn action_result(result: Result<(), ActionError>) -> Response {
    match result {
        Ok(()) => ok_json(),
        Err(e) => action_error(&e),
    }
}

/// Turn a failed torrent-reference resolution into a response.
///
/// The ambiguous case carries the candidates rather than a bare refusal: an
/// operator who typed too short a prefix wants to see which torrents they hit,
/// and re-running `clove list` to find out is a step the daemon can save them.
fn resolve_error(e: &registry::ResolveError) -> Response {
    match e {
        registry::ResolveError::Malformed => error(
            400,
            &format!(
                "torrent reference must be a full 40-character lowercase-hex info-hash, \
                 or a prefix of at least {} of its characters",
                registry::MIN_PREFIX
            ),
        ),
        registry::ResolveError::NotFound => error(404, "no such torrent"),
        registry::ResolveError::Ambiguous(candidates) => {
            let listed: Vec<Value> = candidates
                .iter()
                .map(|c| {
                    Value::Object(vec![
                        ("info_hash".to_owned(), Value::from(c.info_hash.clone())),
                        ("name".to_owned(), Value::from(c.name.clone())),
                    ])
                })
                .collect();
            let body = Value::Object(vec![
                (
                    "error".to_owned(),
                    Value::from(format!(
                        "that prefix matches {} torrents; use more characters",
                        candidates.len()
                    )),
                ),
                ("candidates".to_owned(), Value::Array(listed)),
            ])
            .encode()
            .into_bytes();
            Response::new(409, "application/json", body)
        }
    }
}

fn action_error(e: &ActionError) -> Response {
    match e {
        ActionError::NotFound => error(404, "no such torrent"),
        ActionError::BadInput(what) => error(400, what),
        ActionError::Io(io) => error(500, &io.to_string()),
    }
}

/// Parse a comma-separated priorities body (`1,0,2`) into per-file bytes.
fn parse_priorities(body: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    let mut out = Vec::new();
    for part in text.trim().split(',') {
        let value: u8 = part.trim().parse().ok()?;
        if value > 2 {
            return None;
        }
        out.push(value);
    }
    Some(out)
}

/// Parse a seed-ratio body (`2`, `1.75`, `0`) into thousandths.
///
/// The same grammar `clove.conf` accepts, so a ratio means one thing whether
/// it is typed into a file or handed to the API.
fn parse_ratio_body(body: &[u8]) -> Option<u64> {
    clove_core::config::parse_seed_ratio(std::str::from_utf8(body).ok()?.trim())
}

/// Parse a boolean request body. Deliberately strict — only `true` and
/// `false`, since a flag that silently reads a typo as "off" is worse than
/// one that refuses it.
fn parse_bool_body(body: &[u8]) -> Option<bool> {
    match std::str::from_utf8(body).ok()?.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Whether a query string carries `name` as a truthy flag — bare, or set to
/// `1`, `true` or `yes`.
fn query_flag(query: &str, name: &str) -> bool {
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, "1"));
        key == name && matches!(value, "1" | "true" | "yes")
    })
}

/// Whether a query string carries a truthy `data` flag (`data`, `data=1`,
/// `data=true`, `data=yes`).
fn query_has_data(query: &str) -> bool {
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, "1"));
        key == "data" && matches!(value, "1" | "true" | "yes")
    })
}

fn status_json(daemon: &Daemon) -> Vec<u8> {
    // One lock, one refresh: the count and the rates are then the same
    // reading, rather than two taken a moment apart that do not add up.
    let (count, totals) = {
        let mut registry = lock(&daemon.registry);
        let count = registry.count();
        (count, registry.totals())
    };
    Value::Object(vec![
        ("version".to_owned(), Value::from(env!("CARGO_PKG_VERSION"))),
        (
            "uptime_secs".to_owned(),
            Value::UInt(daemon.start.elapsed().as_secs()),
        ),
        (
            "sam_address".to_owned(),
            Value::from(daemon.sam_address.clone()),
        ),
        (
            "torrents".to_owned(),
            Value::UInt(u64::try_from(count).unwrap_or(u64::MAX)),
        ),
        ("up_rate".to_owned(), Value::UInt(totals.up_rate)),
        ("down_rate".to_owned(), Value::UInt(totals.down_rate)),
        (
            "peers".to_owned(),
            Value::UInt(u64::try_from(totals.peers).unwrap_or(u64::MAX)),
        ),
        (
            "peer_limit".to_owned(),
            Value::UInt(u64::try_from(totals.peer_limit).unwrap_or(u64::MAX)),
        ),
        (
            "router".to_owned(),
            Value::from(lock(&daemon.router).clone()),
        ),
    ])
    .encode()
    .into_bytes()
}

/// A JSON error body with the given status.
fn error(status: u16, message: &str) -> Response {
    let body = Value::Object(vec![("error".to_owned(), Value::from(message))])
        .encode()
        .into_bytes();
    Response::new(status, "application/json", body)
}

fn write_response(stream: &mut ApiStream, response: &Response) -> std::io::Result<()> {
    stream.write_all(&response.encode())?;
    stream.flush()
}

/// Length of the API token as stored: 32 random bytes, hex.
const TOKEN_HEX_LEN: usize = 64;

/// Whether `token` has the shape this daemon writes — exactly
/// [`TOKEN_HEX_LEN`] hex characters.
///
/// Anything else is not a secret we are willing to compare against, the empty
/// string above all: it would match an empty `x-clove-token:` header.
fn is_well_formed_token(token: &str) -> bool {
    token.len() == TOKEN_HEX_LEN && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read the API token from `<data_dir>/token`, creating it (32 random bytes,
/// hex, `0600`) on first run.
///
/// A file that is present but not a well-formed token is replaced, not trusted.
/// That case is reachable: the token used to be the one file written in place
/// rather than temp-and-rename, so a crash, a `SIGKILL`, or a full disk between
/// creating and filling it left a zero-byte file behind — and a zero-byte token
/// authenticates every local caller. Nothing can hold the old value either,
/// because it was never a complete token, so replacing it costs nothing.
fn load_or_create_token(data_dir: &Path) -> std::io::Result<String> {
    let path = data_dir.join("token");
    match read_private_file(&path) {
        Ok(Some(existing)) if is_well_formed_token(existing.trim()) => {
            Ok(existing.trim().to_owned())
        }
        Ok(Some(_)) => {
            eprintln!(
                "cloved: {} is not a well-formed API token (empty or truncated?); \
                 generating a new one",
                path.display()
            );
            write_new_token(&path)
        }
        Ok(None) => write_new_token(&path),
        // Refused for being unsafe rather than malformed: replacing it silently
        // would be the wrong move, because a token that has been world-readable
        // must be assumed known and the operator needs to hear that they had
        // one. Fatal, so the daemon does not come up serving an API whose token
        // somebody else may hold.
        Err(e) => Err(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        )),
    }
}

/// Generate a token and put it at `path` atomically: a `0600` temp file,
/// written, fsynced, then renamed over the target. Rename keeps the mode, so
/// the token is never briefly readable by anyone else and never half-written.
fn write_new_token(path: &Path) -> std::io::Result<String> {
    let mut raw = [0u8; 32];
    getrandom::getrandom(&mut raw).map_err(|e| std::io::Error::other(format!("getrandom: {e}")))?;
    let token = registry::hex(&raw);
    write_private_file(path, token.as_bytes())?;
    Ok(token)
}

/// Read a secret file, refusing one that is not safe to treat as a secret.
///
/// `Ok(None)` means it is not there, which is how both callers say "first run".
///
/// New files are created `0600` by [`write_private_file`], so a clean first run
/// is private and this checks nothing that was not already true. What it catches
/// is a file that arrived some *other* way — restored from a backup that did not
/// preserve modes, copied by hand, unpacked from an archive, or written by a
/// migration — and reading such a file back without looking means the API token
/// or the client's stable identity can sit at `0644` for anyone on the machine
/// to take. `SECURITY.md` names both disclosures in scope.
///
/// The checks are: opened with `O_NOFOLLOW`, so a symlink here is refused rather
/// than followed somewhere unexpected; and then `fstat` on the descriptor we
/// actually hold — not a second look at the path, which could by then be a
/// different file — requiring a regular file, owned by this process's effective
/// user, with no group or other permission bits.
///
/// Refuses rather than repairs. A `0644` token has been readable for as long as
/// it has existed and must be assumed known; quietly `chmod`-ing it to `0600`
/// would hide that while changing nothing about who has already read it. The
/// operator gets told, and can delete it to have a fresh one generated.
///
/// # Errors
///
/// A symlink, a non-regular file, the wrong owner, group/other permission bits,
/// or any read error other than the file being absent.
fn read_private_file(path: &Path) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let refused = |what: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, what);

    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // `O_NOFOLLOW` on a symlink reports ELOOP, which has no stable
        // `ErrorKind`; say what it means rather than passing the number on.
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
            return Err(refused("it is a symbolic link".to_owned()));
        }
        Err(e) => return Err(e),
    };

    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(refused("it is not a regular file".to_owned()));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(refused(format!(
            "it is owned by uid {}, not by this daemon",
            meta.uid()
        )));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(refused(format!(
            "its mode is {mode:04o}; a secret must not be readable by anyone else \
             (delete it and a new one will be generated)"
        )));
    }

    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

/// Write `contents` to `path` atomically and privately: a `0600` temp file,
/// written, fsynced, then renamed over the target.
///
/// Rename keeps the mode, so the file is never briefly readable by anyone else
/// and never half-written. Shared by the API token and the destination key —
/// the two files in the data directory that are secrets, and the two that a
/// crash mid-write must not leave truncated.
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let name = path.file_name().map_or_else(
        || std::ffi::OsString::from("clove"),
        std::ffi::OsStr::to_os_string,
    );
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    // A temp left by an earlier crash of this pid would fail create_new below.
    let _ = std::fs::remove_file(&tmp);
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Length-independent byte comparison, so token checks don't leak length or a
/// prefix match through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    //! Adversarial tests for the control API — the daemon's only externally
    //! reachable surface, and the one place a bug is a security bug rather
    //! than a correctness one.
    //!
    //! `ci/smoke.sh` drives this API through the real CLI, which by
    //! construction always sends a well-formed request with the right token.
    //! Everything an attacker would actually send — no token, a wrong token,
    //! a lying `Content-Length`, a traversal in the path — is only reachable
    //! from here.
    //!
    //! [`handle`] is exercised over a real socketpair rather than by calling
    //! [`route`] directly, so request parsing, authentication, routing and
    //! response writing are all in the path, in that order. A test that
    //! bypassed authentication to reach the router would not notice if the
    //! two were ever swapped.

    use super::*;
    use std::fmt::Write as _;
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A token of the shape the daemon really writes: 32 random bytes as
    /// hex. The length matters — the auth path refuses to compare against
    /// anything that is not a well-formed token.
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static C: AtomicU32 = AtomicU32::new(0);
            let n = C.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("clove-api-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).expect("temp dir");
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A configuration with everything at its default, for tests that care
    /// about one key and should not have to name the rest.
    fn test_config(dir: &TempDir, transmission_rpc: bool) -> Config {
        Config {
            sam_address: "127.0.0.1:7656".to_owned(),
            i_know_sam_is_remote: false,
            data_dir: dir.0.clone(),
            api_socket: dir.0.join("clove.sock"),
            ephemeral: false,
            preallocate: false,
            peer_limit: clove_core::config::DEFAULT_PEER_LIMIT,
            torrent_peer_limit: clove_core::config::DEFAULT_TORRENT_PEER_LIMIT,
            max_active_downloads: clove_core::config::DEFAULT_MAX_ACTIVE_DOWNLOADS,
            max_active_seeds: clove_core::config::DEFAULT_MAX_ACTIVE_SEEDS,
            seed_ratio_milli: 0,
            seed_idle_minutes: 0,
            watch_dir: None,
            api_listen: None,
            transmission_rpc,
        }
    }

    fn daemon(dir: &TempDir) -> Arc<Daemon> {
        daemon_with(dir, false)
    }

    fn daemon_with(dir: &TempDir, transmission_rpc: bool) -> Arc<Daemon> {
        let config = test_config(dir, transmission_rpc);
        Arc::new(Daemon {
            start: Instant::now(),
            sam_address: config.sam_address.clone(),
            token: TOKEN.to_owned(),
            peer_id: *b"-CV0001-testtesttes\0",
            registry: Mutex::new(
                Registry::open(&dir.0, registry::Limits::default()).expect("registry"),
            ),
            rpc: transmission_rpc
                .then(|| transmission::Rpc::new(&dir.0.join("downloads")).expect("rpc")),
            config,
            router: Mutex::new("connecting"),
        })
    }

    /// Send `raw` to the daemon over a socketpair and return what it wrote
    /// back. Half-closing the write side matters: a request whose declared
    /// body never arrives must terminate on EOF rather than block a
    /// connection thread forever.
    fn speak(daemon: &Arc<Daemon>, raw: &[u8]) -> String {
        let (client, server) = UnixStream::pair().expect("socketpair");
        let mut client_w = client.try_clone().expect("clone");
        // The write runs on its own thread: a request larger than the socket
        // buffer would otherwise block here, before the daemon has been given
        // a chance to read a byte of it. That is exactly the shape of the
        // oversized-body case below.
        let body = raw.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = client_w.write_all(&body);
            let _ = client_w.shutdown(std::net::Shutdown::Write);
        });
        handle(ApiStream::Unix(server), daemon).expect("handle");
        let mut reply = Vec::new();
        let mut client_r = client;
        client_r.read_to_end(&mut reply).expect("read response");
        let _ = writer.join();
        String::from_utf8_lossy(&reply).into_owned()
    }

    fn status_of(reply: &str) -> u16 {
        reply
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {reply:?}"))
    }

    fn get(path: &str, token: Option<&str>) -> Vec<u8> {
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
        if let Some(t) = token {
            let _ = write!(req, "x-clove-token: {t}\r\n");
        }
        req.push_str("\r\n");
        req.into_bytes()
    }

    // ------------------------------------------------------------- auth

    #[test]
    fn every_request_needs_the_token() {
        let dir = TempDir::new("auth");
        let d = daemon(&dir);

        // No token at all.
        assert_eq!(status_of(&speak(&d, &get("/v1/status", None))), 401);
        // Wrong token of the same length — the case constant_time_eq exists
        // for.
        let same_len = "f".repeat(TOKEN.len());
        assert_eq!(
            status_of(&speak(&d, &get("/v1/status", Some(&same_len)))),
            401
        );
        // Wrong token of a different length.
        assert_eq!(status_of(&speak(&d, &get("/v1/status", Some("x")))), 401);
        // A prefix of the real token must not pass.
        assert_eq!(
            status_of(&speak(&d, &get("/v1/status", Some(&TOKEN[..16])))),
            401
        );
        // The real one does.
        assert_eq!(status_of(&speak(&d, &get("/v1/status", Some(TOKEN)))), 200);
    }

    #[test]
    fn authentication_precedes_routing() {
        let dir = TempDir::new("order");
        let d = daemon(&dir);
        // An unauthenticated request to a path that does not exist must be
        // refused for the token, not answered with a 404: otherwise the API
        // tells an unauthenticated caller which paths are real.
        assert_eq!(status_of(&speak(&d, &get("/v1/nope", None))), 401);
        assert_eq!(status_of(&speak(&d, &get("/v1/status", None))), 401);
    }

    // ---------------------------------------------------------- routing

    #[test]
    fn unknown_paths_and_methods_are_refused_cleanly() {
        let dir = TempDir::new("routes");
        let d = daemon(&dir);
        assert_eq!(status_of(&speak(&d, &get("/", Some(TOKEN)))), 404);
        assert_eq!(status_of(&speak(&d, &get("/v2/status", Some(TOKEN)))), 404);
        let delete_status =
            format!("DELETE /v1/status HTTP/1.1\r\nx-clove-token: {TOKEN}\r\n\r\n").into_bytes();
        assert_eq!(status_of(&speak(&d, &delete_status)), 405);
    }

    #[test]
    fn a_traversal_in_the_info_hash_is_not_a_path() {
        let dir = TempDir::new("traversal");
        let d = daemon(&dir);
        // The info-hash segment is parsed as 40 hex characters, never joined
        // onto the state directory. These must all be "no such torrent", and
        // must leave the data directory untouched.
        for evil in [
            "/v1/torrents/../../../etc/passwd",
            "/v1/torrents/..%2f..%2fetc%2fpasswd",
            "/v1/torrents/....//....//etc/passwd",
            "/v1/torrents/%00",
            "/v1/torrents/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/../../verify",
        ] {
            let code = status_of(&speak(&d, &get(evil, Some(TOKEN))));
            assert!(
                code == 404 || code == 400 || code == 405,
                "{evil} answered {code}"
            );
        }
        let leaked: Vec<_> = std::fs::read_dir(dir.0.join("state"))
            .expect("state dir")
            .filter_map(Result::ok)
            .collect();
        assert!(leaked.is_empty(), "a traversal created state files");
    }

    #[test]
    fn a_non_hex_info_hash_is_rejected_not_guessed() {
        let dir = TempDir::new("hex");
        let d = daemon(&dir);
        // Malformed is 400 and absent is 404; the two are different answers
        // on purpose, so a typo in a script reads as a typo.
        for bad in [
            "/v1/torrents/zzz",                                        // under MIN_PREFIX
            "/v1/torrents/zzzz",                                       // not hex
            "/v1/torrents/0123456789abcdef0123456789abcdef0123456789", // 42 chars
            "/v1/torrents/0123456789ABCDEF0123456789ABCDEF01234567",   // uppercase
            "/v1/torrents/0123456789abcdef0123456789abcdef0123456 ",   // trailing space
        ] {
            assert_eq!(status_of(&speak(&d, &get(bad, Some(TOKEN)))), 400, "{bad}");
        }
        // Absent is 404 whether it was named in full or by a prefix. The
        // 38-character case used to be here as a malformed hash and is now a
        // well-formed prefix that happens to match nothing — the same answer a
        // full hash gets, which is the point of accepting prefixes at all.
        for absent in [
            "/v1/torrents/0123456789abcdef0123456789abcdef01234567", // full, unknown
            "/v1/torrents/0123456789abcdef0123456789abcdef012345",   // 38-char prefix
            "/v1/torrents/0123",                                     // shortest prefix
        ] {
            assert_eq!(
                status_of(&speak(&d, &get(absent, Some(TOKEN)))),
                404,
                "{absent}"
            );
        }
    }

    // ----------------------------------------------------- hostile HTTP

    #[test]
    fn malformed_requests_get_400_not_a_panic() {
        let dir = TempDir::new("malformed");
        let d = daemon(&dir);
        let cases: Vec<Vec<u8>> = vec![
            b"\r\n\r\n".to_vec(),
            b"GET\r\n\r\n".to_vec(),
            b"GET /v1/status\r\n\r\n".to_vec(),
            b"GET /v1/status HTTP/9.9\r\n\r\n".to_vec(),
            b"GET /v1/status FTP/1.1\r\n\r\n".to_vec(),
            b"\x00\x01\x02\x03\r\n\r\n".to_vec(),
            // A header line with no colon.
            b"GET /v1/status HTTP/1.1\r\nnonsense\r\n\r\n".to_vec(),
            // Non-UTF-8 in the head.
            b"GET /v1/\xff\xfe HTTP/1.1\r\n\r\n".to_vec(),
            // No terminator at all, then EOF.
            b"GET /v1/status HTTP/1.1\r\n".to_vec(),
        ];
        for raw in cases {
            let code = status_of(&speak(&d, &raw));
            assert!(
                code == 400 || code == 401,
                "{:?} answered {code}",
                String::from_utf8_lossy(&raw)
            );
        }
    }

    #[test]
    fn an_oversized_body_is_refused_on_the_declared_length() {
        let dir = TempDir::new("oversize");
        let d = daemon(&dir);
        // Declares a gigabyte and sends none of it. The refusal comes from
        // the Content-Length header alone — if the daemon buffered first this
        // would allocate a gigabyte and then block waiting for bytes that
        // never come.
        //
        // Sending the body too is not tested here, and cannot be: refusing
        // without reading means the unread bytes sit in the socket, and
        // closing on top of them resets the connection before the client can
        // read the 400. That is the correct trade — draining an attacker's
        // body is the denial of service the limit exists to prevent — but it
        // does mean an oversized *send* is answered with a reset rather than
        // a message. Clients should check the length before sending.
        let raw = format!(
            "POST /v1/torrents HTTP/1.1\r\nx-clove-token: {TOKEN}\r\nContent-Length: 1073741824\r\n\r\n"
        );
        assert_eq!(status_of(&speak(&d, raw.as_bytes())), 400);
    }

    #[test]
    fn a_body_at_exactly_the_limit_is_read_and_judged_on_its_merits() {
        let dir = TempDir::new("limit");
        let d = daemon(&dir);
        // The boundary is inclusive: MAX_REQUEST_BODY bytes are accepted by
        // the transport and then rejected by the torrent parser. Both answers
        // are 400, so the distinction that matters is the message — a size
        // refusal here would mean the largest legal .torrent cannot be added.
        let mut raw = format!(
            "POST /v1/torrents HTTP/1.1\r\nx-clove-token: {TOKEN}\r\nContent-Length: {MAX_REQUEST_BODY}\r\n\r\n"
        )
        .into_bytes();
        raw.extend(std::iter::repeat_n(b'a', MAX_REQUEST_BODY));
        let reply = speak(&d, &raw);
        assert_eq!(status_of(&reply), 400);
        assert!(
            !reply.contains("exceeds the allowed size"),
            "a body at the limit was refused for its size: {reply}"
        );
    }

    #[test]
    fn a_body_shorter_than_declared_ends_at_eof() {
        let dir = TempDir::new("short-body");
        let d = daemon(&dir);
        // Promises 4 KiB, sends 3 bytes, then closes. This must terminate.
        let raw = format!(
            "POST /v1/torrents HTTP/1.1\r\nx-clove-token: {TOKEN}\r\nContent-Length: 4096\r\n\r\nabc"
        );
        let code = status_of(&speak(&d, raw.as_bytes()));
        assert!(code == 400, "truncated body answered {code}");
    }

    #[test]
    fn garbage_bodies_do_not_become_torrents() {
        let dir = TempDir::new("garbage-add");
        let d = daemon(&dir);
        for body in [
            b"not a torrent".as_slice(),
            b"d".as_slice(),
            b"magnet:?xt=urn:btih:nothex".as_slice(),
            b"magnet:".as_slice(),
            &[0xFF; 64],
        ] {
            let mut raw = format!(
                "POST /v1/torrents HTTP/1.1\r\nx-clove-token: {TOKEN}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            raw.extend_from_slice(body);
            assert_eq!(status_of(&speak(&d, &raw)), 400);
        }
        assert_eq!(lock(&d.registry).count(), 0, "garbage was accepted");
    }

    // ------------------------------------------------------- pure parts

    #[test]
    fn constant_time_eq_is_an_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
        // Differing in the first byte and in the last must both fail: an
        // early return would make the first case faster and leak.
        assert!(!constant_time_eq(b"xbc", b"abc"));
        assert!(!constant_time_eq(b"abx", b"abc"));
    }

    #[test]
    fn argument_parsing() {
        let args = |v: &[&str]| parse_args_from(v.iter().map(|s| (*s).to_owned()));
        assert!(args(&[]).expect("empty").config_path.is_none());
        assert!(args(&["-C"]).expect("-C").check);
        assert!(args(&["--check"]).expect("--check").check);
        assert_eq!(
            args(&["-c", "/etc/clove.conf"])
                .expect("-c")
                .config_path
                .as_deref(),
            Some(Path::new("/etc/clove.conf"))
        );
        // A flag that eats the next argument must complain when there is none.
        assert!(args(&["-c"]).is_err());
        assert!(args(&["--config"]).is_err());
        // Unknown arguments are fatal, never ignored (the config file's rule).
        assert!(args(&["--nope"]).is_err());
        assert!(args(&["-C", "--nope"]).is_err());
        // A path that looks like a flag is still a path: the parser takes the
        // next argument verbatim.
        assert_eq!(
            args(&["-c", "-C"]).expect("-c -C").config_path.as_deref(),
            Some(Path::new("-C"))
        );
    }

    #[test]
    fn priority_bodies() {
        assert_eq!(parse_priorities(b"1,0,2"), Some(vec![1, 0, 2]));
        assert_eq!(parse_priorities(b" 1 , 0 "), Some(vec![1, 0]));
        assert_eq!(parse_priorities(b"1"), Some(vec![1]));
        assert_eq!(parse_priorities(b"3"), None);
        assert_eq!(parse_priorities(b"-1"), None);
        assert_eq!(parse_priorities(b"1,,2"), None);
        assert_eq!(parse_priorities(b""), None);
        assert_eq!(parse_priorities(b"1,2,x"), None);
        assert_eq!(parse_priorities(&[0xFF, 0xFE]), None);
        // 256 wraps to 0 in a u8 parse only if the parser is careless.
        assert_eq!(parse_priorities(b"256"), None);
    }

    /// The API must stay answerable while a torrent is being hashed. The whole
    /// point of handing the scan out of the lock is that `verify` on a large
    /// torrent — minutes of SHA-1 — no longer stops `status`, `list`, `pause` and
    /// the periodic resume writer dead.
    #[test]
    fn the_api_answers_while_a_torrent_is_being_scanned() {
        use clove_core::bencode::{self, Value as Ben};
        use sha1::{Digest, Sha1};
        use std::collections::BTreeMap;

        let dir = TempDir::new("scan-lock");
        let d = daemon(&dir);

        // A torrent big enough that hashing it is not instant.
        let content: Vec<u8> = (0..(64 * 16 * 1024u32))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let pieces: Vec<u8> = content
            .chunks(16 * 1024)
            .flat_map(|c| <[u8; 20]>::from(Sha1::digest(c)))
            .collect();
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), Ben::Bytes(b"scan-lock".to_vec()));
        info.insert(b"piece length".to_vec(), Ben::Int(16 * 1024));
        info.insert(b"pieces".to_vec(), Ben::Bytes(pieces));
        info.insert(
            b"length".to_vec(),
            Ben::Int(i64::try_from(content.len()).expect("fits")),
        );
        let mut root = BTreeMap::new();
        root.insert(b"info".to_vec(), Ben::Dict(info));
        let bytes = bencode::encode(&Ben::Dict(root));

        // Add it, and write the data so the scan has something to hash.
        let (info_hash, job) = lock(&d.registry)
            .add_torrent(&bytes, registry::AddOptions::default())
            .expect("add");
        let downloads = dir.0.join("downloads/scan-lock");
        std::fs::write(&downloads, &content).expect("write the data");
        let published = run_scan(&d, &job).expect("first scan");
        assert_eq!(published, 64, "the data on disk should have verified");

        // Now a second pass, with the registry watched from another thread.
        let job = lock(&d.registry)
            .begin_verify(&info_hash)
            .expect("begin verify");
        let watcher = {
            let d = Arc::clone(&d);
            std::thread::spawn(move || {
                // If the scan held the lock, this would block until it finished.
                let mut answered = 0;
                for _ in 0..20 {
                    let _ = lock(&d.registry).count();
                    answered += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                answered
            })
        };
        let scanned = job.run();
        assert_eq!(watcher.join().expect("watcher"), 20);
        let verified = lock(&d.registry)
            .finish_scan(&job, scanned)
            .expect("finish");
        assert_eq!(verified, 64);

        // And the state string says what is going on while it runs.
        let job = lock(&d.registry).begin_verify(&info_hash).expect("begin");
        let listing = lock(&d.registry).list().encode();
        assert!(
            listing.contains("verifying"),
            "a torrent being scanned should say so: {listing}"
        );
        // A second verify while one is running is refused rather than queued.
        assert!(matches!(
            lock(&d.registry).begin_verify(&info_hash),
            Err(ActionError::BadInput(_))
        ));
        let scanned = job.run();
        let _ = lock(&d.registry).finish_scan(&job, scanned);
    }

    #[test]
    fn peer_lists_are_deduplicated_however_they_arrive() {
        let a = DestHash([1; 32]);
        let b = DestHash([2; 32]);
        let c = DestHash([3; 32]);
        // Interleaved duplicates — two trackers returning overlapping sets —
        // which a neighbour-only dedup leaves in place.
        let got = distinct(vec![a, b, a, c, b, a]);
        assert_eq!(got.len(), 3);
        for want in [a, b, c] {
            assert!(got.contains(&want));
        }
        assert!(distinct(Vec::new()).is_empty());
        assert_eq!(distinct(vec![a, a, a]).len(), 1);
    }

    #[test]
    fn the_jitter_roll_is_in_range_and_moves() {
        let rolls: Vec<f64> = (0..64).map(|_| random_roll()).collect();
        for roll in &rolls {
            assert!(
                (0.0..1.0).contains(roll),
                "{roll} is outside the [0,1) a jitter roll must be in"
            );
        }
        // Not a constant: a fixed roll de-synchronises nothing, which is the
        // entire reason jitter exists. Bit-inequality is the claim here, not
        // numeric closeness, so an epsilon comparison would be the wrong test.
        assert!(
            rolls
                .windows(2)
                .any(|pair| pair[0].to_bits() != pair[1].to_bits()),
            "every roll was identical"
        );
        // And it must actually shorten a delay when applied.
        let policy = ReconnectPolicy::default();
        let base = policy.base_delay(5);
        assert!(policy.jittered(base, 1.0) < base);
    }

    #[test]
    fn the_peer_id_is_random_and_labelled() {
        let a = build_peer_id().expect("peer id");
        let b = build_peer_id().expect("peer id");
        assert!(a.starts_with(b"-CV0001-"), "Q7 prefix missing");
        assert_ne!(
            a, b,
            "two peer ids were identical: the random tail is not random"
        );
        // The old fallback shipped this when getrandom failed; every instance
        // that hit it would announce under one identity.
        assert_ne!(a, *b"-CV0001-............");
    }

    #[test]
    fn boolean_bodies_are_strict() {
        assert_eq!(parse_bool_body(b"true"), Some(true));
        assert_eq!(parse_bool_body(b"false"), Some(false));
        assert_eq!(parse_bool_body(b" true\n"), Some(true));
        // A typo must be an error, not silently "off".
        assert_eq!(parse_bool_body(b"True"), None);
        assert_eq!(parse_bool_body(b"1"), None);
        assert_eq!(parse_bool_body(b"yes"), None);
        assert_eq!(parse_bool_body(b""), None);
        assert_eq!(parse_bool_body(&[0xFF]), None);
    }

    #[test]
    fn data_query_flag() {
        assert!(query_has_data("data=1"));
        assert!(query_has_data("data"));
        assert!(query_has_data("data=true"));
        assert!(query_has_data("foo=1&data=yes"));
        assert!(!query_has_data(""));
        assert!(!query_has_data("data=0"));
        assert!(!query_has_data("data=maybe"));
        // Substring matches must not count.
        assert!(!query_has_data("metadata=1"));
        assert!(!query_has_data("data_files=1"));
    }

    #[test]
    fn sam_port_extraction() {
        assert_eq!(sam_tcp_port("127.0.0.1:7656"), Some(7656));
        assert_eq!(sam_tcp_port("localhost:7656"), Some(7656));
        assert_eq!(sam_tcp_port("[::1]:7656"), Some(7656));
        // A unix path has no port, and must not be guessed at: the sandbox
        // leaves outbound TCP alone rather than pinning the wrong port.
        assert_eq!(sam_tcp_port("/run/i2pd/sam.sock"), None);
        assert_eq!(sam_tcp_port("127.0.0.1"), None);
        assert_eq!(sam_tcp_port("127.0.0.1:notaport"), None);
        assert_eq!(sam_tcp_port("127.0.0.1:99999"), None);
    }

    #[test]
    fn an_empty_token_authenticates_nobody() {
        // The bug this locks down: `constant_time_eq("", "")` is true, so a
        // daemon whose token file was empty answered any request carrying a
        // bare `x-clove-token:` header. A zero-byte token file was reachable
        // from an interrupted first start.
        let dir = TempDir::new("empty-token");
        let d = Arc::new(Daemon {
            start: Instant::now(),
            sam_address: "127.0.0.1:7656".to_owned(),
            token: String::new(),
            peer_id: *b"-CV0001-testtesttes\0",
            registry: Mutex::new(
                Registry::open(&dir.0, registry::Limits::default()).expect("registry"),
            ),
            config: test_config(&dir, false),
            rpc: None,
            router: Mutex::new("connecting"),
        });
        for header in [Some(""), Some(" "), None, Some(TOKEN)] {
            assert_eq!(
                status_of(&speak(&d, &get("/v1/status", header))),
                401,
                "token {header:?} was accepted against an empty expected token"
            );
        }
    }

    /// Write a secret fixture the way the daemon writes one: `0600`.
    ///
    /// The default umask gives `0644`, which `read_private_file` now refuses —
    /// correctly, and these fixtures stand in for files clove itself created.
    /// A test that wrote them world-readable would be asserting against a state
    /// the daemon never produces.
    fn write_secret_fixture(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    #[test]
    fn a_malformed_token_file_is_replaced_not_trusted() {
        use std::os::unix::fs::PermissionsExt;
        // Every way the file can be present but useless. Each must produce a
        // fresh, well-formed, private token rather than a weak secret.
        for (what, contents) in [
            ("empty", ""),
            ("whitespace", "\n\n  \t"),
            ("truncated", "0123456789abcdef"),
            ("not hex", &"z".repeat(TOKEN_HEX_LEN)),
            ("too long", &"a".repeat(TOKEN_HEX_LEN + 1)),
        ] {
            let dir = TempDir::new(&format!("bad-token-{what}"));
            let path = dir.0.join("token");
            write_secret_fixture(&path, contents);
            let token = load_or_create_token(&dir.0).expect("load");
            assert!(
                is_well_formed_token(&token),
                "{what}: got {token:?} back as a token"
            );
            assert_eq!(
                std::fs::read_to_string(&path).expect("re-read").trim(),
                token,
                "{what}: the new token was not persisted"
            );
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{what}: replacement token is not private");
            // No temp file left behind.
            let strays: Vec<_> = std::fs::read_dir(&dir.0)
                .expect("read dir")
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
                .collect();
            assert!(strays.is_empty(), "{what}: left a temp file behind");
        }
    }

    #[test]
    fn a_well_formed_token_file_is_left_alone() {
        // The other half: a valid token must survive a restart untouched,
        // including a trailing newline, or every running CLI breaks.
        let dir = TempDir::new("good-token");
        let path = dir.0.join("token");
        write_secret_fixture(&path, &format!("{TOKEN}\n"));
        assert_eq!(
            load_or_create_token(&dir.0).expect("load"),
            TOKEN,
            "a valid token was rotated"
        );
    }

    #[test]
    fn token_shape_check() {
        assert!(is_well_formed_token(&"a".repeat(TOKEN_HEX_LEN)));
        assert!(is_well_formed_token(&"F".repeat(TOKEN_HEX_LEN)));
        assert!(!is_well_formed_token(""));
        assert!(!is_well_formed_token(&"a".repeat(TOKEN_HEX_LEN - 1)));
        assert!(!is_well_formed_token(&"a".repeat(TOKEN_HEX_LEN + 1)));
        assert!(!is_well_formed_token(&"g".repeat(TOKEN_HEX_LEN)));
        // Same length, one non-hex byte.
        let mut nearly = "a".repeat(TOKEN_HEX_LEN - 1);
        nearly.push('-');
        assert!(!is_well_formed_token(&nearly));
    }

    // ------------------------------------------------- identity (Q4)

    /// A SAM private key blob: a complete destination followed by the private
    /// crypto and signing keys, the shape §5.1c captured from i2pd.
    fn key_blob_b64() -> String {
        let mut blob = vec![0x42u8; 384];
        blob.push(0x05);
        blob.extend_from_slice(&4u16.to_be_bytes());
        blob.extend_from_slice(&[0x00, 0x07, 0x00, 0x00]);
        blob.extend(std::iter::repeat_n(0xAAu8, 288));
        i2pnet::addr::i2p_base64_encode(&blob)
    }

    #[test]
    fn an_identity_survives_a_restart() {
        let dir = TempDir::new("identity");
        let key = key_blob_b64();

        // Nothing stored yet: the first run asks the router for a transient
        // destination and keeps what comes back.
        let identity = Identity::new(&dir.0, false);
        assert_eq!(identity.load(), None);
        identity.remember(&key);

        // Every later run is the same peer.
        assert_eq!(Identity::new(&dir.0, false).load().as_deref(), Some(&*key));
    }

    #[test]
    fn the_destination_key_is_private_and_written_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("identity-mode");
        Identity::new(&dir.0, false).remember(&key_blob_b64());

        let path = dir.0.join("destination.key");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the destination key is readable by other users"
        );
        let strays: Vec<_> = std::fs::read_dir(&dir.0)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left a temp file behind");
    }

    #[test]
    fn ephemeral_neither_reads_nor_writes_a_key() {
        let dir = TempDir::new("identity-ephemeral");
        // Even with a perfectly good key sitting there.
        std::fs::write(dir.0.join("destination.key"), key_blob_b64()).expect("write");

        let identity = Identity::new(&dir.0, true);
        assert_eq!(
            identity.load(),
            None,
            "ephemeral must not reuse an identity"
        );
        identity.remember(&key_blob_b64());
        // And it must not have touched the file either way.
        assert_eq!(
            std::fs::read_dir(&dir.0).expect("read dir").count(),
            1,
            "ephemeral wrote something"
        );
    }

    /// A key file that is present but unusable must not be sent to the router.
    ///
    /// `SESSION CREATE` with a malformed `DESTINATION=` is refused, and the
    /// supervisor retries into the same refusal forever — the daemon never
    /// connects again. A truncated file reaching §2.9's failure mode is much
    /// more likely than a stale session id ever was.
    #[test]
    fn a_corrupt_key_file_is_refused_rather_than_sent_to_the_router() {
        let full = key_blob_b64();
        for (what, contents) in [
            ("empty", String::new()),
            ("whitespace", "\n \t".to_owned()),
            ("not base64", "!!!not a key!!!".to_owned()),
            ("truncated mid-blob", full[..40].to_owned()),
            // The nastiest one: a *valid destination* with no private half.
            // Well-formed, decodes cleanly, and useless as a DESTINATION= —
            // the router cannot sign with a public key.
            (
                "the public destination only",
                i2pnet::addr::i2p_base64_encode(
                    &i2pnet::addr::i2p_base64_decode(&full).expect("decode")[..391],
                ),
            ),
        ] {
            let dir = TempDir::new(&format!("identity-bad-{}", what.replace(' ', "-")));
            write_secret_fixture(&dir.0.join("destination.key"), &contents);
            assert_eq!(
                Identity::new(&dir.0, false).load(),
                None,
                "{what}: a key that cannot work was handed to the router"
            );
        }
        // And the real thing still loads, or the check above is just refusing
        // everything.
        let dir = TempDir::new("identity-good");
        write_secret_fixture(&dir.0.join("destination.key"), &full);
        assert_eq!(Identity::new(&dir.0, false).load().as_deref(), Some(&*full));
    }

    /// A secret that is not private is refused, not read.
    ///
    /// A first run creates both files `0600`, so this is about one that arrived
    /// another way — restored from a backup that dropped modes, copied by hand,
    /// unpacked from an archive. Reading it anyway means the API token or the
    /// client's stable identity has been readable by everyone on the machine and
    /// clove never said so.
    ///
    /// The token case is fatal on purpose: silently rotating it would hide that
    /// the old one had been exposed, and a daemon whose token somebody else may
    /// hold should not come up serving the API.
    #[test]
    fn a_world_readable_secret_is_refused_rather_than_used() {
        use std::os::unix::fs::PermissionsExt;

        for mode in [0o644, 0o640, 0o604, 0o666] {
            let dir = TempDir::new(&format!("loose-token-{mode:o}"));
            let path = dir.0.join("token");
            std::fs::write(&path, TOKEN).expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let err =
                load_or_create_token(&dir.0).expect_err(&format!("a {mode:o} token was accepted"));
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
            assert_eq!(
                std::fs::read_to_string(&path).expect("re-read").trim(),
                TOKEN,
                "the exposed token was replaced instead of reported"
            );

            // The identity is the same check, and declines to the transient
            // path rather than being fatal: a daemon with no stored key still
            // runs, it just comes back as somebody else.
            let dir = TempDir::new(&format!("loose-key-{mode:o}"));
            let key = dir.0.join("destination.key");
            std::fs::write(&key, key_blob_b64()).expect("write");
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).expect("chmod");
            assert_eq!(
                Identity::new(&dir.0, false).load(),
                None,
                "a {mode:o} destination key was sent to the router"
            );
        }
    }

    /// A symlink where a secret belongs is refused rather than followed.
    #[test]
    fn a_symlinked_secret_is_refused() {
        let dir = TempDir::new("linked-token");
        let elsewhere = dir.0.join("elsewhere");
        std::fs::write(&elsewhere, TOKEN).expect("write");
        std::os::unix::fs::symlink(&elsewhere, dir.0.join("token")).expect("symlink");
        let err = load_or_create_token(&dir.0).expect_err("a symlinked token was followed");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    }

    /// A destination plus a stray byte is not a key, however well-formed the
    /// destination is.
    ///
    /// The old check asked only that *something* followed the destination, so a
    /// single trailing byte passed — and the result was handed to
    /// `SESSION CREATE`, refused, and retried for the life of the process.
    #[test]
    fn a_destination_with_a_stray_byte_is_not_a_key() {
        let full = key_blob_b64();
        let bytes = i2pnet::addr::i2p_base64_decode(&full).expect("decode");
        let dest = i2pnet::addr::destination_len(&bytes).expect("destination");

        for extra in [1usize, 8, MIN_PRIVATE_KEY_BYTES - 1] {
            let mut truncated = bytes[..dest].to_vec();
            truncated.extend(std::iter::repeat_n(0x41, extra));
            assert!(
                !is_private_key_blob(&i2pnet::addr::i2p_base64_encode(&truncated)),
                "a destination plus {extra} byte(s) was accepted as a key"
            );
        }
        // The boundary, and the real thing, both pass.
        let mut just_enough = bytes[..dest].to_vec();
        just_enough.extend(std::iter::repeat_n(0x41, MIN_PRIVATE_KEY_BYTES));
        assert!(is_private_key_blob(&i2pnet::addr::i2p_base64_encode(
            &just_enough
        )));
        assert!(is_private_key_blob(&full));
    }

    #[test]
    fn a_stored_identity_is_not_rewritten_on_every_reconnect() {
        // The supervisor calls `remember` on every successful session, and the
        // session tree rebuilds whenever the router blips. Rewriting the key
        // each time is a needless fsync and a needless window in which the
        // file does not exist.
        let dir = TempDir::new("identity-idempotent");
        let identity = Identity::new(&dir.0, false);
        identity.remember(&key_blob_b64());
        let path = dir.0.join("destination.key");
        let first = std::fs::metadata(&path).expect("stat");
        let before = first.modified().expect("mtime");

        identity.remember(&key_blob_b64());
        let after = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        assert_eq!(before, after, "the key was rewritten with the same value");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read").trim(),
            key_blob_b64()
        );
    }

    #[test]
    fn the_token_file_is_created_once_and_kept_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("token");
        let first = load_or_create_token(&dir.0).expect("create");
        assert_eq!(first.len(), 64, "32 random bytes, hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        let mode = std::fs::metadata(dir.0.join("token"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "token readable by another local user");

        // A second call reads the existing token rather than rotating it —
        // rotating would invalidate every running CLI on restart.
        assert_eq!(load_or_create_token(&dir.0).expect("reuse"), first);
    }

    /// A small single-file torrent, for tests that need a real `.torrent`
    /// rather than a real download. Returns its contents and its bytes.
    fn rpc_fixture(name: &str) -> (Vec<u8>, Vec<u8>) {
        use clove_core::bencode::{self, Value as Ben};
        use sha1::{Digest, Sha1};
        use std::collections::BTreeMap;

        let content: Vec<u8> = (0..(2 * 16 * 1024u32))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let pieces: Vec<u8> = content
            .chunks(16 * 1024)
            .flat_map(|c| <[u8; 20]>::from(Sha1::digest(c)))
            .collect();
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), Ben::Bytes(name.as_bytes().to_vec()));
        info.insert(b"piece length".to_vec(), Ben::Int(16 * 1024));
        info.insert(b"pieces".to_vec(), Ben::Bytes(pieces));
        info.insert(
            b"length".to_vec(),
            Ben::Int(i64::try_from(content.len()).expect("fits")),
        );
        let mut root = BTreeMap::new();
        root.insert(b"info".to_vec(), Ben::Dict(info));
        (content, bencode::encode(&Ben::Dict(root)))
    }

    /// Standard base64, written out here rather than imported.
    ///
    /// `clove_core::base64` only decodes, and a test that encoded with the same
    /// code it is checking would pass whatever that code did. This is the
    /// independent half of the round trip.
    fn base64_encode(raw: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in raw.chunks(3) {
            let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
            let n = (b(0) << 16) | (b(1) << 8) | b(2);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    // ------------------------------------------- Transmission RPC surface

    /// Everything in this section drives the same `handle` the socket does, so
    /// it exercises the authentication split rather than the dispatch alone —
    /// which is the half where a mistake is a security bug rather than a
    /// compatibility one.
    mod transmission_rpc {
        use super::*;

        /// `user:password` as HTTP Basic, standard base64 by hand so the test
        /// does not prove the decoder correct using the decoder.
        fn basic(password: &str) -> String {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let raw = format!("clove:{password}").into_bytes();
            let mut out = String::new();
            for chunk in raw.chunks(3) {
                let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
                let n = (b(0) << 16) | (b(1) << 8) | b(2);
                for i in 0..4 {
                    if i <= chunk.len() {
                        out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        }

        /// A `POST /transmission/rpc` with the given credentials and session id.
        fn rpc_request(body: &str, auth: Option<&str>, session: Option<&str>) -> Vec<u8> {
            let mut req = String::from("POST /transmission/rpc HTTP/1.1\r\nHost: localhost\r\n");
            if let Some(password) = auth {
                let _ = write!(req, "Authorization: Basic {}\r\n", basic(password));
            }
            if let Some(id) = session {
                let _ = write!(req, "X-Transmission-Session-Id: {id}\r\n");
            }
            let _ = write!(req, "Content-Length: {}\r\n\r\n{body}", body.len());
            req.into_bytes()
        }

        /// Drive one authenticated call and return its parsed JSON body.
        fn call(d: &Arc<Daemon>, body: &str) -> Value {
            let session = d.rpc.as_ref().expect("rpc enabled").session_id().to_owned();
            let reply = speak(d, &rpc_request(body, Some(TOKEN), Some(&session)));
            assert_eq!(status_of(&reply), 200, "{reply}");
            let (_head, json) = reply.split_once("\r\n\r\n").expect("a body");
            clove_core::json::parse(json).unwrap_or_else(|e| panic!("{e:?} parsing {json:?}"))
        }

        fn result_of(value: &Value) -> String {
            value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("<none>")
                .to_owned()
        }

        /// The header a client is *required* to be handed before it can do
        /// anything, and must not be handed before it authenticates.
        #[test]
        fn the_csrf_id_is_only_given_to_an_authenticated_caller() {
            let dir = TempDir::new("rpc-csrf");
            let d = daemon_with(&dir, true);

            // No credentials: 401, a challenge, and crucially no session id to
            // harvest.
            let anonymous = speak(&d, &rpc_request("{}", None, None));
            assert_eq!(status_of(&anonymous), 401, "{anonymous}");
            assert!(
                anonymous.to_lowercase().contains("www-authenticate"),
                "no Basic challenge in {anonymous}"
            );
            assert!(
                !anonymous
                    .to_lowercase()
                    .contains("x-transmission-session-id"),
                "the session id leaked to an unauthenticated caller: {anonymous}"
            );

            // Wrong password, same length as the real token: still 401.
            let wrong = speak(&d, &rpc_request("{}", Some(&"f".repeat(TOKEN.len())), None));
            assert_eq!(status_of(&wrong), 401, "{wrong}");

            // Authenticated but no session id: 409 carrying the id, which is
            // the handshake every client is built around.
            let challenge = speak(&d, &rpc_request("{}", Some(TOKEN), None));
            assert_eq!(status_of(&challenge), 409, "{challenge}");
            assert!(
                challenge
                    .to_lowercase()
                    .contains("x-transmission-session-id"),
                "the 409 did not carry the id: {challenge}"
            );

            // A wrong session id is refused as firmly as a missing one.
            let stale = speak(&d, &rpc_request("{}", Some(TOKEN), Some("stale")));
            assert_eq!(status_of(&stale), 409, "{stale}");

            // Both, and it answers.
            let session = d.rpc.as_ref().expect("rpc").session_id().to_owned();
            let ok = speak(
                &d,
                &rpc_request(r#"{"method":"session-get"}"#, Some(TOKEN), Some(&session)),
            );
            assert_eq!(status_of(&ok), 200, "{ok}");
        }

        /// Off by default, and off means the path is not special at all: it
        /// falls through to the token scheme like any other unknown path,
        /// rather than to no scheme.
        #[test]
        fn the_surface_is_unreachable_unless_configured() {
            let dir = TempDir::new("rpc-off");
            let d = daemon(&dir);
            assert!(d.rpc.is_none(), "the default should not enable it");

            let session = "whatever";
            for credentials in [None, Some(TOKEN)] {
                let reply = speak(&d, &rpc_request("{}", credentials, Some(session)));
                assert_eq!(
                    status_of(&reply),
                    401,
                    "a disabled surface answered something other than 401: {reply}"
                );
            }
        }

        /// Enabling one surface must not weaken the other.
        #[test]
        fn the_v1_surface_is_unchanged_when_rpc_is_enabled() {
            let dir = TempDir::new("rpc-v1");
            let d = daemon_with(&dir, true);
            assert_eq!(status_of(&speak(&d, &get("/v1/status", None))), 401);
            assert_eq!(status_of(&speak(&d, &get("/v1/status", Some("x")))), 401);
            assert_eq!(status_of(&speak(&d, &get("/v1/status", Some(TOKEN)))), 200);
            // And an unknown path still refuses for the token rather than
            // admitting it does not exist.
            assert_eq!(status_of(&speak(&d, &get("/v1/nope", None))), 401);
            // Transmission's Basic credentials are not accepted on `/v1/`.
            let mut req = String::from("GET /v1/status HTTP/1.1\r\nHost: localhost\r\n");
            let _ = write!(req, "Authorization: Basic {}\r\n\r\n", basic(TOKEN));
            assert_eq!(status_of(&speak(&d, req.as_bytes())), 401);
        }

        /// A malformed request is a *protocol-level* success carrying a failure
        /// string, because that is where clients look. Answering 400 makes them
        /// report the server as broken.
        #[test]
        fn bad_requests_come_back_as_a_result_string_not_a_status_code() {
            let dir = TempDir::new("rpc-bad");
            let d = daemon_with(&dir, true);

            for (body, expect) in [
                ("not json at all", "valid JSON"),
                ("{}", "no method named"),
                (r#"{"method":"no-such-method"}"#, "unsupported method"),
            ] {
                let value = call(&d, body);
                let result = result_of(&value);
                assert!(
                    result.contains(expect),
                    "{body:?} answered {result:?}, wanted something about {expect:?}"
                );
            }
        }

        /// The tag is how a client matches a reply to the request it sent, so
        /// it has to survive a *failure* as well as a success.
        #[test]
        fn the_tag_is_echoed_on_both_outcomes() {
            let dir = TempDir::new("rpc-tag");
            let d = daemon_with(&dir, true);

            let ok = call(&d, r#"{"method":"session-get","tag":42}"#);
            assert_eq!(ok.get("tag").and_then(Value::as_u64), Some(42));

            let bad = call(&d, r#"{"method":"nope","tag":7}"#);
            assert_eq!(bad.get("tag").and_then(Value::as_u64), Some(7));

            // No tag sent, none echoed: a client keyed on tags must not be
            // handed one it never used.
            let none = call(&d, r#"{"method":"session-get"}"#);
            assert!(none.get("tag").is_none(), "a tag was invented");

            // Some clients send it as a float. Both spellings mean 3.
            let floaty = call(&d, r#"{"method":"session-get","tag":3.0}"#);
            assert_eq!(floaty.get("tag").and_then(Value::as_u64), Some(3));
        }

        #[test]
        fn session_get_reports_the_configuration_and_the_i2p_constants() {
            let dir = TempDir::new("rpc-session");
            let d = daemon_with(&dir, true);
            let value = call(&d, r#"{"method":"session-get"}"#);
            assert_eq!(result_of(&value), "success");
            let args = value.get("arguments").expect("arguments");

            // Real settings, read from the config this daemon was built with.
            assert_eq!(
                args.get("peer-limit-global").and_then(Value::as_u64),
                Some(clove_core::config::DEFAULT_PEER_LIMIT as u64)
            );
            assert_eq!(
                args.get("download-queue-size").and_then(Value::as_u64),
                Some(clove_core::config::DEFAULT_MAX_ACTIVE_DOWNLOADS as u64)
            );

            // The constants that are true of every I2P client rather than
            // stubs. If any of these ever reports `true`, something has been
            // added that SCOPE §2 or §10 says must not exist.
            for key in [
                "dht-enabled",
                "lpd-enabled",
                "utp-enabled",
                "blocklist-enabled",
            ] {
                assert_eq!(
                    args.get(key).and_then(Value::as_bool),
                    Some(false),
                    "{key} should be false on an I2P-only client"
                );
            }
            assert_eq!(args.get("pex-enabled").and_then(Value::as_bool), Some(true));
            assert_eq!(args.get("peer-port").and_then(Value::as_u64), Some(0));

            // The version has to name Transmission for clients, and clove for
            // whoever reads it.
            let version = args
                .get("version")
                .and_then(Value::as_str)
                .expect("version");
            assert!(
                version.starts_with('4'),
                "{version} must look like Transmission's"
            );
            assert!(
                version.contains("clove"),
                "{version} must say what it really is"
            );
        }

        /// The round trip a client actually performs: add, list, act, remove.
        #[test]
        fn a_torrent_can_be_added_listed_and_removed() {
            let dir = TempDir::new("rpc-roundtrip");
            let d = daemon_with(&dir, true);
            let (_content, bytes) = rpc_fixture("rpc-demo");
            let encoded = base64_encode(&bytes);

            let added = call(
                &d,
                &format!(r#"{{"method":"torrent-add","arguments":{{"metainfo":"{encoded}"}}}}"#),
            );
            assert_eq!(result_of(&added), "success");
            let entry = added
                .get("arguments")
                .and_then(|a| a.get("torrent-added"))
                .expect("torrent-added");
            let id = entry.get("id").and_then(Value::as_u64).expect("an id");
            assert_eq!(entry.get("name").and_then(Value::as_str), Some("rpc-demo"));
            let hash = entry
                .get("hashString")
                .and_then(Value::as_str)
                .expect("a hash")
                .to_owned();
            assert_eq!(hash.len(), 40);

            // Adding it again is not an error: it is `torrent-duplicate`, which
            // is what makes an \*arr client's retry idempotent.
            let again = call(
                &d,
                &format!(r#"{{"method":"torrent-add","arguments":{{"metainfo":"{encoded}"}}}}"#),
            );
            assert_eq!(result_of(&again), "success");
            let dup = again
                .get("arguments")
                .and_then(|a| a.get("torrent-duplicate"))
                .expect("torrent-duplicate");
            assert_eq!(
                dup.get("hashString").and_then(Value::as_str),
                Some(&hash[..])
            );

            // It lists, with the id it was given and the fields asked for.
            let listed = call(
                &d,
                r#"{"method":"torrent-get","arguments":{"fields":["id","name","hashString","status","percentDone","rateDownload","eta","queuePosition"]}}"#,
            );
            let torrents = listed
                .get("arguments")
                .and_then(|a| a.get("torrents"))
                .and_then(Value::as_array)
                .expect("torrents");
            assert_eq!(torrents.len(), 1);
            let t = &torrents[0];
            assert_eq!(t.get("id").and_then(Value::as_u64), Some(id));
            assert_eq!(t.get("name").and_then(Value::as_str), Some("rpc-demo"));
            assert_eq!(t.get("queuePosition").and_then(Value::as_u64), Some(0));
            // Only what was asked for came back.
            assert!(t.get("files").is_none(), "an unrequested field was sent");

            // Stopping it is visible in the next poll.
            let stopped = call(
                &d,
                &format!(r#"{{"method":"torrent-stop","arguments":{{"ids":[{id}]}}}}"#),
            );
            assert_eq!(result_of(&stopped), "success");
            let listed = call(
                &d,
                r#"{"method":"torrent-get","arguments":{"fields":["status"]}}"#,
            );
            let status = listed
                .get("arguments")
                .and_then(|a| a.get("torrents"))
                .and_then(Value::as_array)
                .and_then(|t| t.first())
                .and_then(|t| t.get("status"))
                .and_then(Value::as_u64);
            assert_eq!(status, Some(0), "a stopped torrent should report STOPPED");

            // And it can be named by hash as well as by id.
            let removed = call(
                &d,
                &format!(r#"{{"method":"torrent-remove","arguments":{{"ids":["{hash}"]}}}}"#),
            );
            assert_eq!(result_of(&removed), "success");
            let empty = call(
                &d,
                r#"{"method":"torrent-get","arguments":{"fields":["id"]}}"#,
            );
            let torrents = empty
                .get("arguments")
                .and_then(|a| a.get("torrents"))
                .and_then(Value::as_array)
                .expect("torrents");
            assert!(torrents.is_empty(), "the torrent survived its removal");
        }

        /// `recently-active` is the mode a client uses to notice deletions, and
        /// the `removed` half of it has to be exact or the client shows a
        /// torrent that is gone for ever.
        #[test]
        fn a_removed_torrent_is_reported_to_a_recently_active_poller() {
            let dir = TempDir::new("rpc-removed");
            let d = daemon_with(&dir, true);
            let (_content, bytes) = rpc_fixture("gone-soon");
            let encoded = base64_encode(&bytes);
            call(
                &d,
                &format!(r#"{{"method":"torrent-add","arguments":{{"metainfo":"{encoded}"}}}}"#),
            );

            // A first poll, so the surface knows what the client has seen.
            let first = call(
                &d,
                r#"{"method":"torrent-get","arguments":{"ids":"recently-active","fields":["id"]}}"#,
            );
            let id = first
                .get("arguments")
                .and_then(|a| a.get("torrents"))
                .and_then(Value::as_array)
                .and_then(|t| t.first())
                .and_then(|t| t.get("id"))
                .and_then(Value::as_u64)
                .expect("an id");
            let removed = first
                .get("arguments")
                .and_then(|a| a.get("removed"))
                .and_then(Value::as_array)
                .expect("removed");
            assert!(removed.is_empty(), "nothing had gone yet");

            call(
                &d,
                &format!(r#"{{"method":"torrent-remove","arguments":{{"ids":[{id}]}}}}"#),
            );

            let second = call(
                &d,
                r#"{"method":"torrent-get","arguments":{"ids":"recently-active","fields":["id"]}}"#,
            );
            let removed: Vec<u64> = second
                .get("arguments")
                .and_then(|a| a.get("removed"))
                .and_then(Value::as_array)
                .expect("removed")
                .iter()
                .filter_map(Value::as_u64)
                .collect();
            assert_eq!(removed, vec![id], "the removal was not reported");
        }

        /// The refusal that is about the architecture rather than about
        /// convenience: a URL add is a clearnet fetch.
        #[test]
        fn a_url_add_is_refused_for_the_reason_it_is_refused() {
            let dir = TempDir::new("rpc-url");
            let d = daemon_with(&dir, true);
            let value = call(
                &d,
                r#"{"method":"torrent-add","arguments":{"filename":"http://example.com/x.torrent"}}"#,
            );
            let result = result_of(&value);
            assert!(
                result.contains("clearnet"),
                "the refusal should say why, not just no: {result}"
            );
            // And a magnet through the same argument is accepted, so the
            // refusal is about the scheme and not about `filename`.
            let magnet = call(
                &d,
                r#"{"method":"torrent-add","arguments":{"filename":"magnet:?xt=urn:btih:3f2a91c0d4e5b6a7889900112233445566778899"}}"#,
            );
            assert_eq!(result_of(&magnet), "success", "a magnet should still add");
        }

        /// Adversarial envelopes, since the daemon now parses JSON from a
        /// stranger for the first time (`PHASE-F.md` §2 used to say it never
        /// did). `fuzz/README.md` explains why this is a sweep here rather than
        /// a fuzz target: it can drive the real `handle`, authentication and
        /// all, which a target linked against a binary crate cannot.
        ///
        /// The contract is the one `tests/hostile.rs` states for every other
        /// parser: answer or refuse, never panic, never hang, never allocate
        /// without bound. Nothing here should *succeed*; what matters is that
        /// each one comes back as an ordinary reply.
        #[test]
        fn a_hostile_envelope_is_answered_rather_than_survived() {
            let dir = TempDir::new("rpc-hostile");
            let d = daemon_with(&dir, true);

            let deep_array = format!("{}{}", "[".repeat(200), "]".repeat(200));
            let bodies = vec![
                String::new(),
                "null".to_owned(),
                "[]".to_owned(),
                "0".to_owned(),
                "\"a string\"".to_owned(),
                r#"{"method":null}"#.to_owned(),
                r#"{"method":[]}"#.to_owned(),
                r#"{"method":"torrent-get","arguments":"not an object"}"#.to_owned(),
                r#"{"method":"torrent-get","arguments":{"fields":"not an array"}}"#.to_owned(),
                r#"{"method":"torrent-get","arguments":{"fields":[null,1,{},[]]}}"#.to_owned(),
                r#"{"method":"torrent-get","arguments":{"ids":{}}}"#.to_owned(),
                r#"{"method":"torrent-get","arguments":{"ids":[-1,0,1e308,"zz"]}}"#.to_owned(),
                // Ids that are the right shape but name nothing.
                r#"{"method":"torrent-stop","arguments":{"ids":[999999]}}"#.to_owned(),
                r#"{"method":"torrent-remove","arguments":{"ids":["ff"]}}"#.to_owned(),
                // A tag that is not a number, and one that cannot be one.
                r#"{"method":"session-get","tag":"seven"}"#.to_owned(),
                r#"{"method":"session-get","tag":1e400}"#.to_owned(),
                // Nesting, which the parser caps rather than recursing on.
                format!(r#"{{"method":"torrent-get","arguments":{{"ids":{deep_array}}}}}"#),
                // A base64 payload that is not a torrent, and one that is not
                // base64 — the two halves of the only new parser here.
                r#"{"method":"torrent-add","arguments":{"metainfo":"////////"}}"#.to_owned(),
                r#"{"method":"torrent-add","arguments":{"metainfo":"not base64!!"}}"#.to_owned(),
                r#"{"method":"torrent-add","arguments":{"metainfo":""}}"#.to_owned(),
                r#"{"method":"torrent-add","arguments":{"filename":"/etc/passwd"}}"#.to_owned(),
                r#"{"method":"torrent-add","arguments":{}}"#.to_owned(),
                // Priorities pointing outside the file list.
                r#"{"method":"torrent-set","arguments":{"files-wanted":[99999999]}}"#.to_owned(),
                r#"{"method":"torrent-set","arguments":{"seedRatioLimit":-5}}"#.to_owned(),
                // A method name that is not ASCII, and one that is enormous.
                r#"{"method":"\u0000\ud83d\ude00"}"#.to_owned(),
                format!(r#"{{"method":"{}"}}"#, "x".repeat(10_000)),
            ];

            for body in bodies {
                let session = d.rpc.as_ref().expect("rpc").session_id().to_owned();
                let reply = speak(&d, &rpc_request(&body, Some(TOKEN), Some(&session)));
                let status = status_of(&reply);
                assert!(
                    status == 200 || status == 400,
                    "{body:.80?} answered {status}, which is neither a reply nor a refusal"
                );
                if status == 200 {
                    let (_head, json) = reply.split_once("\r\n\r\n").expect("a body");
                    let value = clove_core::json::parse(json)
                        .unwrap_or_else(|e| panic!("{e:?} parsing our own reply to {body:.80?}"));
                    assert!(
                        value.get("result").and_then(Value::as_str).is_some(),
                        "no result string in the reply to {body:.80?}"
                    );
                }
            }

            // And the daemon is still answering afterwards, which is the part a
            // panic in a connection thread would not have broken loudly.
            let after = call(&d, r#"{"method":"session-get"}"#);
            assert_eq!(result_of(&after), "success");
        }

        /// `session-set` refuses rather than accepting and dropping the change:
        /// a setting that silently does not stick is worse than one that says
        /// where it lives.
        #[test]
        fn session_set_says_where_settings_actually_live() {
            let dir = TempDir::new("rpc-set");
            let d = daemon_with(&dir, true);
            let value = call(
                &d,
                r#"{"method":"session-set","arguments":{"peer-limit-global":10}}"#,
            );
            let result = result_of(&value);
            assert!(result.contains("clove.conf"), "{result}");
            // The setting really is unchanged, which is the half a client
            // could otherwise not tell.
            let after = call(&d, r#"{"method":"session-get"}"#);
            assert_eq!(
                after
                    .get("arguments")
                    .and_then(|a| a.get("peer-limit-global"))
                    .and_then(Value::as_u64),
                Some(clove_core::config::DEFAULT_PEER_LIMIT as u64)
            );
        }
    }
}
