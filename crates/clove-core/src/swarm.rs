//! Peer acquisition for a running torrent: the swarm runner (Phase F,
//! `docs/PHASE-F.md` — the mock-first half of the SAM wiring slice).
//!
//! A [`Swarm`] owns two background threads around one [`Torrent`]:
//!
//! - the **dial sweep**: periodically walks the torrent's
//!   [`known_peers`](Torrent::known_peers) (tracker, PEX, magnet `x.pe`,
//!   operator), skips peers already
//!   [connected](Torrent::connected_peers) or in per-peer retry backoff, and
//!   dials + attaches the rest, up to [`SwarmConfig::max_peers`]. A failed
//!   dial (leaseSet warmup `CantReachPeer`, refusal, timeout — see
//!   `PROTOCOL.i2p-bt` §2.6b) schedules that peer for retry after
//!   [`SwarmConfig::retry_backoff`] instead of being forgotten. Dial
//!   *initiation* is sequential by design — it serializes on the session
//!   anyway (§2.6a).
//! - the **acceptor**: blocks on [`I2pListener::accept`], attaching each
//!   inbound peer, refusing (dropping) connections past `max_peers`. It exits
//!   when `accept` fails — session loss — because re-establishing the session
//!   tree is the supervisor's job, not this module's.
//!
//! Backend-agnostic: generic over [`I2pDialer`]/[`I2pListener`], so the mock
//! network proves the logic in CI and the SAM backend reuses it unchanged.
//! All timing is [`SwarmConfig`]-tunable (R5).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use i2pnet::{DestHash, I2pDialer, I2pListener, I2pNamingLookup, I2pStream};

use crate::torrent::Torrent;
use crate::tracker;
use crate::wire::{self, Handshake};

/// Swarm-runner timing and limits. Every field exists to be tuned once live
/// I2P behavior is measured (R5); the defaults lean generous because tunnel
/// latency dwarfs clearnet expectations.
#[derive(Clone, Copy, Debug)]
pub struct SwarmConfig {
    /// Per-attempt dial timeout handed to the dialer.
    pub dial_timeout: Duration,
    /// Pause between dial sweeps.
    pub sweep_interval: Duration,
    /// How long a peer sits out after a failed dial or attach before it is
    /// eligible again (leaseSet-warmup retries land here).
    pub retry_backoff: Duration,
    /// Stop dialing, and refuse inbound, at this many attached peers.
    pub max_peers: usize,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        SwarmConfig {
            dial_timeout: Duration::from_secs(120),
            sweep_interval: Duration::from_secs(10),
            retry_backoff: Duration::from_secs(30),
            max_peers: 50,
        }
    }
}

/// The running swarm threads for one torrent. Dropping it does *not* stop
/// them; call [`shutdown`](Swarm::shutdown).
pub struct Swarm {
    stop: Arc<StopFlag>,
    dial_thread: Option<JoinHandle<()>>,
}

impl Swarm {
    /// Start the dial sweep, and — when `listener` is given — the acceptor,
    /// for `torrent`.
    pub fn spawn<D, L>(
        torrent: Arc<Torrent>,
        dialer: D,
        listener: Option<L>,
        config: SwarmConfig,
    ) -> Swarm
    where
        D: I2pDialer + Send + 'static,
        D::Stream: 'static,
        L: I2pListener + Send + 'static,
        L::Stream: 'static,
    {
        let stop = Arc::new(StopFlag::default());
        if let Some(listener) = listener {
            let torrent = Arc::clone(&torrent);
            // The acceptor is deliberately detached: it blocks in accept()
            // and ends when the listener's session dies (supervisor
            // teardown), not on our stop flag.
            std::thread::spawn(move || accept_loop(&torrent, &listener, config.max_peers));
        }
        let dial_stop = Arc::clone(&stop);
        let dial_thread =
            std::thread::spawn(move || dial_loop(&torrent, &dialer, &dial_stop, config));
        Swarm {
            stop,
            dial_thread: Some(dial_thread),
        }
    }

    /// Start a dial-only swarm (no inbound listener) — e.g. before the
    /// session's forwarded listener exists.
    pub fn dial_only<D>(torrent: Arc<Torrent>, dialer: D, config: SwarmConfig) -> Swarm
    where
        D: I2pDialer + Send + 'static,
        D::Stream: 'static,
    {
        Swarm::spawn::<D, NoListener>(torrent, dialer, None, config)
    }

