//! The torrent registry: the daemon's engine host — hosted torrents, their
//! on-disk state (`docs/STATE-FORMAT.md`), and, once a network backend is
//! attached, a live [`Torrent`] + [`Swarm`] per unpaused entry.
//!
//! Generic over the dialer so the mock network proves the engine wiring in CI
//! and the SAM backend slots into the same seam (`docs/PHASE-F.md` §7 5c).
//! Until [`attach_network`](Registry::attach_network) is called, entries are
//! static state ("waiting for router"); afterwards each unpaused torrent gets
//! storage, a live engine instance registered with the session's
//! [`InboundDemux`], and a dial-only [`Swarm`] (inbound arrives via the
//! demux).
//!
//! Layout under the data dir:
//! - `state/<info-hash>.torrent` — the exact `.torrent` bytes.
//! - `state/<info-hash>.resume`  — versioned bencode resume data.
//! - `downloads/<name>/…`        — the torrent's files.
//!
//! State files are written atomically (temp + fsync + rename), so a crash mid
//! -write never corrupts them — worst case is re-verification.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clove_core::bitfield::Bitfield;
use clove_core::json::Value;
use clove_core::magnet::{self, Magnet};
use clove_core::metainfo::{self, MetaInfo};
use clove_core::picker::Mode;
use clove_core::resume::Resume;
use clove_core::storage::Storage;
use clove_core::swarm::{
    AnnounceTarget, Announcer, AnnouncerConfig, InboundDemux, Swarm, SwarmConfig,
};
use clove_core::torrent::{DEFAULT_MAINTENANCE_INTERVAL, Maintenance, Torrent};
use clove_core::tracker::MIN_ANNOUNCE_INTERVAL;
use i2pnet::naming::NamingCache;
use i2pnet::{DestHash, I2pDialer, I2pNamingLookup};

/// The set of hosted torrents plus where their state lives.
pub(crate) struct Registry<D: I2pDialer + I2pNamingLookup + Clone + Send + Sync + 'static>
where
    D::Stream: 'static,
{
    state_dir: PathBuf,
    downloads_dir: PathBuf,
    torrents: BTreeMap<[u8; 20], Hosted>,
    /// Magnet adds still fetching their metadata (BEP 9). Promoted into
    /// `torrents` by [`complete_magnet`](Registry::complete_magnet).
    pending: BTreeMap<[u8; 20], PendingMagnet>,
    network: Option<Network<D>>,
}

/// A magnet awaiting metadata; its fetch loop runs on a daemon thread.
struct PendingMagnet {
    magnet: Magnet,
    /// Set once a fetch thread owns this entry, so it is spawned once.
    claimed: bool,
    /// What the fetch loop has managed so far. A magnet that never resolves
    /// used to be indistinguishable from one that resolved a second ago —
    /// both were the bare word `fetching-metadata` — which is no state at all
    /// to debug from. Written by [`note_fetch_round`](Registry::note_fetch_round).
    progress: FetchProgress,
}

/// The visible half of a metadata fetch: how hard it has tried, and why the
/// last attempt did not work.
#[derive(Default)]
struct FetchProgress {
    rounds: u32,
    peers_known: usize,
    peers_tried: usize,
    trackers_ok: usize,
    trackers_failed: usize,
    last_error: Option<String>,
}

/// What a metadata-fetch round has to work with.
pub(crate) struct FetchContext<D> {
    /// The magnet's I2P tracker URLs.
    pub(crate) trackers: Vec<String>,
    pub(crate) dialer: D,
    pub(crate) naming: NamingCache<D>,
    pub(crate) peer_id: [u8; 20],
    pub(crate) dest_b64: String,
}

/// The attached network backend: everything needed to bring a torrent live.
struct Network<D> {
    dialer: D,
    demux: Arc<InboundDemux>,
    peer_id: [u8; 20],
    swarm_config: SwarmConfig,
    /// Our session's full base64 destination, for announce `ip=`.
    dest_b64: String,
    /// Shared lookup cache (R6) in front of the session's resolver.
    naming: NamingCache<D>,
}

impl<D: I2pDialer> Network<D> {
    /// Whether this network is still worth handing work to.
    fn usable(&self) -> bool {
        self.dialer.usable()
    }
}

/// A pass over everything a torrent has on disk, to be run *outside* the
/// registry lock.
///
/// `verify_all` reads and SHA-1s every byte present, which for a large torrent
/// is minutes of work. The registry lives behind one mutex that every API
/// request takes, so hashing while holding it stops the whole daemon: status,
/// list, pause, remove and the periodic resume writer all queue behind it. The
/// lock therefore hands out this job, the hashing happens with nothing held, and
/// a second short lock publishes the result
/// ([`finish_scan`](Registry::finish_scan)).
#[must_use = "a scan that is never run and published leaves its torrent marked \
              as verifying, and it will never start"]
pub(crate) struct ScanJob {
    info_hash: [u8; 20],
    meta: MetaInfo,
    downloads_dir: PathBuf,
}

impl ScanJob {
    /// Lay the files out if they are not there yet, then hash what is.
    ///
    /// Takes as long as it takes; the caller must not be holding the registry.
    ///
    /// # Errors
    ///
    /// Any filesystem error opening or reading the torrent's files.
    pub(crate) fn run(&self) -> io::Result<Bitfield> {
        let storage = Storage::create(&self.meta, &self.downloads_dir, false)?;
        storage.verify_all()
    }
}

/// One hosted torrent's in-memory summary.
struct Hosted {
    meta: MetaInfo,
    have: Bitfield,
    /// The subset of [`have`](Hosted::have) we have both hashed and made
    /// durable — what a restart is allowed to believe without re-reading the
    /// disk.
    ///
    /// Advanced only after a successful `sync_all`, so it lags `have` by at
    /// most one persist tick while a torrent is running and catches up on a
    /// clean stop. A crash therefore costs re-verifying that tick's worth,
    /// which is the bargain `docs/STATE-FORMAT.md` describes and the reason
    /// the two fields exist separately at all.
    verified: Bitfield,
    priorities: Vec<u8>,
    uploaded: u64,
    downloaded: u64,
    /// Peers attached right now, and destinations we could dial. Snapshotted
    /// by [`Registry::refresh`] alongside the byte counters, because a live
    /// run's first question is never "how many bytes" but "is anybody there":
    /// zero peers and zero bytes is a peer-acquisition problem, while eight
    /// peers and zero bytes is a wire or choking problem, and the two look
    /// identical without this number.
    peers: usize,
    known_peers: usize,
    /// Of those, how many arrived over `i2p_pex` (M3's PEX criterion).
    pex_peers: u64,
    /// Peers that reached us rather than being dialed — the live proof of the
    /// inbound `STREAM FORWARD` path (`PROTOCOL.i2p-bt` §2.5).
    inbound_peers: u64,
    /// Announces that worked, announces that did not, and the last reason —
    /// the first question to ask of a torrent with no peers.
    announces_ok: u32,
    announces_failed: u32,
    last_announce_error: Option<String>,
    paused: bool,
    /// Pick pieces in order rather than rarest-first (SCOPE §3).
    sequential: bool,
    /// A hash of everything on disk is running for this torrent, outside the
    /// registry lock. Nothing may start its engine or publish a have-set until
    /// that finishes.
    scanning: bool,
    live: Option<Live>,
}

/// A torrent's running engine half.
struct Live {
    torrent: Arc<Torrent>,
    swarm: Swarm,
    announcer: Option<Announcer>,
    /// Keep-alives, idle-peer drops and choke rounds. Held rather than used:
    /// dropping it with the rest of `Live` is what stops the tick on pause.
    _maintenance: Maintenance,
    /// Lifetime (uploaded, downloaded) at engine start; the torrent's own
    /// counters are per-run deltas on top of these.
    stats_base: (u64, u64),
    /// When the operator last forced an announce, so a script cannot turn
    /// `clove announce` into a tracker flood.
    last_forced_announce: Option<Instant>,
}

impl Hosted {
    /// The resume record to persist for this torrent's current state.
    fn resume(&self, info_hash: [u8; 20]) -> Resume {
        Resume {
            info_hash,
            num_pieces: self.have.len(),
            have: self.have.as_bytes().to_vec(),
            // Not a copy of `have`. A piece enters `have` when its bytes pass
            // SHA-1 in memory, which says nothing about whether the write
            // reached the platter; `verified` is the weaker, truer claim that
            // we fsynced the files and the piece was good as of then
            // (`docs/STATE-FORMAT.md`). Copying `have` here is what let a
            // crash — or a bad sector afterwards — come back as a torrent
            // serving pieces nobody had checked.
            verified: self.verified.as_bytes().to_vec(),
            priorities: self.priorities.clone(),
            uploaded: self.uploaded,
            downloaded: self.downloaded,
            trackers: self.meta.trackers.clone(),
            paused: self.paused,
            sequential: self.sequential,
        }
    }
}

