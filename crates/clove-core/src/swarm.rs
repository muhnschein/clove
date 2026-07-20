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

use i2pnet::{DestHash, I2pDialer, I2pListener, I2pStream};

use crate::torrent::Torrent;

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

    /// Signal the dial sweep to stop and wait for it to finish. The acceptor
    /// thread (if any) ends when its listener's session is torn down.
    pub fn shutdown(mut self) {
        self.stop.raise();
        if let Some(handle) = self.dial_thread.take() {
            let _ = handle.join();
        }
    }
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
        let content: Vec<u8> = (0..(3 * BLOCK_LEN + 100))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let pieces: Vec<[u8; 20]> = content
            .chunks(BLOCK_LEN as usize)
            .map(|c| Sha1::digest(c).into())
            .collect();
        let meta = MetaInfo {
            info_hash: InfoHash([0x44; 20]),
            name: "swarm-demo".into(),
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