    /// Signal the dial sweep to stop without waiting: it exits at the next
    /// check, after any in-flight dial completes. Use when blocking on
    /// [`shutdown`](Swarm::shutdown) (up to a dial timeout) is unacceptable —
    /// e.g. an API-driven pause.
    pub fn request_stop(&self) {
        self.stop.raise();
    }

    /// Signal the dial sweep to stop and wait for it to finish. The acceptor
    /// thread (if any) ends when its listener's session is torn down.
    pub fn shutdown(mut self) {
        self.stop.raise();
        if let Some(handle) = self.dial_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Announcer timing. Tunable (R5); the state machine's own scheduling
/// ([`AnnounceState`]) governs per-tracker cadence — this is just the poll
/// tick and transport limits.
#[derive(Clone, Debug)]
pub struct AnnouncerConfig {
    /// How often due-ness is checked.
    pub poll_interval: Duration,
    /// Dial timeout for reaching a tracker.
    pub dial_timeout: Duration,
    /// Peers requested per announce.
    pub numwant: u32,
}

impl Default for AnnouncerConfig {
    fn default() -> Self {
        AnnouncerConfig {
            poll_interval: Duration::from_secs(5),
            dial_timeout: Duration::from_secs(120),
            numwant: 50,
        }
    }
}

/// The tracker announce loop for one torrent: resolves each announce URL's
/// host (SAM naming), dials the tracker over I2P, performs the HTTP announce
/// (`tracker::announce_over`), and feeds returned peers into
/// [`Torrent::add_peers`] for the swarm's dial sweep. Scheduling and backoff
/// per URL follow [`AnnounceState`] (interval floor, exponential failure
/// backoff — `PROTOCOL.i2p-bt` §5.3).
///
/// Each URL is currently tracked independently rather than with strict BEP 12
/// tier semantics (fine for the one-or-two-tracker torrents typical on I2P;
/// revisit against live swarms).
pub struct Announcer {
    stop: Arc<StopFlag>,
    thread: Option<JoinHandle<()>>,
}

/// What the announcer announces: the URLs and our torrent-shape facts.
pub struct AnnounceTarget {
    /// Announce URLs (flattened tiers; see the tier note on [`Announcer`]).
    pub urls: Vec<String>,
    /// Our session's full base64 destination — the announce `ip` parameter.
    pub our_dest_b64: String,
    /// The torrent's piece length in bytes, sizing the `left` report.
    pub piece_length: u64,
    /// The torrent's total length in bytes.
    pub total_length: u64,
}

impl Announcer {
    /// Start announcing `torrent` per `target`.
    pub fn spawn<D, N>(
        torrent: Arc<Torrent>,
        target: AnnounceTarget,
        dialer: D,
        naming: N,
        config: AnnouncerConfig,
    ) -> Announcer
    where
        D: I2pDialer + Send + 'static,
        N: I2pNamingLookup + Send + 'static,
    {
        let stop = Arc::new(StopFlag::default());
        let loop_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            announce_loop(&torrent, &target, &dialer, &naming, &config, &loop_stop);
        });
        Announcer {
            stop,
            thread: Some(thread),
        }
    }

    /// Signal the loop to stop without waiting (it exits after any in-flight
    /// announce).
    pub fn request_stop(&self) {
        self.stop.raise();
    }