/// Why a torrent action (pause, verify, set priorities…) failed.
#[derive(Debug)]
pub(crate) enum ActionError {
    /// No torrent with that info-hash (404).
    NotFound,
    /// The request was malformed (400).
    BadInput(&'static str),
    /// A filesystem error (500).
    Io(io::Error),
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::NotFound => write!(f, "no such torrent"),
            ActionError::BadInput(what) => write!(f, "{what}"),
            ActionError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Why adding a torrent failed (mapped to an HTTP status by the caller).
#[derive(Debug)]
pub(crate) enum AddError {
    /// The `.torrent` did not parse (400).
    Parse(metainfo::Error),
    /// The magnet URI did not parse (400).
    Magnet(magnet::Error),
    /// A torrent with this info-hash is already hosted (409).
    Duplicate,
    /// A filesystem error (500).
    Io(io::Error),
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddError::Parse(e) => write!(f, "{e}"),
            AddError::Magnet(e) => write!(f, "{e}"),
            AddError::Duplicate => write!(f, "torrent already added"),
            AddError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Why removing a torrent failed.
#[derive(Debug)]
pub(crate) enum RemoveError {
    /// No torrent with that info-hash (404).
    NotFound,
    /// A filesystem error (500).
    Io(io::Error),
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoveError::NotFound => write!(f, "no such torrent"),
            RemoveError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl<D: I2pDialer + I2pNamingLookup + Clone + Send + Sync + 'static> Registry<D>
where
    D::Stream: 'static,
{
    /// Open the registry under `data_dir`, creating the `state`/`downloads`
    /// directories and loading any previously added torrents. No network is
    /// attached yet; torrents sit in "waiting for router".
    ///
    /// # Errors
    ///
    /// The state or downloads directory cannot be created.
    pub(crate) fn open(data_dir: &Path) -> io::Result<Registry<D>> {
        let state_dir = data_dir.join("state");
        let downloads_dir = data_dir.join("downloads");
        fs::create_dir_all(&state_dir)?;
        fs::create_dir_all(&downloads_dir)?;
        let mut registry = Registry {
            state_dir,
            downloads_dir,
            torrents: BTreeMap::new(),
            pending: BTreeMap::new(),
            network: None,
        };
        registry.load_all();
        Ok(registry)
    }

    /// Attach the network backend (the session's dialer and inbound demux)
    /// and bring every unpaused torrent live.
    pub(crate) fn attach_network(
        &mut self,
        dialer: D,
        demux: Arc<InboundDemux>,
        peer_id: [u8; 20],
        swarm_config: SwarmConfig,
        dest_b64: String,
    ) {
        let naming = NamingCache::new(dialer.clone());
        self.network = Some(Network {
            dialer,
            demux,
            peer_id,
            swarm_config,
            dest_b64,
            naming,
        });
        let hashes: Vec<[u8; 20]> = self.torrents.keys().copied().collect();
        for info_hash in hashes {
            if let Err(e) = self.start_live(&info_hash) {
                eprintln!("cloved: starting {}: {e}", hex(&info_hash));
            }
        }
    }

    /// Detach the network backend (session lost): every live torrent goes
    /// offline with its progress snapshotted, and entries fall back to
    /// "waiting-for-router" until a rebuilt session re-attaches.
    pub(crate) fn detach_network(&mut self) {
        let hashes: Vec<[u8; 20]> = self.torrents.keys().copied().collect();
        for info_hash in hashes {
            self.stop_live(&info_hash);
        }
        self.network = None;
        self.persist_progress();
    }

    /// Bring a hosted, unpaused torrent live: open storage, build the engine
    /// instance from the persisted have-set, register it with the demux, and
    /// start its dial swarm. A no-op without a network, for a paused torrent,
    /// or if already live.
    fn start_live(&mut self, info_hash: &[u8; 20]) -> io::Result<()> {
        let Some(network) = &self.network else {
            return Ok(());
        };
        let Some(hosted) = self.torrents.get_mut(info_hash) else {
            return Ok(());
        };
        // A scan in flight owns this torrent's have-set until it publishes.
        // Starting the engine now would hand it an empty one and re-download
        // data that is already on disk.
        if hosted.paused || hosted.live.is_some() || hosted.scanning {
            return Ok(());
        }
        let storage = Arc::new(Storage::create(&hosted.meta, &self.downloads_dir, false)?);
        let torrent = Torrent::new(
            &hosted.meta,
            storage,
            &hosted.have,
            hosted.mode(),
            network.peer_id,
        );
        // Before anything is dialed, so the first pick already knows what the
        // user asked for. Persisted priorities that never reached the engine
        // were a `clove priority` that reported success and changed nothing.
        torrent.set_piece_priorities(&hosted.meta.piece_priorities(&hosted.priorities));
        network.demux.register(&torrent);
        let swarm = Swarm::dial_only(
            Arc::clone(&torrent),
            network.dialer.clone(),
            network.swarm_config,
        );
        // Tracker announces are the swarm's peer feed; a trackerless torrent
        // relies on operator peers and PEX.
        let urls: Vec<String> = hosted.meta.trackers.iter().flatten().cloned().collect();
        let announcer = if urls.is_empty() {
            None
        } else {
            Some(Announcer::spawn(
                Arc::clone(&torrent),
                AnnounceTarget {
                    urls,
                    our_dest_b64: network.dest_b64.clone(),
                    piece_length: u64::from(hosted.meta.piece_length),
                    total_length: hosted.meta.total_length,
                },
                network.dialer.clone(),
                network.naming.clone(),
                AnnouncerConfig::default(),
            ))
        };
        let maintenance = torrent.spawn_maintenance(DEFAULT_MAINTENANCE_INTERVAL);
        hosted.live = Some(Live {
            torrent,
            swarm,
            announcer,
            _maintenance: maintenance,
            stats_base: (hosted.uploaded, hosted.downloaded),
            last_forced_announce: None,
        });
        Ok(())
    }

    /// Take a torrent offline: unregister from the demux, snapshot its
    /// progress, and signal its swarm to stop (without blocking on in-flight
    /// dials). Peers already attached drain on their own; full disconnect
    /// arrives with the peer-timeout work (R5).
    fn stop_live(&mut self, info_hash: &[u8; 20]) {
        let Some(hosted) = self.torrents.get_mut(info_hash) else {
            return;
        };
        let Some(live) = hosted.live.take() else {
            return;
        };
        if let Some(network) = &self.network {
            network.demux.unregister(info_hash);
        }
        // A clean stop is the one moment the two sets can legitimately meet:
        // flush, then claim exactly what the flush covered. Without this every
        // pause would leave up to a tick's worth to re-verify on resume.
        match live.torrent.sync_storage() {
            Ok(durable) => hosted.verified = durable,
            Err(e) => eprintln!("cloved: syncing {}: {e}", hex(info_hash)),
        }
        hosted.have = live.torrent.have();
        let (up, down) = live.torrent.stats();
        hosted.uploaded = live.stats_base.0.saturating_add(up);
        hosted.downloaded = live.stats_base.1.saturating_add(down);
        live.swarm.request_stop();
        if let Some(announcer) = &live.announcer {
            announcer.request_stop();
        }
        live.torrent.disconnect_all();
        // Graceful goodbye to the trackers, best-effort and detached — but
        // only when there is a working session to send it over.
        //
        // Without that condition this fires on every session teardown,
        // including the ones caused by a wedged session (PROTOCOL.i2p-bt
        // §2.12): each one spawns a detached thread holding a clone of the
        // dead session and opening a fresh naming-lookup socket to the SAM
        // bridge, for a goodbye that cannot be delivered. Measured live as a
        // socket count against the bridge that climbed steadily for the life
        // of a run — and a bridge at its connection ceiling refuses new
        // streams, which is what wedges the session in the first place. The
        // loop fed itself.
        if let Some(network) = &self.network
            && network.usable()
        {
            let urls: Vec<String> = hosted.meta.trackers.iter().flatten().cloned().collect();
            if !urls.is_empty() {
                clove_core::swarm::announce_stopped(
                    *info_hash,
                    network.peer_id,
                    AnnounceTarget {
                        urls,
                        our_dest_b64: network.dest_b64.clone(),
                        piece_length: u64::from(hosted.meta.piece_length),
                        total_length: hosted.meta.total_length,
                    },
                    network.dialer.clone(),
                    network.naming.clone(),
                    network.swarm_config.dial_timeout,
                );
            }
        }
    }

    /// Refresh each live torrent's progress and stats snapshot.
    fn refresh(&mut self) {
        for hosted in self.torrents.values_mut() {
            if let Some(live) = &hosted.live {
                hosted.have = live.torrent.have();
                let (up, down) = live.torrent.stats();
                hosted.uploaded = live.stats_base.0.saturating_add(up);
                hosted.downloaded = live.stats_base.1.saturating_add(down);
                hosted.peers = live.torrent.connected_peers().len();
                hosted.known_peers = live.torrent.known_peers().len();
                hosted.pex_peers = live.torrent.pex_learned();
                hosted.inbound_peers = live.torrent.inbound_peers();
                let (ok, failed, why) = live.torrent.announce_status();
                hosted.announces_ok = ok;
                hosted.announces_failed = failed;
                hosted.last_announce_error = why;
            } else {
                // No engine means no peers. Leaving the last live values in
                // place would show a paused torrent still holding eight peers.
                //
                // `pex_peers` is deliberately not cleared: it is a count of
                // what happened, not a description of the present, and a live
                // run that proved peer exchange works should not lose that
                // evidence the moment the operator pauses the torrent. It
                // resets on its own when a new engine starts, since the
                // engine's own counter starts at zero.
                hosted.peers = 0;
                hosted.known_peers = 0;
            }
        }
    }

    /// Snapshot every live torrent's progress and persist its resume record.
    /// Called periodically by the daemon and around lifecycle transitions.
    pub(crate) fn persist_progress(&mut self) {
        self.refresh();
        for (info_hash, hosted) in &mut self.torrents {
            let Some(live) = &hosted.live else {
                continue;
            };
            // Make the data durable before recording that it is. A failure is
            // not fatal — the resume file is still written, just with the older
            // verified set, so the next start re-checks what this tick could
            // not promise for.
            match live.torrent.sync_storage() {
                Ok(durable) => hosted.verified = durable,
                Err(e) => eprintln!("cloved: syncing {}: {e}", hex(info_hash)),
            }
            if let Err(e) =
                write_resume_file(&self.state_dir, info_hash, &hosted.resume(*info_hash))
            {
                eprintln!("cloved: persisting {}: {e}", hex(info_hash));
            }
        }
    }

    /// Hand a live torrent an operator-supplied peer to dial.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`], or [`ActionError::BadInput`] when the
    /// torrent is not running (paused, or no router yet).
    pub(crate) fn add_peer(
        &mut self,
        info_hash: &[u8; 20],
        peer: DestHash,
    ) -> Result<(), ActionError> {
        let hosted = self.torrents.get(info_hash).ok_or(ActionError::NotFound)?;
        let Some(live) = &hosted.live else {
            return Err(ActionError::BadInput(
                "torrent is not running (paused, or the router is not connected)",
            ));
        };
        live.torrent.add_peers(&[peer]);
        Ok(())
    }

    /// Number of hosted torrents.
    pub(crate) fn count(&self) -> usize {
        self.torrents.len()
    }

    /// Add a torrent from its `.torrent` bytes: parse, lay out and verify its
    /// storage, persist state, and register it. Returns the info-hash.
    ///
    /// # Errors
    ///
    /// [`AddError`] if the bytes do not parse, the torrent is already hosted,
    /// or persistence fails.
    pub(crate) fn add_torrent(&mut self, bytes: &[u8]) -> Result<([u8; 20], ScanJob), AddError> {
        let meta = MetaInfo::parse(bytes).map_err(AddError::Parse)?;
        let info_hash = meta.info_hash.0;
        if self.torrents.contains_key(&info_hash) {
            return Err(AddError::Duplicate);
        }
        let num_pieces = u32::try_from(meta.pieces.len()).unwrap_or(u32::MAX);
        let priorities = vec![1u8; meta.files.len()];

        // Registered with nothing yet, and marked as scanning: the initial pass
        // over whatever is already on disk is the caller's to run without the
        // lock, and the engine waits for it. A torrent re-added over a finished
        // download is exactly the case that used to hold the daemon still.
        let hosted = Hosted {
            meta: meta.clone(),
            have: Bitfield::empty(num_pieces),
            // Nothing claimed and nothing confirmed; the scan below sets both.
            verified: Bitfield::empty(num_pieces),
            priorities,
            uploaded: 0,
            downloaded: 0,
            peers: 0,
            known_peers: 0,
            pex_peers: 0,
            inbound_peers: 0,
            announces_ok: 0,
            announces_failed: 0,
            last_announce_error: None,
            paused: false,
            sequential: false,
            scanning: true,
            live: None,
        };
        let hex = hex(&info_hash);
        atomic_write(&self.state_dir.join(format!("{hex}.torrent")), bytes)
            .map_err(AddError::Io)?;
        self.write_resume(&info_hash, &hosted)
            .map_err(AddError::Io)?;
        self.torrents.insert(info_hash, hosted);
        Ok((
            info_hash,
            ScanJob {
                info_hash,
                meta,
                downloads_dir: self.downloads_dir.clone(),
            },
        ))
    }

    /// Add a magnet link: parse it, persist the URI, and queue it for
    /// metadata fetching (the caller spawns the fetch thread). Returns the
    /// info-hash.
    ///
    /// # Errors
    ///
    /// [`AddError`] on an unparseable magnet, a duplicate, or a filesystem
    /// error persisting the URI.
    pub(crate) fn add_magnet(&mut self, uri: &str) -> Result<[u8; 20], AddError> {
        let magnet = Magnet::parse(uri).map_err(AddError::Magnet)?;
        let info_hash = magnet.info_hash;
        if self.torrents.contains_key(&info_hash) || self.pending.contains_key(&info_hash) {
            return Err(AddError::Duplicate);
        }
        atomic_write(
            &self.state_dir.join(format!("{}.magnet", hex(&info_hash))),
            uri.as_bytes(),
        )
        .map_err(AddError::Io)?;
        self.pending.insert(
            info_hash,
            PendingMagnet {
                magnet,
                claimed: false,
                progress: FetchProgress::default(),
            },
        );
        Ok(info_hash)
    }

    /// Claim a pending magnet for fetching. `true` exactly once per entry, so
    /// callers spawn at most one fetch thread each.
    pub(crate) fn claim_fetch(&mut self, info_hash: &[u8; 20]) -> bool {
        match self.pending.get_mut(info_hash) {
            Some(pending) if !pending.claimed => {
                pending.claimed = true;
                true
            }
            _ => false,
        }
    }

    /// Every pending magnet's info-hash (for spawning fetchers at startup).
    pub(crate) fn pending_hashes(&self) -> Vec<[u8; 20]> {
        self.pending.keys().copied().collect()
    }

    /// Scan jobs for torrents that came back from disk needing re-verification
    /// — those whose resume file claimed pieces it could not confirm.
    ///
    /// They are already marked as scanning by [`load_one`](Registry::load_one),
    /// so nothing can start them in the meantime; the caller runs each job
    /// unlocked and reports through [`finish_scan`](Registry::finish_scan), as
    /// with any other scan. Normally empty, because a clean run leaves the two
    /// sets equal.
    pub(crate) fn pending_scans(&self) -> Vec<ScanJob> {
        self.torrents
            .iter()
            .filter(|(_, hosted)| hosted.scanning)
            .map(|(info_hash, hosted)| ScanJob {
                info_hash: *info_hash,
                meta: hosted.meta.clone(),
                downloads_dir: self.downloads_dir.clone(),
            })
            .collect()
    }

    /// Record what a metadata-fetch round managed, so `clove list` can show
    /// it. A no-op for a magnet that resolved or was removed mid-round.
    pub(crate) fn note_fetch_round(
        &mut self,
        info_hash: &[u8; 20],
        rounds: u32,
        round: &crate::FetchRound,
    ) {
        if let Some(pending) = self.pending.get_mut(info_hash) {
            pending.progress = FetchProgress {
                rounds,
                peers_known: round.peers_returned,
                peers_tried: round.peers_tried,
                trackers_ok: round.trackers_ok,
                trackers_failed: round.trackers_failed,
                last_error: round.last_error.clone(),
            };
        }
    }

    /// The context a fetch round needs, or `None` when the magnet is gone
    /// (fetch thread should exit) wrapped as the outer Option, and the inner
    /// `None` when there is simply no network yet (sleep and retry).
    #[allow(
        clippy::option_option,
        reason = "the two Nones mean different things: gone vs. not-yet"
    )]
    pub(crate) fn fetch_context(&self, info_hash: &[u8; 20]) -> Option<Option<FetchContext<D>>> {
        let pending = self.pending.get(info_hash)?;
        Some(self.network.as_ref().map(|network| FetchContext {
            trackers: pending.magnet.trackers.clone(),
            dialer: network.dialer.clone(),
            naming: network.naming.clone(),
            peer_id: network.peer_id,
            dest_b64: network.dest_b64.clone(),
        }))
    }

    /// Promote a fetched magnet: drop the pending entry and its URI file,
    /// then add the synthesized `.torrent` bytes through the normal path
    /// (persist, go live).
    ///
    /// # Errors
    ///
    /// [`AddError`] from the underlying [`add_torrent`](Registry::add_torrent).
    pub(crate) fn complete_magnet(
        &mut self,
        info_hash: &[u8; 20],
        torrent_bytes: &[u8],
    ) -> Result<ScanJob, AddError> {
        self.pending.remove(info_hash);
        let _ = fs::remove_file(self.state_dir.join(format!("{}.magnet", hex(info_hash))));
        self.add_torrent(torrent_bytes).map(|(_, job)| job)
    }

    /// Pause or resume a torrent, persisting the change.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`] or a filesystem error.
    pub(crate) fn set_paused(
        &mut self,
        info_hash: &[u8; 20],
        paused: bool,
    ) -> Result<(), ActionError> {
        {
            let hosted = self
                .torrents
                .get_mut(info_hash)
                .ok_or(ActionError::NotFound)?;
            if !paused && hosted.scanning {
                return Err(ActionError::BadInput(
                    "a verification is running for this torrent; it will start on its own when that finishes",
                ));
            }
            hosted.paused = paused;
        }
        if paused {
            self.stop_live(info_hash);
        } else {
            self.start_live(info_hash).map_err(ActionError::Io)?;
        }
        let hosted = self.torrents.get(info_hash).ok_or(ActionError::NotFound)?;
        let resume = hosted.resume(*info_hash);
        write_resume_file(&self.state_dir, info_hash, &resume).map_err(ActionError::Io)
    }

    /// Set per-file priorities (one byte per file, `0` skip / `1` normal /
    /// `2` high), persisting the change. Returns the number of files.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`], [`ActionError::BadInput`] if the count or a
    /// value is wrong, or a filesystem error.
    pub(crate) fn set_priorities(
        &mut self,
        info_hash: &[u8; 20],
        priorities: Vec<u8>,
    ) -> Result<usize, ActionError> {
        let hosted = self
            .torrents
            .get_mut(info_hash)
            .ok_or(ActionError::NotFound)?;
        if priorities.len() != hosted.meta.files.len() {
            return Err(ActionError::BadInput(
                "priority count must equal the file count",
            ));
        }
        if priorities.iter().any(|&p| p > 2) {
            return Err(ActionError::BadInput("priorities must be 0, 1, or 2"));
        }
        let count = priorities.len();
        hosted.priorities = priorities;
        // The engine first, then the disk. A live torrent that kept picking
        // pieces the user just told it to skip is the whole of this bug, and
        // it is not fixed by persisting the choice for next time.
        if let Some(live) = &hosted.live {
            live.torrent
                .set_piece_priorities(&hosted.meta.piece_priorities(&hosted.priorities));
        }
        let resume = hosted.resume(*info_hash);
        write_resume_file(&self.state_dir, info_hash, &resume).map_err(ActionError::Io)?;
        Ok(count)
    }

    /// Switch a torrent between rarest-first and sequential piece selection
    /// (SCOPE §3), persisting the choice. A live torrent picks up the change
    /// immediately; nothing in flight is cancelled.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`], or a filesystem error writing the resume
    /// file.
    pub(crate) fn set_sequential(
        &mut self,
        info_hash: &[u8; 20],
        sequential: bool,
    ) -> Result<(), ActionError> {
        let hosted = self
            .torrents
            .get_mut(info_hash)
            .ok_or(ActionError::NotFound)?;
        hosted.sequential = sequential;
        if let Some(live) = &hosted.live {
            live.torrent.set_mode(hosted.mode());
        }
        let resume = hosted.resume(*info_hash);
        write_resume_file(&self.state_dir, info_hash, &resume).map_err(ActionError::Io)
    }

    /// Force an immediate announce to every tracker (SCOPE §3's operator
    /// re-announce), bypassing the intervals trackers gave us.
    ///
    /// Rate-limited to one forced announce per
    /// [`MIN_ANNOUNCE_INTERVAL`](clove_core::tracker::MIN_ANNOUNCE_INTERVAL):
    /// the command exists for an operator who suspects a stale peer set, not
    /// as something to put in a loop.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`], or [`ActionError::BadInput`] when the
    /// torrent has no running announcer or was asked too recently.
    pub(crate) fn announce_now(&mut self, info_hash: &[u8; 20]) -> Result<(), ActionError> {
        let hosted = self
            .torrents
            .get_mut(info_hash)
            .ok_or(ActionError::NotFound)?;
        if hosted.paused {
            return Err(ActionError::BadInput("torrent is paused"));
        }
        let Some(live) = &mut hosted.live else {
            return Err(ActionError::BadInput(
                "torrent is not running yet (waiting for the router)",
            ));
        };
        let Some(announcer) = &live.announcer else {
            return Err(ActionError::BadInput(
                "torrent has no I2P trackers to announce to",
            ));
        };
        if let Some(last) = live.last_forced_announce
            && last.elapsed() < MIN_ANNOUNCE_INTERVAL
        {
            return Err(ActionError::BadInput(
                "an announce was already forced in the last minute",
            ));
        }
        announcer.announce_now();
        live.last_forced_announce = Some(Instant::now());
        Ok(())
    }

    /// Claim a torrent for re-verification and hand back the pass to run.
    ///
    /// The caller runs [`ScanJob::run`] with the registry unlocked and then
    /// reports back through [`finish_scan`](Registry::finish_scan) — which it
    /// must do even on failure, or the torrent stays marked as scanning and
    /// never starts again.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`], or [`ActionError::BadInput`] if the torrent is
    /// running or already being scanned.
    pub(crate) fn begin_verify(&mut self, info_hash: &[u8; 20]) -> Result<ScanJob, ActionError> {
        let downloads_dir = self.downloads_dir.clone();
        let hosted = self
            .torrents
            .get_mut(info_hash)
            .ok_or(ActionError::NotFound)?;
        if hosted.live.is_some() {
            return Err(ActionError::BadInput(
                "pause the torrent before verifying (it is actively writing)",
            ));
        }
        if hosted.scanning {
            return Err(ActionError::BadInput(
                "this torrent is already being verified",
            ));
        }
        hosted.scanning = true;
        Ok(ScanJob {
            info_hash: *info_hash,
            meta: hosted.meta.clone(),
            downloads_dir,
        })
    }

    /// Publish the result of a [`ScanJob`]: adopt the have-set, persist it, and
    /// bring the torrent live if it should be. Returns the verified piece count.
    ///
    /// Re-validates on the way in, because the lock was released for the
    /// duration: the torrent may have been removed, and a `scanned` error may be
    /// exactly why (its files went with it).
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`] if the torrent was removed while the pass ran,
    /// or the pass's own filesystem error.
    pub(crate) fn finish_scan(
        &mut self,
        job: &ScanJob,
        scanned: io::Result<Bitfield>,
    ) -> Result<u32, ActionError> {
        let info_hash = job.info_hash;
        let Some(hosted) = self.torrents.get_mut(&info_hash) else {
            // Removed while we hashed: nothing to publish, nothing to clear.
            return Err(ActionError::NotFound);
        };
        hosted.scanning = false;
        let have = scanned.map_err(ActionError::Io)?;
        // A scan is the strongest claim available: every bit in it came from
        // hashing bytes that were read back off the disk a moment ago. This is
        // the one place the two sets are equal by construction rather than by
        // an fsync we have to trust.
        hosted.verified = have.clone();
        hosted.have = have;
        let count = hosted.have.count();
        let resume = hosted.resume(info_hash);
        write_resume_file(&self.state_dir, &info_hash, &resume).map_err(ActionError::Io)?;
        // Nothing could start it while the scan was in flight, so this is where
        // an added or freshly-verified torrent goes live.
        self.start_live(&info_hash).map_err(ActionError::Io)?;
        Ok(count)
    }

    /// A torrent's full detail as JSON (files, priorities, trackers), or `None`
    /// if it is not hosted. Live progress is refreshed first.
    pub(crate) fn detail(&mut self, info_hash: &[u8; 20]) -> Option<Value> {
        self.refresh();
        self.torrents.get(info_hash).map(Hosted::to_detail_json)
    }

    /// Persist `hosted`'s resume record.
    fn write_resume(&self, info_hash: &[u8; 20], hosted: &Hosted) -> io::Result<()> {
        write_resume_file(&self.state_dir, info_hash, &hosted.resume(*info_hash))
    }

    /// Remove a torrent, deleting its state files and — if `delete_data` — its
    /// downloaded files.
    ///
    /// # Errors
    ///
    /// [`RemoveError::NotFound`] if no such torrent is hosted.
    pub(crate) fn remove(
        &mut self,
        info_hash: &[u8; 20],
        delete_data: bool,
    ) -> Result<(), RemoveError> {
        if self.pending.remove(info_hash).is_some() {
            // A magnet still fetching: drop its URI file; the fetch thread
            // notices the entry is gone and exits.
            return remove_file_ok(&self.state_dir.join(format!("{}.magnet", hex(info_hash))));
        }
        if !self.torrents.contains_key(info_hash) {
            return Err(RemoveError::NotFound);
        }
        // Take it offline first so nothing is writing while files disappear.
        self.stop_live(info_hash);
        let hosted = self.torrents.get(info_hash).ok_or(RemoveError::NotFound)?;
        let hex = hex(info_hash);
        // Delete the state files first and surface a real failure: if they
        // survive, the torrent would reappear on restart. The in-memory entry
        // is dropped only once the on-disk state is gone, so a failure here
        // leaves a consistent registry.
        remove_file_ok(&self.state_dir.join(format!("{hex}.torrent")))?;
        remove_file_ok(&self.state_dir.join(format!("{hex}.resume")))?;
        if delete_data {
            for file in &hosted.meta.files {
                let mut path = self.downloads_dir.clone();
                for component in &file.path {
                    path.push(component);
                }
                let _ = fs::remove_file(&path); // data deletion is best-effort
            }
            // Best-effort: drop the torrent's now-empty top directory.
            let _ = fs::remove_dir(self.downloads_dir.join(&hosted.meta.name));
        }
        self.torrents.remove(info_hash);
        Ok(())
    }

    /// The torrents as a JSON array, one object each, ordered by info-hash.
    /// Live progress is refreshed first.
    pub(crate) fn list(&mut self) -> Value {
        self.refresh();
        let mut items: Vec<Value> = self.torrents.values().map(Hosted::to_json).collect();
        for (info_hash, pending) in &self.pending {
            let name = pending
                .magnet
                .display_name
                .clone()
                .unwrap_or_else(|| hex(info_hash));
            let p = &pending.progress;
            let mut entry = vec![
                ("info_hash".to_owned(), Value::from(hex(info_hash))),
                ("name".to_owned(), Value::from(name)),
                ("state".to_owned(), Value::from("fetching-metadata")),
                ("progress".to_owned(), Value::Float(0.0)),
                ("fetch_rounds".to_owned(), Value::UInt(u64::from(p.rounds))),
                (
                    "trackers_ok".to_owned(),
                    Value::UInt(u64::try_from(p.trackers_ok).unwrap_or(u64::MAX)),
                ),
                (
                    "trackers_failed".to_owned(),
                    Value::UInt(u64::try_from(p.trackers_failed).unwrap_or(u64::MAX)),
                ),
                (
                    "known_peers".to_owned(),
                    Value::UInt(u64::try_from(p.peers_known).unwrap_or(u64::MAX)),
                ),
                (
                    "peers_tried".to_owned(),
                    Value::UInt(u64::try_from(p.peers_tried).unwrap_or(u64::MAX)),
                ),
            ];
            if let Some(err) = &p.last_error {
                entry.push(("last_error".to_owned(), Value::from(err.clone())));
            }
            items.push(Value::Object(entry));
        }
        Value::Array(items)
    }

    /// Load every previously added torrent from the state directory. A file
    /// that cannot be loaded is logged and skipped, never fatal.
    fn load_all(&mut self) {
        let Ok(entries) = fs::read_dir(&self.state_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("torrent") => {
                    if let Err(e) = self.load_one(&path) {
                        eprintln!("cloved: skipping {}: {e}", path.display());
                    }
                }
                Some("magnet") => {
                    if let Err(e) = self.load_magnet(&path) {
                        eprintln!("cloved: skipping {}: {e}", path.display());
                    }
                }
                _ => {}
            }
        }
    }

    fn load_magnet(&mut self, path: &Path) -> Result<(), String> {
        let uri = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let magnet = Magnet::parse(uri.trim()).map_err(|e| e.to_string())?;
        self.pending.insert(
            magnet.info_hash,
            PendingMagnet {
                magnet,
                claimed: false,
                progress: FetchProgress::default(),
            },
        );
        Ok(())
    }

    fn load_one(&mut self, torrent_path: &Path) -> Result<(), String> {
        let bytes = fs::read(torrent_path).map_err(|e| e.to_string())?;
        let meta = MetaInfo::parse(&bytes).map_err(|e| e.to_string())?;
        let info_hash = meta.info_hash.0;
        let hex = hex(&info_hash);
        let resume_bytes =
            fs::read(self.state_dir.join(format!("{hex}.resume"))).map_err(|e| e.to_string())?;
        let resume = Resume::decode(&resume_bytes).map_err(|e| e.to_string())?;
        if resume.info_hash != info_hash {
            return Err("resume file does not match the .torrent".to_owned());
        }
        // The piece count has to agree too, or every bitfield in the resume
        // file is measured against a different torrent than the one on disk:
        // progress, completeness and the have-set handed to the engine all
        // come out wrong, and quietly.
        if usize::try_from(resume.num_pieces).unwrap_or(usize::MAX) != meta.pieces.len() {
            return Err(format!(
                "resume file says {} pieces, the .torrent has {}",
                resume.num_pieces,
                meta.pieces.len()
            ));
        }
        // Same reasoning as the piece count: a priorities vector measured
        // against a different file list decides the wrong files are skipped,
        // and does it silently.
        if resume.priorities.len() != meta.files.len() {
            return Err(format!(
                "resume file has {} priorities, the .torrent has {} file(s)",
                resume.priorities.len(),
                meta.files.len()
            ));
        }
        let have = Bitfield::from_bytes(&resume.have, resume.num_pieces)
            .map_err(|_| "resume have-bitfield is inconsistent".to_owned())?;
        let verified = Bitfield::from_bytes(&resume.verified, resume.num_pieces)
            .map_err(|_| "resume verified-bitfield is inconsistent".to_owned())?;
        // Only what `verified` covers is publishable. `have` is what we
        // believed in memory; `verified` is what we last confirmed was on disk
        // and correct, and the gap between them is exactly what a crash, a
        // truncated write or a bad sector leaves behind. Publishing `have`
        // unread — which is what this did — meant announcing, serving and
        // counting as complete pieces nothing had looked at since.
        //
        // The difference is normally empty (the persist loop fsyncs and then
        // records what it made durable), so this costs a scan only when
        // something actually went wrong.
        let rescan = have != verified;
        if rescan {
            eprintln!(
                "cloved: {hex}: {} piece(s) were held but not confirmed on disk; verifying",
                have.count().saturating_sub(verified.count())
            );
        }
        self.torrents.insert(
            info_hash,
            Hosted {
                have: verified.clone(),
                verified,
                priorities: resume.priorities,
                uploaded: resume.uploaded,
                downloaded: resume.downloaded,
                // Peer counts are live facts, not persisted ones: a torrent
                // loaded from disk has no engine and therefore no peers until
                // `refresh` sees one.
                peers: 0,
                known_peers: 0,
                pex_peers: 0,
                inbound_peers: 0,
                announces_ok: 0,
                announces_failed: 0,
                last_announce_error: None,
                paused: resume.paused,
                sequential: resume.sequential,
                // Claimed here so nothing can start the torrent before the scan
                // publishes; `pending_scans` hands out the job at startup.
                scanning: rescan,
                meta,
                live: None,
            },
        );
        Ok(())
    }
}

impl Hosted {
    /// The picker mode this torrent's engine should run with.
    fn mode(&self) -> Mode {
        if self.sequential {
            Mode::Sequential
        } else {
            Mode::RarestFirst
        }
    }

    /// The state string shown in listings.
    /// Pieces this torrent is asking for, and how many of those it holds.
    ///
    /// Everything user-facing counts against this rather than against the whole
    /// torrent: a download with files set to skip is finished when the files it
    /// was told to fetch are, and reporting it at 60% for ever — never
    /// "seeding", never done — would be describing a job it was told not to do.
    fn wanted_and_held(&self) -> (u32, u32) {
        let priorities = self.meta.piece_priorities(&self.priorities);
        let mut wanted = 0;
        let mut held = 0;
        for index in 0..self.have.len() {
            if priorities.get(index as usize).copied().unwrap_or(1) == 0 {
                continue;
            }
            wanted += 1;
            if self.have.has(index) {
                held += 1;
            }
        }
        (wanted, held)
    }

    fn state(&self) -> &'static str {
        let (wanted, held) = self.wanted_and_held();
        let complete = held == wanted;
        if self.scanning {
            "verifying"
        } else if self.paused {
            "paused"
        } else if self.live.is_some() {
            if complete { "seeding" } else { "downloading" }
        } else if complete {
            "complete"
        } else {
            // Unpaused but no engine: the router is not connected yet.
            "waiting-for-router"
        }
    }

    fn progress(&self) -> f64 {
        let (wanted, held) = self.wanted_and_held();
        if wanted == 0 {
            // Nothing asked for is nothing outstanding. Reporting 0% for a
            // torrent with every file skipped would be the one number that
            // never moves however long it runs.
            1.0
        } else {
            f64::from(held) / f64::from(wanted)
        }
    }

    /// The summary object shown in `list`.
    fn to_json(&self) -> Value {
        let priorities = self
            .priorities
            .iter()
            .map(|&p| Value::UInt(u64::from(p)))
            .collect();
        Value::Object(vec![
            (
                "info_hash".to_owned(),
                Value::from(hex(&self.meta.info_hash.0)),
            ),
            ("name".to_owned(), Value::from(self.meta.name.clone())),
            ("size".to_owned(), Value::UInt(self.meta.total_length)),
            ("pieces".to_owned(), Value::UInt(u64::from(self.have.len()))),
            ("have".to_owned(), Value::UInt(u64::from(self.have.count()))),
            ("progress".to_owned(), Value::Float(self.progress())),
            ("uploaded".to_owned(), Value::UInt(self.uploaded)),
            ("downloaded".to_owned(), Value::UInt(self.downloaded)),
            (
                "peers".to_owned(),
                Value::UInt(u64::try_from(self.peers).unwrap_or(u64::MAX)),
            ),
            ("state".to_owned(), Value::from(self.state())),
            ("priorities".to_owned(), Value::Array(priorities)),
        ])
    }

    /// The full detail object shown in `show`, adding per-file and tracker info.
    fn to_detail_json(&self) -> Value {
        let files = self
            .meta
            .files
            .iter()
            .enumerate()
            .map(|(i, file)| {
                let priority = self.priorities.get(i).copied().unwrap_or(1);
                Value::Object(vec![
                    ("path".to_owned(), Value::from(file.path.join("/"))),
                    ("length".to_owned(), Value::UInt(file.length)),
                    ("priority".to_owned(), Value::UInt(u64::from(priority))),
                ])
            })
            .collect();
        let trackers = self
            .meta
            .trackers
            .iter()
            .flatten()
            .map(|url| Value::from(url.clone()))
            .collect();
        let mut fields = vec![
            (
                "info_hash".to_owned(),
                Value::from(hex(&self.meta.info_hash.0)),
            ),
            ("name".to_owned(), Value::from(self.meta.name.clone())),
            ("size".to_owned(), Value::UInt(self.meta.total_length)),
            ("pieces".to_owned(), Value::UInt(u64::from(self.have.len()))),
            ("have".to_owned(), Value::UInt(u64::from(self.have.count()))),
            ("progress".to_owned(), Value::Float(self.progress())),
            ("uploaded".to_owned(), Value::UInt(self.uploaded)),
            ("downloaded".to_owned(), Value::UInt(self.downloaded)),
            (
                "peers".to_owned(),
                Value::UInt(u64::try_from(self.peers).unwrap_or(u64::MAX)),
            ),
            (
                "known_peers".to_owned(),
                Value::UInt(u64::try_from(self.known_peers).unwrap_or(u64::MAX)),
            ),
            ("pex_peers".to_owned(), Value::UInt(self.pex_peers)),
            ("inbound_peers".to_owned(), Value::UInt(self.inbound_peers)),
            (
                "announces_ok".to_owned(),
                Value::UInt(u64::from(self.announces_ok)),
            ),
            (
                "announces_failed".to_owned(),
                Value::UInt(u64::from(self.announces_failed)),
            ),
            ("state".to_owned(), Value::from(self.state())),
            ("sequential".to_owned(), Value::Bool(self.sequential)),
            ("private".to_owned(), Value::Bool(self.meta.private)),
            ("files".to_owned(), Value::Array(files)),
            ("trackers".to_owned(), Value::Array(trackers)),
        ];
        // Only present when there is one: an absent key reads as "nothing has
        // gone wrong", which an empty string does not.
        if let Some(why) = &self.last_announce_error {
            fields.push(("last_announce_error".to_owned(), Value::from(why.clone())));
        }
        Value::Object(fields)
    }
}

/// Remove a file, treating "already gone" as success but surfacing any other
/// error (e.g. a permission problem that would leave stale state behind).
fn remove_file_ok(path: &Path) -> Result<(), RemoveError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(RemoveError::Io(e)),
    }
}

/// Write a resume record atomically to `state/<info-hash>.resume`.
fn write_resume_file(state_dir: &Path, info_hash: &[u8; 20], resume: &Resume) -> io::Result<()> {
    atomic_write(
        &state_dir.join(format!("{}.resume", hex(info_hash))),
        &resume.encode(),
    )
}

/// Parse a 40-char lowercase-hex info-hash into 20 bytes.
pub(crate) fn parse_info_hash(text: &str) -> Option<[u8; 20]> {
    if text.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    let bytes = text.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_digit(bytes[i * 2])?;
        let lo = hex_digit(bytes[i * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Lowercase-hex encode.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[(b >> 4) as usize]));
        out.push(char::from(HEX[(b & 0x0f) as usize]));
    }
    out
}

/// Write `bytes` to `path` atomically: a sibling temp file, fsynced, then
/// renamed over `path` (atomic on the same filesystem).
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // The rename is atomic, but it only survives a power cut once the
    // directory entry itself is on the disk. Best-effort: filesystems that
    // refuse to fsync a directory leave us exactly where we were.
    if let Some(dir) = path.parent() {
        let _ = fs::File::open(dir).and_then(|handle| handle.sync_all());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clove_core::bencode::{self, Value as Ben};
    use clove_core::wire::BLOCK_LEN;
    use i2pnet::mock::{MockDialer, MockNet};
    use sha1::{Digest, Sha1};
    use std::collections::BTreeMap as Map;
    use std::io::Read as _;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static C: AtomicU32 = AtomicU32::new(0);
            let n = C.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("clove-registry-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Deterministic multi-piece content and a real single-file `.torrent`.
    fn fixture(name: &str) -> (Vec<u8>, Vec<u8>) {
        let content: Vec<u8> = (0..(3 * BLOCK_LEN + 100))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let pieces: Vec<u8> = content
            .chunks(BLOCK_LEN as usize)
            .flat_map(|c| <[u8; 20]>::from(Sha1::digest(c)))
            .collect();
        let mut info = Map::new();
        info.insert(b"name".to_vec(), Ben::Bytes(name.as_bytes().to_vec()));
        info.insert(b"piece length".to_vec(), Ben::Int(i64::from(BLOCK_LEN)));
        info.insert(b"pieces".to_vec(), Ben::Bytes(pieces));
        info.insert(
            b"length".to_vec(),
            Ben::Int(i64::try_from(content.len()).unwrap()),
        );
        let mut root = Map::new();
        root.insert(b"info".to_vec(), Ben::Dict(info));
        (content, bencode::encode(&Ben::Dict(root)))
    }

    fn quick_swarm() -> SwarmConfig {
        SwarmConfig {
            dial_timeout: Duration::from_millis(200),
            sweep_interval: Duration::from_millis(50),
            retry_backoff: Duration::from_millis(100),
            max_peers: 8,
            dial_concurrency: 4,
        }
    }

    /// A complete core-level seeder serving `bytes` behind a demux on `net`.
    fn spawn_seeder(net: &MockNet, content: &[u8], bytes: &[u8]) -> (DestHash, TempDir) {
        let meta = MetaInfo::parse(bytes).unwrap();
        let dir = TempDir::new("seed");
        let storage = Arc::new(Storage::create(&meta, &dir.0, false).unwrap());
        for p in 0..storage.num_pieces() {
            let start = p as usize * BLOCK_LEN as usize;
            let end = (start + storage.piece_len(p) as usize).min(content.len());
            storage.write_block(p, 0, &content[start..end]).unwrap();
        }
        let have = storage.verify_all().unwrap();
        assert!(have.is_full());
        let torrent = Torrent::new(
            &meta,
            storage,
            &have,
            Mode::RarestFirst,
            *b"-CV0001-seedseedseed",
        );
        let ep = net.endpoint();
        let dest = ep.dest();
        let demux = InboundDemux::new(8);
        demux.register(&torrent);
        let _accept = demux.run(ep);
        // Keep the torrent alive for the test's duration.
        std::mem::forget(torrent);
        (dest, dir)
    }

    /// Add a torrent the way the daemon does: register it, run its initial scan
    /// with the registry not held, publish the result.
    ///
    /// The two halves exist because that scan hashes whatever is already on disk
    /// and must not happen under the lock; a test that only called the first
    /// half would leave the torrent stuck in "verifying" and never started.
    fn add_and_scan(registry: &mut Registry<MockDialer>, bytes: &[u8]) -> [u8; 20] {
        let (info_hash, job) = registry.add_torrent(bytes).expect("add");
        let scanned = job.run();
        registry
            .finish_scan(&job, scanned)
            .expect("publish the scan");
        info_hash
    }

    fn first_progress(registry: &mut Registry<MockDialer>) -> f64 {
        registry
            .list()
            .as_array()
            .and_then(|items| items.first().cloned())
            .and_then(|item| item.get("progress").and_then(Value::as_f64))
            .unwrap_or(0.0)
    }

    #[test]
    fn engine_downloads_and_persists_over_the_mock() {
        let net = MockNet::new();
        let (content, bytes) = fixture("engine-demo");
        let (seed_dest, _seed_dir) = spawn_seeder(&net, &content, &bytes);

        let data = TempDir::new("data");
        let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
        let leech_ep = net.endpoint();
        registry.attach_network(
            leech_ep.dialer(),
            InboundDemux::new(8),
            *b"-CV0001-leechleechle",
            quick_swarm(),
            "leecher-b64".to_owned(),
        );

        let info_hash = add_and_scan(&mut registry, &bytes);
        registry.add_peer(&info_hash, seed_dest).unwrap();

        // Poll the registry's own view until the download completes.
        let deadline = Instant::now() + Duration::from_secs(20);
        while first_progress(&mut registry) < 1.0 {
            assert!(Instant::now() < deadline, "download did not complete");
            std::thread::sleep(Duration::from_millis(50));
        }
        registry.persist_progress();
        drop(registry);

        // A fresh registry (daemon restart) sees the completed state from
        // the persisted resume file alone.
        let mut reopened = Registry::<MockDialer>::open(&data.0).unwrap();
        assert_eq!(reopened.count(), 1);
        assert!((first_progress(&mut reopened) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pause_takes_the_engine_offline_and_resume_restores_it() {
        let net = MockNet::new();
        let (_content, bytes) = fixture("pause-demo");

        let data = TempDir::new("data");
        let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
        let ep = net.endpoint();
        registry.attach_network(
            ep.dialer(),
            InboundDemux::new(8),
            *b"-CV0001-pausepausepa",
            quick_swarm(),
            "pause-b64".to_owned(),
        );
        let info_hash = add_and_scan(&mut registry, &bytes);

        // Live: an operator peer is accepted.
        registry.add_peer(&info_hash, DestHash([0xEE; 32])).unwrap();

        registry.set_paused(&info_hash, true).unwrap();
        assert!(
            registry.add_peer(&info_hash, DestHash([0xEE; 32])).is_err(),
            "paused torrent must not accept peers"
        );

        registry.set_paused(&info_hash, false).unwrap();
        registry.add_peer(&info_hash, DestHash([0xEE; 32])).unwrap();
    }

    #[test]
    fn without_a_network_torrents_wait_for_the_router() {
        let (_content, bytes) = fixture("waiting-demo");
        let data = TempDir::new("data");
        let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
        let info_hash = add_and_scan(&mut registry, &bytes);
        let state = registry
            .list()
            .as_array()
            .and_then(|items| items.first().cloned())
            .and_then(|item| item.get("state").and_then(|s| s.as_str().map(String::from)));
        assert_eq!(state.as_deref(), Some("waiting-for-router"));
        assert!(registry.add_peer(&info_hash, DestHash([0xEE; 32])).is_err());
    }

    #[test]
    fn sequential_mode_persists_across_a_restart() {
        let (_content, bytes) = fixture("sequential-demo");
        let data = TempDir::new("data");
        let info_hash = {
            let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
            let info_hash = add_and_scan(&mut registry, &bytes);
            // Rarest-first is the default; nothing is claimed until asked.
            assert_eq!(sequential_flag(&mut registry, &info_hash), Some(false));
            registry.set_sequential(&info_hash, true).unwrap();
            assert_eq!(sequential_flag(&mut registry, &info_hash), Some(true));
            info_hash
        };
        // A fresh registry over the same data dir reads the flag back out of
        // the resume file — the point of putting it in the format at all.
        let mut reopened = Registry::<MockDialer>::open(&data.0).unwrap();
        assert_eq!(sequential_flag(&mut reopened, &info_hash), Some(true));
        reopened.set_sequential(&info_hash, false).unwrap();
        assert_eq!(sequential_flag(&mut reopened, &info_hash), Some(false));
    }

    #[test]
    fn announce_now_refuses_a_torrent_that_is_not_running() {
        let (_content, bytes) = fixture("announce-demo");
        let data = TempDir::new("data");
        let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
        let info_hash = add_and_scan(&mut registry, &bytes);
        // No router yet: the error names that, rather than pretending to
        // announce into a void.
        assert!(matches!(
            registry.announce_now(&info_hash),
            Err(ActionError::BadInput(_))
        ));
        registry.set_paused(&info_hash, true).unwrap();
        assert!(matches!(
            registry.announce_now(&info_hash),
            Err(ActionError::BadInput(_))
        ));
        assert!(matches!(
            registry.announce_now(&[0xAB; 20]),
            Err(ActionError::NotFound)
        ));
    }

    fn sequential_flag(registry: &mut Registry<MockDialer>, info_hash: &[u8; 20]) -> Option<bool> {
        registry
            .detail(info_hash)?
            .get("sequential")
            .and_then(Value::as_bool)
    }

    /// The defect the first live swarm run walked into: a magnet whose
    /// tracker name will not resolve produced no output whatsoever. Nine
    /// minutes of `fetching-metadata`, no log line, nothing in `clove list`
    /// beyond the word itself — so "the name is unknown to this router",
    /// "the tracker returned nothing" and "peers were dialed and none served"
    /// were one indistinguishable state.
    ///
    /// A round that fails must say which stage failed and why.
    #[test]
    fn a_metadata_fetch_that_fails_says_which_stage_and_why() {
        let net = MockNet::new();
        let (_content, bytes) = fixture("unresolvable-demo");
        let meta = MetaInfo::parse(&bytes).unwrap();
        let info_hash = meta.info_hash.0;

        let data = TempDir::new("magnet-unresolvable");
        let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
        let ep = net.endpoint();
        registry.attach_network(
            ep.dialer(),
            InboundDemux::new(8),
            *b"-CV0001-magnetmagnet",
            quick_swarm(),
            "magnet-b64".to_owned(),
        );

        // The mock network has no name registered for this tracker, which is
        // what an address book that has never heard of a host looks like.
        let uri = format!(
            "magnet:?xt=urn:btih:{}&dn=nope&tr=http%3A%2F%2Fnobody.i2p%2Fannounce",
            hex(&info_hash)
        );
        registry.add_magnet(&uri).unwrap();
        let ctx = registry.fetch_context(&info_hash).unwrap().unwrap();
        let (found, round) = crate::try_fetch_round(&ctx, info_hash, true);
        assert!(
            found.is_none(),
            "an unresolvable tracker yields no metadata"
        );
        registry.note_fetch_round(&info_hash, 1, &round);

        let error = round.last_error.expect("a failed round records its reason");
        assert!(
            error.contains("resolving tracker") && error.contains("nobody.i2p"),
            "the reason must name the stage and the host, got: {error}"
        );

        // And it reaches the operator without reading the daemon's stderr.
        let listed = registry.list();
        let entry = listed
            .as_array()
            .and_then(|items| items.first().cloned())
            .expect("the pending magnet is listed");
        assert_eq!(
            entry.get("state").and_then(Value::as_str),
            Some("fetching-metadata")
        );
        assert_eq!(entry.get("fetch_rounds").and_then(Value::as_u64), Some(1));
        assert_eq!(
            entry.get("trackers_failed").and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(entry.get("known_peers").and_then(Value::as_u64), Some(0));
        assert!(
            entry
                .get("last_error")
                .and_then(Value::as_str)
                .is_some_and(|e| e.contains("nobody.i2p")),
            "clove list carries the reason: {entry:?}"
        );
    }

    #[test]
    fn magnet_fetches_metadata_and_becomes_a_live_torrent() {
        let net = MockNet::new();
        let (content, bytes) = fixture("magnet-demo");
        let meta = MetaInfo::parse(&bytes).unwrap();
        let info_hash = meta.info_hash.0;
        let (seed_dest, _seed_dir) = spawn_seeder(&net, &content, &bytes);

        // A mock tracker that always returns the seeder.
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
                let response = clove_core::http::Response::new(200, "text/plain", body);
                let _ = std::io::Write::write_all(&mut stream, &response.encode());
            }
        });

        let data = TempDir::new("magnet-data");
        let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
        let ep = net.endpoint();
        registry.attach_network(
            ep.dialer(),
            InboundDemux::new(8),
            *b"-CV0001-magnetmagnet",
            quick_swarm(),
            "magnet-b64".to_owned(),
        );

        let uri = format!(
            "magnet:?xt=urn:btih:{}&dn=magnet-demo&tr=http%3A%2F%2Ftracker.i2p%2Fannounce",
            hex(&info_hash)
        );
        assert_eq!(registry.add_magnet(&uri).unwrap(), info_hash);
        assert!(registry.claim_fetch(&info_hash));
        assert!(!registry.claim_fetch(&info_hash), "claim is once-only");

        // Drive the fetch loop by hand (the daemon thread does the same).
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut rounds = 0;
        loop {
            assert!(Instant::now() < deadline, "metadata fetch did not finish");
            let ctx = registry.fetch_context(&info_hash).unwrap().unwrap();
            rounds += 1;
            let (found, round) = crate::try_fetch_round(&ctx, info_hash, true);
            registry.note_fetch_round(&info_hash, rounds, &round);
            // A round that has not resolved the magnet yet must still have
            // said what it tried, since that is the only thing an operator
            // watching a stalled fetch has to go on.
            let listed = registry.list();
            let pending = listed
                .as_array()
                .and_then(|items| items.iter().find(|i| i.get("state").is_some()))
                .expect("the pending magnet is listed");
            if found.is_none() {
                assert_eq!(
                    pending.get("state").and_then(Value::as_str),
                    Some("fetching-metadata")
                );
                assert!(
                    pending.get("fetch_rounds").is_some(),
                    "a pending magnet reports how many rounds have run"
                );
            }
            if let Some(bytes) = found {
                // Same two steps as the daemon's fetch thread: promote, then run
                // and publish the initial scan. Skipping the second would leave
                // the torrent marked as verifying and it would never start.
                let job = registry.complete_magnet(&info_hash, &bytes).unwrap();
                let scanned = job.run();
                registry.finish_scan(&job, scanned).unwrap();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Promoted: pending gone, fetch_context reports it.
        assert!(registry.fetch_context(&info_hash).is_none());

        // And the promoted torrent downloads to completion.
        let deadline = Instant::now() + Duration::from_secs(20);
        while first_progress(&mut registry) < 1.0 {
            assert!(
                Instant::now() < deadline,
                "promoted torrent did not download"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_resume_file_for_a_different_torrent_is_skipped() {
        let (_content, bytes) = fixture("mismatch-demo");
        let data = TempDir::new("mismatch");
        let info_hash = {
            let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
            add_and_scan(&mut registry, &bytes)
        };
        let resume_path = data.0.join(format!("state/{}.resume", hex(&info_hash)));

        // Reopening as-is finds it.
        assert_eq!(Registry::<MockDialer>::open(&data.0).unwrap().count(), 1);

        // Now claim a different piece count. Every bitfield in the file is
        // sized against that number, so the entry has to be refused rather
        // than loaded and measured against the wrong torrent.
        let good = Resume::decode(&fs::read(&resume_path).unwrap()).unwrap();
        let mut bad = good.clone();
        bad.num_pieces = good.num_pieces + 1;
        bad.have = vec![0u8; clove_core::resume::bitfield_len(bad.num_pieces)];
        bad.verified = bad.have.clone();
        fs::write(&resume_path, bad.encode()).unwrap();
        assert_eq!(
            Registry::<MockDialer>::open(&data.0).unwrap().count(),
            0,
            "a resume file describing a different torrent was loaded anyway"
        );

        // And the good one still loads, so the check is about the mismatch and
        // not about rejecting resume files in general.
        fs::write(&resume_path, good.encode()).unwrap();
        assert_eq!(Registry::<MockDialer>::open(&data.0).unwrap().count(), 1);
    }

    /// A resume file that claims pieces it cannot vouch for does not get to
    /// hand them to the engine.
    ///
    /// `have` is what clove believed in memory; `verified` is what it last
    /// confirmed was on disk. Loading published `have` and never read
    /// `verified` at all, so a file edited to claim everything — or one written
    /// before a crash, or one whose data rotted since — came back as a torrent
    /// that announced those pieces, served them on request, and called itself
    /// complete, without anything having hashed a byte.
    #[test]
    fn a_resume_file_cannot_claim_pieces_the_disk_does_not_back() {
        let (_content, bytes) = fixture("forged-demo");
        let data = TempDir::new("forged");
        let info_hash = {
            let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
            add_and_scan(&mut registry, &bytes)
        };
        let resume_path = data.0.join(format!("state/{}.resume", hex(&info_hash)));

        // Nothing is on disk, so the honest state is "no pieces".
        let honest = Resume::decode(&fs::read(&resume_path).unwrap()).unwrap();
        assert_eq!(
            honest.have.iter().copied().max(),
            Some(0),
            "fixture holds nothing"
        );

        // Forge `have` into "I hold everything", keeping `verified` honest —
        // the shape of a crash, and of a hand-edited state file.
        let mut forged = honest.clone();
        let full = {
            let mut bits = vec![0u8; clove_core::resume::bitfield_len(honest.num_pieces)];
            for piece in 0..honest.num_pieces as usize {
                bits[piece / 8] |= 0x80 >> (piece % 8);
            }
            bits
        };
        forged.have = full;
        fs::write(&resume_path, forged.encode()).unwrap();

        let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
        assert_eq!(registry.count(), 1, "the torrent still loads");
        assert!(
            first_progress(&mut registry).abs() < f64::EPSILON,
            "unverified pieces were published to the engine"
        );

        // And it is claimed for a scan, so the pieces are recovered rather than
        // simply forgotten — a torrent whose data really is there must not have
        // to be re-added by hand.
        assert_eq!(
            registry.pending_scans().len(),
            1,
            "the gap between have and verified must schedule a re-verify"
        );
    }

    /// Forging both halves is refused outright: `verified` is a subset of
    /// `have` by construction, so a file where it is not was not written by
    /// anything that understood the format.
    #[test]
    fn a_resume_file_whose_verified_exceeds_have_is_refused() {
        let (_content, bytes) = fixture("subset-demo");
        let data = TempDir::new("subset");
        let info_hash = {
            let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
            add_and_scan(&mut registry, &bytes)
        };
        let resume_path = data.0.join(format!("state/{}.resume", hex(&info_hash)));

        let good = Resume::decode(&fs::read(&resume_path).unwrap()).unwrap();
        let mut bad = good.clone();
        bad.verified = vec![0u8; clove_core::resume::bitfield_len(good.num_pieces)];
        bad.verified[0] = 0x80; // verified piece 0, which `have` does not hold
        fs::write(&resume_path, bad.encode()).unwrap();
        assert!(
            Resume::decode(&fs::read(&resume_path).unwrap()).is_err(),
            "verified must not be able to exceed have"
        );
        assert_eq!(
            Registry::<MockDialer>::open(&data.0).unwrap().count(),
            0,
            "an impossible resume file was loaded anyway"
        );
    }

    /// A priorities vector measured against a different file list decides the
    /// wrong files are skipped — silently, now that priorities do something.
    #[test]
    fn a_resume_file_with_the_wrong_priority_count_is_skipped() {
        let (_content, bytes) = fixture("prio-count-demo");
        let data = TempDir::new("prio-count");
        let info_hash = {
            let mut registry = Registry::<MockDialer>::open(&data.0).unwrap();
            add_and_scan(&mut registry, &bytes)
        };
        let resume_path = data.0.join(format!("state/{}.resume", hex(&info_hash)));

        let good = Resume::decode(&fs::read(&resume_path).unwrap()).unwrap();
        let mut bad = good.clone();
        bad.priorities = vec![1u8; good.priorities.len() + 1];
        fs::write(&resume_path, bad.encode()).unwrap();
        assert_eq!(
            Registry::<MockDialer>::open(&data.0).unwrap().count(),
            0,
            "a priorities vector for a different file list was loaded anyway"
        );

        fs::write(&resume_path, good.encode()).unwrap();
        assert_eq!(Registry::<MockDialer>::open(&data.0).unwrap().count(), 1);
    }

    #[test]
    fn hex_round_trips() {
        let ih = [0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(hex(&ih), "deadbeef");
        let full = [0x11u8; 20];
        assert_eq!(parse_info_hash(&hex(&full)), Some(full));
    }

    #[test]
    fn parse_info_hash_rejects_bad() {
        assert!(parse_info_hash("short").is_none());
        assert!(parse_info_hash(&"g".repeat(40)).is_none());
        assert!(parse_info_hash(&"A".repeat(40)).is_none()); // uppercase not accepted
    }
}