    /// Signal the loop to stop and wait for it to finish.
    pub fn shutdown(mut self) {
        self.stop.raise();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Fire one best-effort `stopped` announce to every URL on a detached
/// thread — a graceful goodbye on pause/remove/shutdown. Failures are
/// swallowed: the tracker times us out anyway.
pub fn announce_stopped<D, N>(
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    target: AnnounceTarget,
    dialer: D,
    naming: N,
    dial_timeout: Duration,
) where
    D: I2pDialer + Send + 'static,
    N: I2pNamingLookup + Send + 'static,
{
    std::thread::spawn(move || {
        for url in &target.urls {
            let params = tracker::AnnounceParams {
                info_hash,
                peer_id,
                uploaded: 0,
                downloaded: 0,
                left: 0,
                event: tracker::Event::Stopped,
                numwant: 0,
                our_dest_b64: &target.our_dest_b64,
            };
            let Ok((host, request)) = tracker::build_announce(url, &params) else {
                continue;
            };
            let Ok(dest) = naming.lookup(&host) else {
                continue;
            };
            let Ok(mut stream) = dialer.dial(dest, dial_timeout) else {
                continue;
            };
            let _ = tracker::announce_over(&mut stream, &request);
        }
    });
}

/// Seconds since the unix epoch — the announce state machine's clock.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn announce_loop<D, N>(
    torrent: &Arc<Torrent>,
    target: &AnnounceTarget,
    dialer: &D,
    naming: &N,
    config: &AnnouncerConfig,
    stop: &StopFlag,
) where
    D: I2pDialer,
    N: I2pNamingLookup,
{
    let mut states: Vec<tracker::AnnounceState> = target
        .urls
        .iter()
        .map(|_| tracker::AnnounceState::new())
        .collect();
    loop {
        for (url, state) in target.urls.iter().zip(states.iter_mut()) {
            if stop.is_raised() {
                return;
            }
            if !state.due(unix_now()) {
                continue;
            }
            match announce_once(torrent, url, state, target, dialer, naming, config) {
                Ok(interval) => state.on_success(unix_now(), interval),
                Err(_) => state.on_failure(unix_now()),
            }
        }
        if stop.wait(config.poll_interval) {
            return;
        }
    }
}

/// One announce to one tracker: build, resolve, dial, exchange, feed peers.
fn announce_once<D, N>(
    torrent: &Arc<Torrent>,
    url: &str,
    state: &tracker::AnnounceState,
    target: &AnnounceTarget,
    dialer: &D,
    naming: &N,
    config: &AnnouncerConfig,
) -> Result<u32, tracker::Error>
where
    D: I2pDialer,
    N: I2pNamingLookup,
{
    let have = torrent.have();
    let complete = have.count() == have.len();
    let done = u64::from(have.count()).saturating_mul(target.piece_length);
    let left = if complete {
        0
    } else {
        target
            .total_length
            .saturating_sub(done.min(target.total_length))
    };
    let (uploaded, downloaded) = torrent.stats();
    let params = tracker::AnnounceParams {
        info_hash: torrent.info_hash(),
        peer_id: torrent.peer_id(),
        uploaded,
        downloaded,
        left,
        event: state.next_event(complete),
        numwant: config.numwant,
        our_dest_b64: &target.our_dest_b64,
    };
    let (host, request) = tracker::build_announce(url, &params)?;
    let dest = naming.lookup(&host).map_err(tracker::Error::Io)?;
    let mut stream = dialer
        .dial(dest, config.dial_timeout)
        .map_err(tracker::Error::Io)?;
    let response = tracker::announce_over(&mut stream, &request)?;
    torrent.add_peers(&response.peers);
    Ok(response.interval)
}

/// One dial sweep after another until stopped, with per-peer retry backoff.
fn dial_loop<D>(torrent: &Arc<Torrent>, dialer: &D, stop: &StopFlag, config: SwarmConfig)
where
    D: I2pDialer,
    D::Stream: 'static,
{
    let mut retry_after: HashMap<DestHash, Instant> = HashMap::new();
    loop {
        sweep(torrent, dialer, stop, &config, &mut retry_after);
        if stop.wait(config.sweep_interval) {
            return;
        }
    }
}

/// Dial every eligible known peer once, newest state first.
fn sweep<D>(
    torrent: &Arc<Torrent>,
    dialer: &D,
    stop: &StopFlag,
    config: &SwarmConfig,
    retry_after: &mut HashMap<DestHash, Instant>,
) where
    D: I2pDialer,
    D::Stream: 'static,
{
    let connected: Vec<DestHash> = torrent.connected_peers();
    let mut budget = config.max_peers.saturating_sub(connected.len());
    if budget == 0 {
        return;
    }
    let now = Instant::now();
    retry_after.retain(|_, at| *at > now);
    for peer in torrent.known_peers() {
        if budget == 0 || stop.is_raised() {
            return;
        }
        if connected.contains(&peer) || retry_after.contains_key(&peer) {
            continue;
        }
        let attached = dialer
            .dial(peer, config.dial_timeout)
            .and_then(|stream| torrent.attach(stream, peer));
        match attached {
            Ok(()) => budget -= 1,
            Err(_) => {
                retry_after.insert(peer, Instant::now() + config.retry_backoff);
            }
        }
    }
}

/// Accept inbound peers until the listener's session dies. Connections past
/// `max_peers` are dropped, which the dialing side sees as a refusal.
fn accept_loop<L>(torrent: &Arc<Torrent>, listener: &L, max_peers: usize)
where
    L: I2pListener,
    L::Stream: 'static,
{
    loop {
        match listener.accept() {
            Ok((stream, from)) => {
                if torrent.connected_peers().len() >= max_peers {
                    drop(stream);
                    continue;
                }
                // A failed handshake just closes this peer; the loop lives on.
                let _ = torrent.attach(stream, from);
            }
            Err(_) => return,
        }
    }
}

/// Routes inbound peers to torrents on a shared destination (Q4: one client
/// identity serves every torrent), which is the daemon's real inbound shape —
/// [`Swarm::spawn`]'s per-torrent listener fits single-torrent uses and tests.
///
/// The demux owns the session's one listener: for each accepted stream it
/// reads the peer's BEP 3 handshake on a short-lived thread (so a stalling
/// peer never blocks the accept loop), looks the info-hash up, and hands the
/// stream to that torrent via [`Torrent::attach_accepted`]. Unknown
/// info-hashes and torrents at their peer cap are dropped — the dialing side
/// sees a refusal.
pub struct InboundDemux {
    torrents: Mutex<HashMap<[u8; 20], Arc<Torrent>>>,
    /// Per-torrent attached-peer cap, matching [`SwarmConfig::max_peers`].
    max_peers: usize,
    /// Raised on session teardown; the accept loop exits at its next accept.
    stopped: std::sync::atomic::AtomicBool,
}

impl InboundDemux {
    /// An empty demux with the given per-torrent peer cap.
    #[must_use]
    pub fn new(max_peers: usize) -> Arc<InboundDemux> {
        Arc::new(InboundDemux {
            torrents: Mutex::new(HashMap::new()),
            max_peers,
            stopped: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Serve `torrent`'s info-hash. Replaces any previous registration.
    pub fn register(&self, torrent: &Arc<Torrent>) {
        lock_map(&self.torrents).insert(torrent.info_hash(), Arc::clone(torrent));
    }

    /// Stop serving an info-hash (torrent removed/paused). Peers already
    /// attached are unaffected; new inbound connections for it are dropped.
    pub fn unregister(&self, info_hash: &[u8; 20]) {
        lock_map(&self.torrents).remove(info_hash);
    }

    /// Raise the stop flag: the accept loop exits at its next accept. A
    /// blocked accept needs a poke (e.g. `sam::poke_listener`) or the
    /// session's death to wake it.
    pub fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Run the accept loop on `listener` until its session dies (the
    /// supervisor owns re-establishment; a rebuilt session gets a fresh
    /// `run`). Returns the loop's thread handle.
    pub fn run<L>(self: &Arc<Self>, listener: L) -> JoinHandle<()>
    where
        L: I2pListener + Send + 'static,
        L::Stream: 'static,
    {
        let demux = Arc::clone(self);
        std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, from)) => {
                        if demux.stopped.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        let demux = Arc::clone(&demux);
                        // Per-connection thread: the handshake read must
                        // never stall the accept loop.
                        std::thread::spawn(move || demux.route(stream, from));
                    }
                    Err(_) => return,
                }
            }
        })
    }

    /// Read one inbound peer's handshake and attach it to its torrent.
    fn route<S: I2pStream + 'static>(&self, mut stream: S, from: DestHash) {
        let mut buf = [0u8; wire::HANDSHAKE_LEN];
        if stream.read_exact(&mut buf).is_err() {
            return;
        }
        let Ok(theirs) = Handshake::parse(&buf) else {
            return;
        };
        let torrent = lock_map(&self.torrents).get(&theirs.info_hash).cloned();
        let Some(torrent) = torrent else {
            return; // unknown info-hash: drop, nothing to say
        };
        if torrent.connected_peers().len() >= self.max_peers {
            return;
        }
        let _ = torrent.attach_accepted(stream, from, &theirs);
    }
}

fn lock_map<K, V>(m: &Mutex<HashMap<K, V>>) -> std::sync::MutexGuard<'_, HashMap<K, V>> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A raise-once flag with a timed wait, so `shutdown` interrupts the sweep
/// pause immediately instead of sleeping it out.
#[derive(Default)]
struct StopFlag {
    raised: Mutex<bool>,
    cv: Condvar,
}

impl StopFlag {
    fn raise(&self) {
        *self.raised.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    fn is_raised(&self) -> bool {
        *self.raised.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait up to `timeout`; returns whether the flag is raised.
    fn wait(&self, timeout: Duration) -> bool {
        let guard = self.raised.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |raised| !*raised)
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

/// An uninhabited listener for [`Swarm::dial_only`]: it cannot be
/// constructed, so the acceptor thread is never spawned.
pub enum NoListener {}

/// The (equally uninhabited) stream type of [`NoListener`].
pub enum NoStream {}

impl Read for NoStream {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        match *self {}
    }
}

impl Write for NoStream {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        match *self {}
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {}
    }
}

impl I2pStream for NoStream {
    type Reader = NoStream;
    type Writer = NoStream;

    fn split(self) -> io::Result<(NoStream, NoStream)> {
        match self {}
    }
}

impl I2pListener for NoListener {
    type Stream = NoStream;

    fn local_dest(&self) -> DestHash {
        match *self {}
    }

    fn accept(&self) -> io::Result<(NoStream, DestHash)> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitfield::Bitfield;
    use crate::metainfo::{FileEntry, InfoHash, MetaInfo};
    use crate::picker::Mode;
    use crate::storage::Storage;
    use crate::wire::BLOCK_LEN;
    use i2pnet::mock::MockNet;
    use sha1::{Digest, Sha1};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static C: AtomicU32 = AtomicU32::new(0);
            let n = C.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("clove-swarm-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn quick_config() -> SwarmConfig {
        SwarmConfig {
            dial_timeout: Duration::from_millis(100),
            sweep_interval: Duration::from_millis(50),
            retry_backoff: Duration::from_millis(100),
            max_peers: 8,
        }
    }

    /// A multi-piece torrent plus a complete seeder and an empty leecher.
    fn seed_and_leech() -> (Arc<Torrent>, Arc<Torrent>, TempDir, TempDir) {
        seed_and_leech_tagged(0x44)
    }

    /// Like [`seed_and_leech`], but `tag` differentiates both the info-hash
    /// and the content, so several distinct torrents can coexist in one test.
    fn seed_and_leech_tagged(tag: u8) -> (Arc<Torrent>, Arc<Torrent>, TempDir, TempDir) {
        let content: Vec<u8> = (0..(3 * BLOCK_LEN + 100))
            .map(|i| u8::try_from((i + u32::from(tag)) % 251).unwrap_or(0))
            .collect();
        let pieces: Vec<[u8; 20]> = content
            .chunks(BLOCK_LEN as usize)
            .map(|c| Sha1::digest(c).into())
            .collect();
        let meta = MetaInfo {
            info_hash: InfoHash([tag; 20]),
            name: format!("swarm-demo-{tag}"),
            piece_length: BLOCK_LEN,
            pieces,
            files: vec![FileEntry {
                path: vec!["swarm-demo".into()],
                length: content.len() as u64,
            }],
            total_length: content.len() as u64,
            private: true,
            trackers: vec![],
            skipped_trackers: 0,
            raw_info: Vec::new(),
        };

        let seed_dir = TempDir::new("seed");
        let seed_storage = Arc::new(Storage::create(&meta, &seed_dir.0, false).unwrap());
        for p in 0..seed_storage.num_pieces() {
            let start = p as usize * BLOCK_LEN as usize;
            let end = (start + seed_storage.piece_len(p) as usize).min(content.len());
            seed_storage
                .write_block(p, 0, &content[start..end])
                .unwrap();
        }
        let seed_have = seed_storage.verify_all().unwrap();
        assert!(seed_have.is_full());
        let seeder = Torrent::new(
            &meta,
            seed_storage,
            &seed_have,
            Mode::RarestFirst,
            *b"-CV0001-seedseedseed",
        );

        let leech_dir = TempDir::new("leech");
        let leech_storage = Arc::new(Storage::create(&meta, &leech_dir.0, false).unwrap());
        let leecher = Torrent::new(
            &meta,
            leech_storage,
            &Bitfield::empty(meta.pieces.len().try_into().unwrap()),
            Mode::RarestFirst,
            *b"-CV0001-leechleechle",
        );
        (seeder, leecher, seed_dir, leech_dir)
    }

    #[test]
    fn swarm_completes_a_download() {
        let net = MockNet::new();
        let (seeder, leecher, _sd, _ld) = seed_and_leech();

        let seed_ep = net.endpoint();
        let leech_ep = net.endpoint();
        let seed_dest = seed_ep.dest();

        let seed_swarm = Swarm::spawn(
            Arc::clone(&seeder),
            seed_ep.dialer(),
            Some(seed_ep),
            quick_config(),
        );
        leecher.add_peers(&[seed_dest]);
        let leech_swarm = Swarm::dial_only(Arc::clone(&leecher), leech_ep.dialer(), quick_config());

        assert!(
            leecher.wait_complete(Duration::from_secs(20)),
            "leecher did not complete via the swarm runner"
        );
        leech_swarm.shutdown();
        seed_swarm.shutdown();
    }

    #[test]
    fn failed_dials_retry_after_backoff() {
        let net = MockNet::new();
        let (seeder, leecher, _sd, _ld) = seed_and_leech();

        let seed_ep = net.endpoint();
        let leech_ep = net.endpoint();
        let seed_dest = seed_ep.dest();
        let seed_faults = seed_ep.fault_handle();

        // The seeder is unreachable at first — every dial burns its timeout
        // and fails, as during leaseSet warmup.
        seed_faults.set_black_hole(true);

        let seed_swarm = Swarm::spawn(
            Arc::clone(&seeder),
            seed_ep.dialer(),
            Some(seed_ep),
            quick_config(),
        );
        leecher.add_peers(&[seed_dest]);
        let leech_swarm = Swarm::dial_only(Arc::clone(&leecher), leech_ep.dialer(), quick_config());

        // Let a few failed dial rounds happen, then lift the fault.
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !leecher.is_complete(),
            "must not complete while black-holed"
        );
        seed_faults.set_black_hole(false);

        assert!(
            leecher.wait_complete(Duration::from_secs(20)),
            "leecher did not recover once the peer became reachable"
        );
        leech_swarm.shutdown();
        seed_swarm.shutdown();
    }

    #[test]
    fn demux_routes_two_torrents_on_one_destination() {
        let net = MockNet::new();
        let (seeder_a, leecher_a, _sa, _la) = seed_and_leech_tagged(0x21);
        let (seeder_b, leecher_b, _sb, _lb) = seed_and_leech_tagged(0x22);

        // One seeding endpoint serves both torrents through the demux.
        let seed_ep = net.endpoint();
        let seed_dest = seed_ep.dest();
        let demux = InboundDemux::new(8);
        demux.register(&seeder_a);
        demux.register(&seeder_b);
        let _accept = demux.run(seed_ep);

        // Each leecher dials the same destination for its own torrent.
        let ep_a = net.endpoint();
        let ep_b = net.endpoint();
        leecher_a.add_peers(&[seed_dest]);
        leecher_b.add_peers(&[seed_dest]);
        let swarm_a = Swarm::dial_only(Arc::clone(&leecher_a), ep_a.dialer(), quick_config());
        let swarm_b = Swarm::dial_only(Arc::clone(&leecher_b), ep_b.dialer(), quick_config());

        assert!(
            leecher_a.wait_complete(Duration::from_secs(20)),
            "torrent A did not complete through the demux"
        );
        assert!(
            leecher_b.wait_complete(Duration::from_secs(20)),
            "torrent B did not complete through the demux"
        );
        swarm_a.shutdown();
        swarm_b.shutdown();
    }

    #[test]
    fn demux_drops_unknown_info_hash() {
        let net = MockNet::new();
        let (seeder_a, _leecher_a, _sa, _la) = seed_and_leech_tagged(0x31);
        let (_seeder_c, leecher_c, _sc, _lc) = seed_and_leech_tagged(0x33);

        let seed_ep = net.endpoint();
        let seed_dest = seed_ep.dest();
        let demux = InboundDemux::new(8);
        demux.register(&seeder_a); // torrent C is NOT registered
        let _accept = demux.run(seed_ep);

        let ep_c = net.endpoint();
        let stream = ep_c
            .dialer()
            .dial(seed_dest, Duration::from_secs(5))
            .unwrap();
        // The demux reads C's handshake, finds no torrent, and drops the
        // stream; the initiator's attach fails reading the reply.
        assert!(leecher_c.attach(stream, seed_dest).is_err());
    }

    #[test]
    fn announcer_bootstraps_a_download_from_a_tracker() {
        let net = MockNet::new();
        let (seeder, leecher, _sd, _ld) = seed_and_leech();

        // Seeder accepts inbound (core swarm with listener).
        let seed_ep = net.endpoint();
        let seed_dest = seed_ep.dest();
        let seed_swarm = Swarm::spawn(
            Arc::clone(&seeder),
            seed_ep.dialer(),
            Some(seed_ep),
            quick_config(),
        );

        // A mock tracker: reads one HTTP request, replies with a compact
        // bencoded response naming the seeder, forever.
        let tracker_ep = net.endpoint();
        net.register_name("tracker.i2p", tracker_ep.dest());
        std::thread::spawn(move || {
            while let Ok((mut stream, _from)) = tracker_ep.accept() {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).map(|n| n == 0).unwrap_or(true) {
                        break;
                    }
                    head.push(byte[0]);
                }
                let mut body = b"d8:intervali60e5:peers32:".to_vec();
                body.extend_from_slice(&seed_dest.0);
                body.push(b'e');
                let response = crate::http::Response::new(200, "text/plain", body);
                let _ = stream.write_all(&response.encode());
            }
        });

        // The leecher knows only the tracker URL; peers arrive via announce.
        let leech_ep = net.endpoint();
        let leech_swarm = Swarm::dial_only(Arc::clone(&leecher), leech_ep.dialer(), quick_config());
        let announcer = Announcer::spawn(
            Arc::clone(&leecher),
            AnnounceTarget {
                urls: vec!["http://tracker.i2p/announce".to_owned()],
                our_dest_b64: "leecher-b64-dest".to_owned(),
                piece_length: u64::from(BLOCK_LEN),
                total_length: u64::from(3 * BLOCK_LEN + 100),
            },
            leech_ep.dialer(),
            leech_ep.dialer(),
            AnnouncerConfig {
                poll_interval: Duration::from_millis(50),
                dial_timeout: Duration::from_secs(5),
                numwant: 8,
            },
        );

        assert!(
            leecher.wait_complete(Duration::from_secs(20)),
            "tracker-bootstrapped download did not complete"
        );
        announcer.shutdown();
        leech_swarm.shutdown();
        seed_swarm.shutdown();
    }

    #[test]
    fn disconnect_all_empties_the_peer_table() {
        let net = MockNet::new();
        let (seeder, leecher, _sd, _ld) = seed_and_leech();

        let seed_ep = net.endpoint();
        let leech_ep = net.endpoint();
        let seed_dest = seed_ep.dest();

        let seeder_bg = Arc::clone(&seeder);
        let accept = std::thread::spawn(move || {
            let (stream, from) = seed_ep.accept().unwrap();
            seeder_bg.attach(stream, from).unwrap();
        });
        let stream = leech_ep
            .dialer()
            .dial(seed_dest, Duration::from_secs(5))
            .unwrap();
        leecher.attach(stream, seed_dest).unwrap();
        accept.join().unwrap();
        assert_eq!(leecher.connected_peers().len(), 1);

        leecher.disconnect_all();
        assert!(
            leecher.connected_peers().is_empty(),
            "peer table must be empty after disconnect_all"
        );
    }

    #[test]
    fn shutdown_interrupts_a_long_sweep_pause() {
        let net = MockNet::new();
        let (_seeder, leecher, _sd, _ld) = seed_and_leech();
        let ep = net.endpoint();

        let config = SwarmConfig {
            sweep_interval: Duration::from_secs(3600),
            ..quick_config()
        };
        let swarm = Swarm::dial_only(leecher, ep.dialer(), config);

        let start = Instant::now();
        swarm.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "shutdown had to wait out the sweep pause"
        );
    }
}
