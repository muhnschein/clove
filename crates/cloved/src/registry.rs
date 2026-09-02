//! The torrent registry: the daemon's engine host — hosted torrents, their
//! on-disk state (`docs/STATE-FORMAT.md`), and, once a network backend is
//! attached, a live [`Torrent`] + [`Swarm`] per unpaused entry.
//!
//! Generic over the dialer so the mock network proves the engine wiring in CI
//! and the SAM backend slots into the same seam.
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
use std::time::{Duration, Instant};

use clove_core::bitfield::Bitfield;
use clove_core::budget::PeerBudget;
use clove_core::config;
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
use i2pnet::naming::NamingCache;
use i2pnet::{I2pDialer, I2pNamingLookup};
// Only the test-support `add_peer` and the tests themselves name a peer
// directly; the daemon learns peers from announces, PEX and inbound streams.
#[cfg(test)]
use i2pnet::DestHash;

/// The `clove.conf` tunables the registry acts on.
///
/// One struct rather than a growing argument list: the registry is where the
/// operator's ceilings meet the engine, and every one of them arrives the same
/// way. [`Default`] is the empty-config configuration — what a fresh install
/// runs with, and what tests want.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Lay files out at full length on creation rather than letting them go
    /// sparse (`preallocate` in clove.conf(5)).
    pub(crate) preallocate: bool,
    /// Ceiling on peer connections across every hosted torrent at once.
    ///
    /// The per-torrent ceiling is not here: it reaches the engine through
    /// [`SwarmConfig::max_peers`] and the demux, which is where it was
    /// already, and one value with two homes is how they drift apart.
    pub(crate) peer_limit: usize,
    /// How many incomplete torrents may run at once.
    /// How many complete torrents may seed at once.
    /// Stop seeding at this ratio, in thousandths; `0` seeds without limit.
    pub(crate) seed_ratio_milli: u64,
    /// Stop seeding after this long with no peer; `0` never stops.
    pub(crate) seed_idle_minutes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            preallocate: false,
            peer_limit: config::DEFAULT_PEER_LIMIT,
            // Seeding without limit: stopping is a deviation an operator opts
            // into, not something a fresh install does behind their back.
            seed_ratio_milli: 0,
            seed_idle_minutes: 0,
        }
    }
}

impl From<&config::Config> for Limits {
    fn from(config: &config::Config) -> Self {
        Limits {
            preallocate: config.preallocate,
            peer_limit: config.peer_limit,
            seed_ratio_milli: config.seed_ratio_milli,
            seed_idle_minutes: config.seed_idle_minutes,
        }
    }
}

/// The set of hosted torrents plus where their state lives.
pub(crate) struct Registry<D: I2pDialer + I2pNamingLookup + Clone + Send + Sync + 'static>
where
    D::Stream: 'static,
{
    state_dir: PathBuf,
    downloads_dir: PathBuf,
    limits: Limits,
    /// The client-wide connection ceiling every hosted torrent draws on.
    ///
    /// Owned by the registry rather than by the attached network, because it
    /// outlives any one session: a router restart tears every peer down and
    /// returns their slots, and the ceiling itself is the operator's
    /// configuration, not the session's.
    budget: Arc<PeerBudget>,
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
    /// What the fetch loop has managed so far.
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
    preallocate: bool,
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
        let storage = Storage::create(&self.meta, &self.downloads_dir, self.preallocate)?;
        storage.verify_all()
    }
}

/// Whether a hosted torrent should be running, and if not, why not.
///
/// An enum rather than a `paused` boolean beside a reason, per `SCOPE.md` §9:
/// a stopped torrent cannot exist without an answer to the question its
/// operator will ask. The transitions are:
///
/// | From | To | On |
/// |---|---|---|
/// | `Running` | `Paused` | `clove pause`, the seed ratio, the seed idle limit |
/// | `Paused` | `Running` | `clove resume` |
///
/// Only `Paused` is persisted (resume `paused`); `Running` is what everything
/// else is.
///
/// Orthogonal to it is [`Hosted::scanning`]: a hash pass can be running over a
/// paused torrent, which is exactly what `clove verify` does, so that stays a
/// separate flag rather than a third variant here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wanted {
    /// Should run, and does when the session is up.
    Running,
    /// Stopped, and why. Survives restarts; nothing but the operator takes a
    /// torrent out of this state, whichever reason put it there.
    Paused(Why),
}

/// Why a torrent is stopped.
///
/// Carried by [`Wanted::Paused`] rather than kept alongside it, so a stopped
/// torrent cannot exist without an answer to the question its operator will
/// ask. A torrent that stops for a reason nobody can read is a bug report
/// about a torrent that "stopped working".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    /// `clove pause`, or a torrent loaded from a resume file that predates
    /// this distinction.
    Operator,
    /// It reached its seed ratio (`seed_ratio`).
    SeedRatio,
    /// It seeded with no peer for `seed_idle_minutes`.
    SeedIdle,
}

impl Why {
    /// The wire/on-disk tag. Explicit rather than derived from the variant
    /// order, because these numbers are in a file format.
    fn code(self) -> i64 {
        match self {
            Why::Operator => 0,
            Why::SeedRatio => 1,
            Why::SeedIdle => 2,
        }
    }

    /// Read a tag back. An unknown code reads as [`Why::Operator`]: a paused
    /// torrent is paused whatever the reason field says, and refusing to load
    /// one over a label would be the wrong trade entirely.
    fn from_code(code: i64) -> Why {
        match code {
            1 => Why::SeedRatio,
            2 => Why::SeedIdle,
            _ => Why::Operator,
        }
    }

    /// What `clove show` says about it.
    fn describe(self) -> &'static str {
        match self {
            Why::Operator => "paused by the operator",
            Why::SeedRatio => "stopped: seed ratio reached",
            Why::SeedIdle => "stopped: no peers for the idle limit",
        }
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
    /// Of those, how many arrived over `i2p_pex`.
    pex_peers: u64,
    /// Peers that reached us rather than being dialed — the live proof of the
    /// inbound `STREAM FORWARD` path (`PROTOCOL.i2p-bt` §2.5).
    inbound_peers: u64,
    /// Announces that worked, announces that did not, and the last reason —
    /// the first question to ask of a torrent with no peers.
    announces_ok: u32,
    announces_failed: u32,
    last_announce_error: Option<String>,
    /// Whether this torrent should be running, and if not, why not.
    wanted: Wanted,
    /// Pick pieces in order rather than rarest-first (SCOPE §3).
    sequential: bool,
    /// When this torrent was added (Unix seconds), which is the order the
    /// listing is in. Persisted since resume v4; 0 for anything added before
    /// that, which sorts it first.
    added: u64,
    /// Up and down rates in bytes per second, smoothed over the refresh tick.
    ///
    /// The listing had lifetime totals and nothing else, so it could not
    /// answer the first question anybody asks a torrent client — is anything
    /// moving *now*. Computed here rather than differenced by each client, so
    /// there is one implementation and `--json` consumers get it too.
    up_rate: f64,
    down_rate: f64,
    /// The counters and the moment [`Registry::refresh`] last read them, which
    /// is what the rates above are a difference of.
    rate_mark: Option<(Instant, u64, u64)>,
    /// This torrent's own seed ratio in thousandths, or `0` to follow the
    /// daemon's. Persisted (resume v5).
    seed_ratio_milli: u64,
    /// Since when this torrent has been seeding with nobody attached, for the
    /// idle limit. `None` while it has a peer, or is not seeding at all.
    ///
    /// In memory only: a restart starts the clock again, which is the
    /// forgiving direction — it costs a torrent nothing but time.
    idle_since: Option<Instant>,
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
}

impl Hosted {
    /// Fold this refresh's byte counts into the smoothed rates.
    ///
    /// Uses the wall-clock gap since the last call rather than assuming the
    /// refresh interval: refreshes also happen on demand — every `list` and
    /// `show` triggers one — so the gap is whatever it is, and dividing by a
    /// nominal tick would report a rate several times too high the moment
    /// anybody polled.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a byte delta over one refresh tick is nowhere near 2^53, and \
                  the result is a displayed rate"
    )]
    fn update_rates(&mut self) {
        let now = Instant::now();
        let Some((then, up_then, down_then)) = self.rate_mark else {
            // First reading: a baseline to measure the next one against, and
            // no rate yet. Guessing one from lifetime totals would report a
            // week's average as the current speed.
            self.rate_mark = Some((now, self.uploaded, self.downloaded));
            return;
        };
        let elapsed = now.duration_since(then).as_secs_f64();
        // Two refreshes in the same instant divide by ~zero; keep the previous
        // estimate rather than inventing an enormous one.
        if elapsed < 0.05 {
            return;
        }
        let up = self.uploaded.saturating_sub(up_then) as f64 / elapsed;
        let down = self.downloaded.saturating_sub(down_then) as f64 / elapsed;
        self.up_rate = self
            .up_rate
            .mul_add(RATE_SMOOTHING, up * (1.0 - RATE_SMOOTHING));
        self.down_rate = self
            .down_rate
            .mul_add(RATE_SMOOTHING, down * (1.0 - RATE_SMOOTHING));
        self.rate_mark = Some((now, self.uploaded, self.downloaded));
    }

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
            // (`docs/STATE-FORMAT.md`).
            verified: self.verified.as_bytes().to_vec(),
            priorities: self.priorities.clone(),
            uploaded: self.uploaded,
            downloaded: self.downloaded,
            paused: matches!(self.wanted, Wanted::Paused(_)),
            pause_reason: match self.wanted {
                Wanted::Paused(why) => why.code(),
                Wanted::Running => 0,
            },
            seed_ratio_milli: self.seed_ratio_milli,
            sequential: self.sequential,
            added: self.added,
        }
    }
}

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
///
/// Only ever used for the listing's order, so a clock that cannot be read
/// sensibly costs an ordering, not a start. Milliseconds because seconds are
/// not enough to order a bulk add — see [`Resume::added`].
///
/// [`Resume::added`]: clove_core::resume::Resume::added
fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// How much of the previous rate estimate survives each refresh.
///
/// An exponentially weighted mean rather than the raw per-tick difference:
/// piece traffic is bursty enough that the instantaneous number jumps around
/// too much to read, and a rate an operator cannot read is not a rate. Low
/// enough to still respond within a few seconds of a torrent stalling.
const RATE_SMOOTHING: f64 = 0.6;

/// A smoothed rate as whole bytes per second, for the API.
///
/// Integer rather than float on the wire: a rate is a measurement with maybe
/// two useful digits, and a JSON reader should not have to look at
/// `1234.5678901` and wonder which of it means anything.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "guarded finite, positive and below the clamp before the cast; \
              the result is a displayed rate"
)]
fn rounded(rate: f64) -> u64 {
    // A rate that reaches this is not a rate, and anything at all is a better
    // answer than a wrapped one.
    const CLAMP: f64 = 1e18;
    if !rate.is_finite() || rate <= 0.0 {
        return 0;
    }
    rate.min(CLAMP) as u64
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

/// What an operator asked for at add time.
///
/// Both settings are ones a torrent would otherwise need a second command to
/// change, after it had already started doing the wrong thing — beginning a
/// download you meant to queue for later, or picking pieces rarest-first when
/// you added it to watch.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AddOptions {
    /// Add it stopped rather than letting it start.
    pub(crate) paused: bool,
    /// Pick pieces in file order from the outset.
    pub(crate) sequential: bool,
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
    pub(crate) fn open(data_dir: &Path, limits: Limits) -> io::Result<Registry<D>> {
        let state_dir = data_dir.join("state");
        let downloads_dir = data_dir.join("downloads");
        fs::create_dir_all(&state_dir)?;
        fs::create_dir_all(&downloads_dir)?;
        let mut registry = Registry {
            state_dir,
            downloads_dir,
            limits,
            budget: PeerBudget::new(limits.peer_limit),
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
        self.reconcile();
    }

    /// Bring the set of running engines in line with what each torrent wants.
    ///
    /// The one place that decides what runs. Everything that could change the
    /// answer — an add, a pause, a resume, a removal, a completion, a session
    /// coming up — calls this rather than starting or stopping a torrent
    /// itself, so the decision cannot be made in one path and forgotten in
    /// another.
    ///
    /// **Stable by construction.** The decision is a function of the torrents'
    /// own states, not of what happens to be running, so a pass that changes
    /// nothing stops nothing: torrents already live stay live and keep their
    /// peers. That matters because stopping and restarting a torrent costs its
    /// whole peer set and a fresh announce.
    ///
    /// Every wanted torrent runs. What bounds the client is the peer budget,
    /// not a count of torrents: `peer_limit` across all of them and
    /// `torrent_peer_limit` within each (`clove.conf(5)`).
    pub(crate) fn reconcile(&mut self) {
        if self.network.is_none() {
            // Nothing can run without a session; `waiting-for-router` is the
            // honest state for all of them.
            return;
        }

        let mut start = Vec::new();
        let mut stop = Vec::new();
        for (info_hash, hosted) in &self.torrents {
            // A paused torrent is the operator's decision and a scanning one
            // has no publishable have-set yet.
            if matches!(hosted.wanted, Wanted::Paused(_)) || hosted.scanning {
                stop.push(*info_hash);
            } else {
                start.push(*info_hash);
            }
        }

        for info_hash in stop {
            self.stop_live(&info_hash);
        }
        for info_hash in start {
            if let Err(e) = self.start_live(&info_hash) {
                eprintln!("cloved: starting {}: {e}", hex(&info_hash));
            }
        }
    }

    /// Stop any torrent that has met its seeding limits.
    ///
    /// Run from the periodic tick, before the rebalance, so a torrent that
    /// stops here frees its slot in the same pass rather than a tick later.
    ///
    /// Only *seeding* torrents are candidates: a ratio is meaningless while a
    /// torrent is still fetching, and stopping an incomplete download because
    /// it had no peers for an hour would turn a quiet swarm into a torrent
    /// that never finishes.
    fn enforce_seed_limits(&mut self) {
        let idle_limit = (self.seed_idle_minutes() > 0)
            .then(|| Duration::from_secs(self.seed_idle_minutes().saturating_mul(60)));
        let default_ratio = self.limits.seed_ratio_milli;
        let now = Instant::now();
        let mut stop: Vec<([u8; 20], Why)> = Vec::new();

        for (info_hash, hosted) in &mut self.torrents {
            if hosted.live.is_none() || !hosted.is_complete() {
                hosted.idle_since = None;
                continue;
            }
            let ratio = hosted.effective_seed_ratio(default_ratio);
            if ratio > 0 && hosted.ratio_milli() >= ratio {
                stop.push((*info_hash, Why::SeedRatio));
                continue;
            }
            // The idle clock runs only while seeding with nobody attached, and
            // resets the moment a peer arrives — so "idle" means genuinely
            // nobody wants this, not "quiet for a while".
            if hosted.peers > 0 {
                hosted.idle_since = None;
                continue;
            }
            let since = *hosted.idle_since.get_or_insert(now);
            if let Some(limit) = idle_limit
                && now.duration_since(since) >= limit
            {
                stop.push((*info_hash, Why::SeedIdle));
            }
        }

        for (info_hash, why) in stop {
            let Some(hosted) = self.torrents.get_mut(&info_hash) else {
                continue;
            };
            hosted.wanted = Wanted::Paused(why);
            hosted.idle_since = None;
            eprintln!("cloved: {}: {}", hex(&info_hash), why.describe());
            self.stop_live(&info_hash);
            if let Some(hosted) = self.torrents.get(&info_hash) {
                let resume = hosted.resume(info_hash);
                if let Err(e) = write_resume_file(&self.state_dir, &info_hash, &resume) {
                    eprintln!("cloved: persisting {}: {e}", hex(&info_hash));
                }
            }
        }
    }

    /// The configured idle limit in minutes.
    fn seed_idle_minutes(&self) -> u64 {
        self.limits.seed_idle_minutes
    }

    /// Set one torrent's seed ratio, in thousandths; `0` follows the daemon's.
    ///
    /// A torrent stopped *because* of its ratio is un-stopped by raising it —
    /// otherwise the operator raises the limit, nothing happens, and the
    /// reason is invisible.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`], or a filesystem error persisting it.
    pub(crate) fn set_seed_ratio(
        &mut self,
        info_hash: &[u8; 20],
        milli: u64,
    ) -> Result<(), ActionError> {
        {
            let default_ratio = self.limits.seed_ratio_milli;
            let hosted = self
                .torrents
                .get_mut(info_hash)
                .ok_or(ActionError::NotFound)?;
            hosted.seed_ratio_milli = milli;
            if hosted.wanted == Wanted::Paused(Why::SeedRatio) {
                let ratio = hosted.effective_seed_ratio(default_ratio);
                if ratio == 0 || hosted.ratio_milli() < ratio {
                    hosted.wanted = Wanted::Running;
                }
            }
        }
        self.reconcile();
        let hosted = self.torrents.get(info_hash).ok_or(ActionError::NotFound)?;
        write_resume_file(&self.state_dir, info_hash, &hosted.resume(*info_hash))
            .map_err(ActionError::Io)
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
        if hosted.wanted != Wanted::Running || hosted.live.is_some() || hosted.scanning {
            return Ok(());
        }
        let storage = Arc::new(Storage::create(
            &hosted.meta,
            &self.downloads_dir,
            self.limits.preallocate,
        )?);
        let torrent = Torrent::with_budget(
            &hosted.meta,
            storage,
            &hosted.have,
            hosted.mode(),
            network.peer_id,
            Arc::clone(&self.budget),
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
        });
        Ok(())
    }

    /// Take a torrent offline: unregister from the demux, snapshot its
    /// progress, and signal its swarm to stop (without blocking on in-flight
    /// dials). Peers already attached drain on their own; full disconnect
    /// arrives with the peer-timeout work.
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
                // An offline torrent moves nothing. Leaving the last live
                // figures in place would show a paused torrent still pulling
                // 400 KiB/s, which is the same class of lie as the peer count
                // above.
                hosted.up_rate = 0.0;
                hosted.down_rate = 0.0;
                hosted.rate_mark = None;
            }
            hosted.update_rates();
        }
    }

    /// Snapshot every live torrent's progress and persist its resume record.
    /// Called periodically by the daemon and around lifecycle transitions.
    pub(crate) fn persist_progress(&mut self) {
        self.refresh();
        // Seeding limits first, so a torrent that stops here frees its slot
        // in the same pass rather than a tick later.
        self.enforce_seed_limits();
        // A torrent stopped by its seed limits above has just changed what it
        // wants. Nothing else notices a completion — it happens in the engine,
        // not through an API call — so this is where that lands.
        self.reconcile();
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

    /// Hand a live torrent a peer to dial.
    ///
    /// Test support: the engine tests need a peer from somewhere, and on the
    /// mock network there is no tracker to get one from. The daemon has no
    /// path to this — its peers come from announces, PEX and inbound streams.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`], or [`ActionError::BadInput`] when the
    /// torrent is not running (paused, or no router yet).
    #[cfg(test)]
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
    pub(crate) fn add_torrent(
        &mut self,
        bytes: &[u8],
        options: AddOptions,
    ) -> Result<([u8; 20], ScanJob), AddError> {
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
            wanted: if options.paused {
                Wanted::Paused(Why::Operator)
            } else {
                Wanted::Running
            },
            sequential: options.sequential,
            // The listing's order is add order, and this is the add.
            added: now_unix_millis(),
            up_rate: 0.0,
            down_rate: 0.0,
            rate_mark: None,
            // Recomputed by the rebalance that follows the initial scan.
            // No per-torrent override until one is set; the daemon's applies.
            seed_ratio_milli: 0,
            idle_since: None,
            scanning: true,
            live: None,
        };
        let hex = hex(&info_hash);
        let torrent_path = self.state_dir.join(format!("{hex}.torrent"));
        atomic_write(&torrent_path, bytes).map_err(AddError::Io)?;
        if let Err(e) = self.write_resume(&info_hash, &hosted) {
            // The pair is the unit: a `.torrent` with no resume file beside it
            // is not a torrent this daemon can load, and leaving one behind
            // turns a failed add into a file that looks like a half-added
            // torrent for ever. Best-effort, because the disk that just refused
            // a write may refuse this too — but then nothing was added either.
            let _ = fs::remove_file(&torrent_path);
            return Err(AddError::Io(e));
        }
        self.torrents.insert(info_hash, hosted);
        Ok((
            info_hash,
            ScanJob {
                info_hash,
                meta,
                downloads_dir: self.downloads_dir.clone(),
                preallocate: self.limits.preallocate,
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
                preallocate: self.limits.preallocate,
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

    /// Promote a fetched magnet: add the synthesized `.torrent` bytes through
    /// the normal path (persist, go live), and only then drop the pending entry
    /// and its URI file.
    ///
    /// # Errors
    ///
    /// [`AddError`] from the underlying [`add_torrent`](Registry::add_torrent),
    /// with the pending magnet left intact.
    pub(crate) fn complete_magnet(
        &mut self,
        info_hash: &[u8; 20],
        torrent_bytes: &[u8],
    ) -> Result<ScanJob, AddError> {
        let (_, job) = self.add_torrent(torrent_bytes, AddOptions::default())?;
        self.pending.remove(info_hash);
        let _ = fs::remove_file(self.state_dir.join(format!("{}.magnet", hex(info_hash))));
        Ok(job)
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
            hosted.wanted = if paused {
                Wanted::Paused(Why::Operator)
            } else {
                Wanted::Running
            };
        }
        self.reconcile();
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
        let preallocate = self.limits.preallocate;
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
            preallocate,
        })
    }

    /// Publish the result of a [`ScanJob`]: adopt the have-set, persist it, and
    /// bring the torrent live if it should be. Returns the verified piece count.
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
        // an added or freshly-verified torrent starts running.
        self.reconcile();
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
                // Resolved the same way it was written: following a symlinked
                // component here would delete somebody else's file, which is
                // the write-side escape pointed the other way and rather less
                // recoverable.
                if let Err(e) = clove_core::storage::remove_beneath(&self.downloads_dir, &file.path)
                {
                    // The path is the torrent's, so it is a stranger's text on
                    // its way to a terminal: `check_component` refuses
                    // separators, `.`, `..` and NUL, and has no opinion about
                    // `ESC`. See `clove_core::text`.
                    eprintln!(
                        "cloved: not deleting {}: {e}",
                        clove_core::text::scrub(&file.path.join("/"))
                    );
                }
            }
            // Best-effort: drop the torrent's now-empty top directory.
            let _ = fs::remove_dir(self.downloads_dir.join(&hosted.meta.name));
        }
        self.torrents.remove(info_hash);
        self.reconcile();
        Ok(())
    }

    /// The torrents as a JSON array, one object each, in add order.
    /// Live progress is refreshed first.
    pub(crate) fn list(&mut self) -> Value {
        self.refresh();
        // Add order, not info-hash order. The map is keyed by hash, so the
        // listing used to be sorted by a hash — which is to say shuffled, and
        // reshuffled on every add, so a row moved out from under whoever was
        // reading it. Ties break on the hash, so the order is total and does
        // not wobble between calls for torrents added in the same second.
        let mut ordered: Vec<(&[u8; 20], &Hosted)> = self.torrents.iter().collect();
        ordered.sort_by_key(|(info_hash, hosted)| (hosted.added, **info_hash));
        let mut items: Vec<Value> = ordered
            .into_iter()
            .map(|(_, hosted)| hosted.to_json())
            .collect();
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
        let mut torrents = Vec::new();
        let mut magnets = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("torrent") => torrents.push(path),
                Some("magnet") => magnets.push(path),
                _ => {}
            }
        }
        // Torrents first, all of them, because `load_magnet` needs to know
        // whether a magnet has already been promoted and `read_dir` order says
        // nothing about which it hands over first. Promotion writes the torrent
        // before dropping the magnet, so a crash in between leaves both on
        // disk; without this ordering that came back as the same info-hash in
        // the torrent list *and* the pending list, listed twice and fetched for
        // no reason.
        for path in torrents {
            if let Err(e) = self.load_one(&path) {
                eprintln!("cloved: skipping {}: {e}", path.display());
            }
        }
        for path in magnets {
            if let Err(e) = self.load_magnet(&path) {
                eprintln!("cloved: skipping {}: {e}", path.display());
            }
        }
    }

    fn load_magnet(&mut self, path: &Path) -> Result<(), String> {
        let uri = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let magnet = Magnet::parse(uri.trim()).map_err(|e| e.to_string())?;
        // Already promoted: the torrent is the newer, better-founded record of
        // the same thing, so the leftover magnet is stale rather than a second
        // torrent. Removing it here is what keeps a crash mid-promotion from
        // costing anything at all.
        if self.torrents.contains_key(&magnet.info_hash) {
            let _ = fs::remove_file(path);
            return Ok(());
        }
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
                wanted: if resume.paused {
                    Wanted::Paused(Why::from_code(resume.pause_reason))
                } else {
                    Wanted::Running
                },
                sequential: resume.sequential,
                // 0 for anything written before resume v4, which sorts it
                // ahead of everything added since — true, as it happens.
                added: resume.added,
                up_rate: 0.0,
                down_rate: 0.0,
                rate_mark: None,
                // Not persisted: forcing is a statement about right now.
                seed_ratio_milli: resume.seed_ratio_milli,
                idle_since: None,
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
    /// The ratio this torrent is held to: its own if set, else the daemon's.
    fn effective_seed_ratio(&self, default_milli: u64) -> u64 {
        if self.seed_ratio_milli > 0 {
            self.seed_ratio_milli
        } else {
            default_milli
        }
    }

    /// Uploaded over downloaded, in thousandths.
    ///
    /// A torrent that downloaded nothing — added complete, or every file
    /// skipped — has no ratio to speak of, and dividing by zero to get one
    /// would stop it the instant it served a byte. It reports `0`, so only an
    /// explicit ratio of 0 (meaning "no limit") applies to it, which is to say
    /// it seeds until the operator says otherwise.
    fn ratio_milli(&self) -> u64 {
        if self.downloaded == 0 {
            return 0;
        }
        self.uploaded
            .saturating_mul(1000)
            .checked_div(self.downloaded)
            .unwrap_or(0)
    }

    /// Whether every piece this torrent was told to fetch is present — which
    /// is to say, whether it draws on the seed allowance or the download one.
    fn is_complete(&self) -> bool {
        let (wanted, held) = self.wanted_and_held();
        held == wanted
    }

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
        let complete = self.is_complete();
        if self.scanning {
            "verifying"
        } else if matches!(self.wanted, Wanted::Paused(_)) {
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
            ("up_rate".to_owned(), Value::UInt(rounded(self.up_rate))),
            ("down_rate".to_owned(), Value::UInt(rounded(self.down_rate))),
            ("added".to_owned(), Value::UInt(self.added)),
            (
                "peers".to_owned(),
                Value::UInt(u64::try_from(self.peers).unwrap_or(u64::MAX)),
            ),
            // Alongside `peers` rather than only in the detail object: the two
            // are one reading, and "four connected" says nothing without "out
            // of how many we know of". A listing that carried the first and
            // not the second could not tell a small swarm from a torrent that
            // is failing to dial the swarm it has.
            (
                "known_peers".to_owned(),
                Value::UInt(u64::try_from(self.known_peers).unwrap_or(u64::MAX)),
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
            ("up_rate".to_owned(), Value::UInt(rounded(self.up_rate))),
            ("down_rate".to_owned(), Value::UInt(rounded(self.down_rate))),
            ("added".to_owned(), Value::UInt(self.added)),
            (
                "peers".to_owned(),
                Value::UInt(u64::try_from(self.peers).unwrap_or(u64::MAX)),
            ),
            (
                "known_peers".to_owned(),
                Value::UInt(u64::try_from(self.known_peers).unwrap_or(u64::MAX)),
            ),
            ("ratio".to_owned(), Value::UInt(self.ratio_milli())),
            ("seed_ratio".to_owned(), Value::UInt(self.seed_ratio_milli)),
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
        // Why a stopped torrent stopped, which is the question its operator
        // will ask — and the reason `Wanted::Paused` carries one at all.
        if let Wanted::Paused(why) = self.wanted {
            fields.push(("paused_because".to_owned(), Value::from(why.describe())));
        }
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

/// Shortest prefix that may stand in for an info-hash.
///
/// Four hex characters is 16 bits. That is short enough to type from a listing
/// and long enough that a collision across the tens of torrents a client hosts
/// is something you have to go looking for — and when it happens the answer is
/// [`ResolveError::Ambiguous`], not a guess.
pub(crate) const MIN_PREFIX: usize = 4;

/// What the whole client is doing right now, for `GET /v1/status`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Totals {
    /// Bytes per second out, summed over every torrent.
    pub(crate) up_rate: u64,
    /// Bytes per second in, summed over every torrent.
    pub(crate) down_rate: u64,
    /// Peers attached across every torrent — the draw on the budget below.
    pub(crate) peers: usize,
    /// The ceiling those peers come from (`peer_limit`), reported beside them
    /// because a peer count means nothing without the number it is approaching.
    pub(crate) peer_limit: usize,
}

/// A torrent that a prefix could have meant.
#[derive(Debug)]
pub(crate) struct Candidate {
    /// Full hex info-hash.
    pub(crate) info_hash: String,
    /// Its display name, for telling two candidates apart.
    pub(crate) name: String,
}

/// Why a torrent reference did not name exactly one torrent.
#[derive(Debug)]
pub(crate) enum ResolveError {
    /// Not hex, too short, or longer than an info-hash (400).
    Malformed,
    /// Well-formed, but no hosted torrent starts with it (404).
    NotFound,
    /// More than one does (409). Never resolved by picking one.
    Ambiguous(Vec<Candidate>),
}

impl<D: I2pDialer + I2pNamingLookup + Clone + Send + Sync + 'static> Registry<D>
where
    D::Stream: 'static,
{
    /// Resolve an operator's torrent reference — a full info-hash, or a unique
    /// hex prefix of one — to exactly one info-hash.
    ///
    /// Prefixes are git's affordance, and they exist here for the same reason:
    /// every per-torrent command used to require all forty characters, which is
    /// fine for the one torrent you just added and unusable for the twentieth.
    ///
    /// Two rules make this safe to hand a script. A **full 40-character hash
    /// keeps its exact-match path** and never consults the table, so anything
    /// that works today keeps working and cannot start meaning a different
    /// torrent because of what else got added. And an **ambiguous prefix is an
    /// error carrying the candidates**, never a choice — the failure mode this
    /// must not have is `clove remove --data` quietly picking one of two.
    ///
    /// Resolution happens here rather than in `clove(1)` because the CLI is a
    /// one-request client: resolving there means a listing fetch before every
    /// action, two round trips, and a window in which the answer changes
    /// between the fetch and the act.
    ///
    /// # Errors
    ///
    /// [`ResolveError`] — malformed, unmatched, or matching more than one.
    pub(crate) fn resolve(&self, text: &str) -> Result<[u8; 20], ResolveError> {
        if let Some(exact) = parse_info_hash(text) {
            return Ok(exact);
        }
        if text.len() < MIN_PREFIX
            || text.len() > 40
            || !text.bytes().all(|b| hex_digit(b).is_some())
        {
            return Err(ResolveError::Malformed);
        }
        // Linear over a map of tens. A `BTreeMap` range would be asymptotically
        // nicer and is not worth it: the bound is the number of torrents one
        // person hosts, and an odd-length prefix makes the byte range fiddly
        // enough to get subtly wrong.
        //
        // Magnets still fetching their metadata are listed and can be removed,
        // so they answer to a prefix too.
        let found: Vec<[u8; 20]> = self
            .torrents
            .keys()
            .chain(self.pending.keys())
            .filter(|info_hash| hex(*info_hash).starts_with(text))
            .copied()
            .collect();
        match found.as_slice() {
            [] => Err(ResolveError::NotFound),
            [one] => Ok(*one),
            // Names are looked up only here: the answer an operator needs from
            // an ambiguous prefix is which torrents it hit, and the resolving
            // path that succeeds should not pay to build that.
            many => Err(ResolveError::Ambiguous(
                many.iter().map(|ih| self.candidate(ih)).collect(),
            )),
        }
    }

    /// Client-wide totals for `GET /v1/status`: current up and down rates in
    /// bytes per second, peers attached, and the budget those peers come from.
    ///
    /// One refresh for the whole answer, so the rates here and the ones in the
    /// listing are the same reading rather than two a moment apart.
    pub(crate) fn totals(&mut self) -> Totals {
        self.refresh();
        let mut totals = Totals {
            up_rate: 0,
            down_rate: 0,
            peers: 0,
            peer_limit: self.budget.limit(),
        };
        let (mut up, mut down) = (0.0, 0.0);
        for hosted in self.torrents.values() {
            up += hosted.up_rate;
            down += hosted.down_rate;
            totals.peers += hosted.peers;
        }
        totals.up_rate = rounded(up);
        totals.down_rate = rounded(down);
        totals
    }

    /// Whether this info-hash is a magnet still waiting for its metadata.
    ///
    /// Such an entry is listed, resolvable and removable, but has no engine and
    /// no file list, so every other operation has nothing to act on. The
    /// distinction exists so those can say *why* rather than claiming the
    /// torrent does not exist — which, for something `clove list` is showing,
    /// is just untrue.
    pub(crate) fn is_pending(&self, info_hash: &[u8; 20]) -> bool {
        self.pending.contains_key(info_hash)
    }

    /// One resolution candidate: its hash, and the name that tells it apart.
    fn candidate(&self, info_hash: &[u8; 20]) -> Candidate {
        let full = hex(info_hash);
        let name = self.torrents.get(info_hash).map_or_else(
            || {
                self.pending
                    .get(info_hash)
                    .and_then(|p| p.magnet.display_name.clone())
                    .unwrap_or_else(|| full.clone())
            },
            |hosted| hosted.meta.name.clone(),
        );
        Candidate {
            info_hash: full,
            name,
        }
    }
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
///
/// The temp name carries this process's pid and is opened `create_new`, the
/// way `write_private_file` in `main.rs` does it. A fixed `<path>.tmp` opened
/// with truncation let two daemons on one data directory interleave: one
/// truncates the temp the other has just fsynced, and the other renames the
/// half-written file into place. The instance lock makes that pairing
/// unlikely; this makes it harmless.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    // A temp left by an earlier crash of this pid would fail create_new below.
    let _ = fs::remove_file(&tmp);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
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
        let (info_hash, job) = registry
            .add_torrent(bytes, AddOptions::default())
            .expect("add");
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
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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
        let mut reopened = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        assert_eq!(reopened.count(), 1);
        assert!((first_progress(&mut reopened) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pause_takes_the_engine_offline_and_resume_restores_it() {
        let net = MockNet::new();
        let (_content, bytes) = fixture("pause-demo");

        let data = TempDir::new("data");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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
            let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
            let info_hash = add_and_scan(&mut registry, &bytes);
            // Rarest-first is the default; nothing is claimed until asked.
            assert_eq!(sequential_flag(&mut registry, &info_hash), Some(false));
            registry.set_sequential(&info_hash, true).unwrap();
            assert_eq!(sequential_flag(&mut registry, &info_hash), Some(true));
            info_hash
        };
        // A fresh registry over the same data dir reads the flag back out of
        // the resume file — the point of putting it in the format at all.
        let mut reopened = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        assert_eq!(sequential_flag(&mut reopened, &info_hash), Some(true));
        reopened.set_sequential(&info_hash, false).unwrap();
        assert_eq!(sequential_flag(&mut reopened, &info_hash), Some(false));
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
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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
            let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
            add_and_scan(&mut registry, &bytes)
        };
        let resume_path = data.0.join(format!("state/{}.resume", hex(&info_hash)));

        // Reopening as-is finds it.
        assert_eq!(
            Registry::<MockDialer>::open(&data.0, Limits::default())
                .unwrap()
                .count(),
            1
        );

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
            Registry::<MockDialer>::open(&data.0, Limits::default())
                .unwrap()
                .count(),
            0,
            "a resume file describing a different torrent was loaded anyway"
        );

        // And the good one still loads, so the check is about the mismatch and
        // not about rejecting resume files in general.
        fs::write(&resume_path, good.encode()).unwrap();
        assert_eq!(
            Registry::<MockDialer>::open(&data.0, Limits::default())
                .unwrap()
                .count(),
            1
        );
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
            let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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

        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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
            let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
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
            Registry::<MockDialer>::open(&data.0, Limits::default())
                .unwrap()
                .count(),
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
            let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
            add_and_scan(&mut registry, &bytes)
        };
        let resume_path = data.0.join(format!("state/{}.resume", hex(&info_hash)));

        let good = Resume::decode(&fs::read(&resume_path).unwrap()).unwrap();
        let mut bad = good.clone();
        bad.priorities = vec![1u8; good.priorities.len() + 1];
        fs::write(&resume_path, bad.encode()).unwrap();
        assert_eq!(
            Registry::<MockDialer>::open(&data.0, Limits::default())
                .unwrap()
                .count(),
            0,
            "a priorities vector for a different file list was loaded anyway"
        );

        fs::write(&resume_path, good.encode()).unwrap();
        assert_eq!(
            Registry::<MockDialer>::open(&data.0, Limits::default())
                .unwrap()
                .count(),
            1
        );
    }

    /// A failure against a peer must not record *which* peer.
    ///
    /// A tracker chooses the peers we dial, so it chooses which destinations
    /// end up written next to our torrent — and this text is not merely logged,
    /// it is kept and served by `clove list`. `SECURITY.md` puts a peer's
    /// destination reaching "logs, error messages, or the local API" in the
    /// leak class, and this reached all three.
    #[test]
    fn a_peer_that_fails_is_not_named_in_the_recorded_error() {
        let net = MockNet::new();
        let (_content, bytes) = fixture("peer-name-demo");
        let meta = MetaInfo::parse(&bytes).unwrap();
        let info_hash = meta.info_hash.0;

        // A tracker that hands back one peer nobody is listening for, so the
        // dial fails and the round records why.
        let dead_peer = DestHash([0x5A; 32]);
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
                body.extend_from_slice(&dead_peer.0);
                body.push(b'e');
                let response = clove_core::http::Response::new(200, "text/plain", body);
                let _ = std::io::Write::write_all(&mut stream, &response.encode());
            }
        });

        let data = TempDir::new("peer-name");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        let ep = net.endpoint();
        registry.attach_network(
            ep.dialer(),
            InboundDemux::new(8),
            *b"-CV0001-magnetmagnet",
            quick_swarm(),
            "magnet-b64".to_owned(),
        );
        let uri = format!(
            "magnet:?xt=urn:btih:{}&dn=nope&tr=http%3A%2F%2Ftracker.i2p%2Fannounce",
            hex(&info_hash)
        );
        registry.add_magnet(&uri).unwrap();
        let ctx = registry.fetch_context(&info_hash).unwrap().unwrap();
        let (found, round) = crate::try_fetch_round(&ctx, info_hash, true);
        assert!(found.is_none(), "there was nobody to fetch from");
        assert_eq!(round.peers_tried, 1, "the peer was dialed");
        registry.note_fetch_round(&info_hash, 1, &round);

        let error = round
            .last_error
            .clone()
            .expect("the round records a reason");
        let b32 = dead_peer.to_b32();
        let label = b32.trim_end_matches(".b32.i2p");
        assert!(!error.contains(&b32), "the peer's address is in: {error}");
        assert!(
            !error.contains(label),
            "the peer's b32 label is in: {error}"
        );
        // What it is *for* survives: the stage, and how many were tried.
        assert!(error.contains("peer"), "the stage is missing from: {error}");

        // And the same through the API, which is where it actually travels.
        let listed = registry.list().encode();
        assert!(!listed.contains(label), "a peer address reached the API");
    }

    /// A promotion that fails leaves the magnet exactly where it was.
    ///
    /// The old order dropped the pending entry and deleted its `.magnet` file
    /// *first*, so a failure after that point had destroyed the recoverable
    /// state and registered nothing in its place: gone from memory, gone from
    /// disk, no torrent, and a restart finding neither. The magnet is the only
    /// record of what the user asked for, and it is not the promotion's to
    /// spend before the replacement exists.
    #[test]
    fn a_failed_promotion_leaves_the_magnet_recoverable() {
        let data = TempDir::new("promote-fail");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        let info_hash = registry
            .add_magnet(&format!("magnet:?xt=urn:btih:{}", "ab".repeat(20)))
            .expect("add magnet");
        let magnet_path = data.0.join(format!("state/{}.magnet", hex(&info_hash)));
        assert!(magnet_path.is_file(), "the magnet was persisted");

        // Promote with bytes that are not a torrent: `add_torrent` fails, and
        // everything about the magnet must survive it.
        assert!(
            registry
                .complete_magnet(&info_hash, b"not a torrent")
                .is_err(),
            "rubbish must not promote"
        );
        assert_eq!(registry.pending_hashes(), vec![info_hash], "still pending");
        assert!(magnet_path.is_file(), "the magnet file was deleted anyway");
        assert_eq!(registry.count(), 0, "nothing was registered");

        // And a restart still finds it, which is the part that matters after a
        // crash rather than an error return.
        let reopened = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        assert_eq!(reopened.pending_hashes(), vec![info_hash]);
    }

    /// A crash between writing the torrent and dropping the magnet leaves both
    /// on disk. The torrent wins, and the stale magnet is cleaned up.
    ///
    /// Without this the same info-hash came back in the torrent list *and* the
    /// pending list — listed twice, and a metadata fetch spawned for something
    /// already fully known.
    #[test]
    fn a_leftover_magnet_beside_its_torrent_is_dropped() {
        let (_content, bytes) = fixture("leftover-demo");
        let data = TempDir::new("leftover");
        let info_hash = {
            let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
            add_and_scan(&mut registry, &bytes)
        };

        // The state promotion passes through, if it stops half way.
        let magnet_path = data.0.join(format!("state/{}.magnet", hex(&info_hash)));
        fs::write(
            &magnet_path,
            format!("magnet:?xt=urn:btih:{}", hex(&info_hash)),
        )
        .unwrap();

        let registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        assert_eq!(registry.count(), 1, "the torrent loads");
        assert!(
            registry.pending_hashes().is_empty(),
            "the same torrent was also listed as a pending magnet"
        );
        assert!(
            !magnet_path.exists(),
            "the stale magnet file was left behind"
        );
    }

    /// The temp file is this process's own, not a fixed name any writer of
    /// the same target would open with truncation.
    #[test]
    fn atomic_write_uses_a_private_temp_name() {
        let data = TempDir::new("atomic-tmp");
        let target = data.0.join("state.resume");
        // What the old fixed name would have been. Left in place by "another
        // writer"; if this write went through it, it would be truncated and
        // renamed away.
        let fixed = data.0.join("state.resume.tmp");
        fs::write(&fixed, b"another writer's half-written temp").unwrap();
        // And a leftover of *our* own pid, from a crash: it must not make the
        // write fail, and it must not be trusted either.
        let ours = data
            .0
            .join(format!("state.resume.{}.tmp", std::process::id()));
        fs::write(&ours, b"stale").unwrap();

        atomic_write(&target, b"the real state").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"the real state");
        assert_eq!(
            fs::read(&fixed).unwrap(),
            b"another writer's half-written temp",
            "the fixed-name temp was touched"
        );
        assert!(
            !ours.exists(),
            "the pid temp was left behind after the rename"
        );
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

    /// Magnets are how a test picks the info-hash it wants: a `.torrent`'s is
    /// SHA-1 of its info dict and cannot be chosen, but `xt=urn:btih:` is
    /// whatever we write. That is what makes the ambiguous case testable at
    /// all.
    fn add_magnet_named(registry: &mut Registry<MockDialer>, hash_hex: &str, name: &str) {
        registry
            .add_magnet(&format!("magnet:?xt=urn:btih:{hash_hex}&dn={name}"))
            .expect("add magnet");
    }

    #[test]
    fn a_prefix_names_a_torrent_and_an_ambiguous_one_never_guesses() {
        let data = TempDir::new("resolve");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();

        let a = format!("aaaa{}", "1".repeat(36));
        let b = format!("aaaa{}", "2".repeat(36));
        let c = format!("bbbb{}", "3".repeat(36));
        add_magnet_named(&mut registry, &a, "first");
        add_magnet_named(&mut registry, &b, "second");
        add_magnet_named(&mut registry, &c, "third");

        // A full hash resolves to itself, and is the one form that never
        // consults the table — so it cannot start meaning a different torrent
        // because of what else got added.
        assert_eq!(hex(&registry.resolve(&a).expect("exact")), a);

        // A prefix that names exactly one torrent resolves to it, at the
        // shortest accepted length and beyond.
        assert_eq!(hex(&registry.resolve("bbbb").expect("unique")), c);
        assert_eq!(hex(&registry.resolve("bbbb3333").expect("longer")), c);
        assert_eq!(
            hex(&registry.resolve("aaaa1").expect("one past the fork")),
            a
        );

        // The failure this must never have: two candidates and a choice made.
        match registry.resolve("aaaa") {
            Err(ResolveError::Ambiguous(candidates)) => {
                assert_eq!(candidates.len(), 2);
                let mut names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, ["first", "second"]);
                // The candidates carry full hashes, so the operator can retype
                // one of them without going back to `clove list`.
                for candidate in &candidates {
                    assert_eq!(candidate.info_hash.len(), 40);
                }
            }
            _ => panic!("an ambiguous prefix must not resolve"),
        }

        // Well-formed but matching nothing.
        assert!(matches!(
            registry.resolve("cccc"),
            Err(ResolveError::NotFound)
        ));
        // A *full* hash that names nothing still resolves: the exact path
        // answers "what hash is this", and whether a torrent by that name
        // exists is the action's question, answered with the same 404. Keeping
        // the two separate is what lets a full hash skip the table entirely.
        let unknown = "9".repeat(40);
        assert_eq!(hex(&registry.resolve(&unknown).expect("exact")), unknown);

        // Malformed: too short to be worth guessing from, not hex, longer than
        // an info-hash, or the uppercase `parse_info_hash` already refuses.
        for bad in [
            "",
            "a",
            "aaa",
            "zzzz",
            "aaaa!",
            &"a".repeat(41),
            &"A".repeat(40),
        ] {
            assert!(
                matches!(registry.resolve(bad), Err(ResolveError::Malformed)),
                "{bad:?} should be malformed"
            );
        }
        // The boundary itself is accepted rather than refused.
        assert_eq!(MIN_PREFIX, 4);
        assert!(!matches!(
            registry.resolve("aaaa"),
            Err(ResolveError::Malformed)
        ));
    }

    #[test]
    fn the_listing_is_in_add_order_not_hash_order() {
        let data = TempDir::new("order");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();

        // Added in a deliberate order; their info-hashes are SHA-1 of the info
        // dict and so land in whatever order they land in. Before this, the
        // listing was that order — reshuffled on every add.
        let names = ["third", "first", "second"];
        let mut added = Vec::new();
        for name in names {
            let (_, bytes) = fixture(name);
            added.push(add_and_scan(&mut registry, &bytes));
        }

        let listed = registry.list();
        let order: Vec<&str> = listed
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|item| item.get("name").and_then(clove_core::json::Value::as_str))
            .collect();
        assert_eq!(
            order, names,
            "the listing must be in the order torrents were added"
        );

        // Stable across calls, which is what stops a row moving under the
        // cursor of whoever is reading it.
        let again = registry.list();
        assert_eq!(listed.encode(), again.encode());

        // Adding one puts it last and disturbs nothing before it.
        let (_, bytes) = fixture("fourth");
        add_and_scan(&mut registry, &bytes);
        let listed = registry.list();
        let order: Vec<&str> = listed
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|item| item.get("name").and_then(clove_core::json::Value::as_str))
            .collect();
        assert_eq!(order, ["third", "first", "second", "fourth"]);

        // The reason `added` is milliseconds and not seconds: these were all
        // added inside the same second, and at one-second resolution they
        // shared a timestamp, fell through to the info-hash tie-break, and
        // came out shuffled — which is the exact failure the field exists to
        // fix, and what any scripted bulk add would hit every time.
        assert_eq!(added.len(), 3);
    }

    /// Every torrent's state, in listing order — what an operator sees.
    fn states(registry: &mut Registry<MockDialer>) -> Vec<String> {
        registry
            .list()
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|item| item.get("state").and_then(clove_core::json::Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn every_wanted_torrent_runs_and_pausing_stops_only_that_one() {
        let net = MockNet::new();
        let data = TempDir::new("running");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();

        let mut hashes = Vec::new();
        for name in ["a", "b", "c", "d", "e"] {
            let (_, bytes) = fixture(name);
            hashes.push(add_and_scan(&mut registry, &bytes));
        }

        // Without a session nothing runs, and the router is the reason.
        assert_eq!(states(&mut registry), ["waiting-for-router"; 5]);

        let ep = net.endpoint();
        registry.attach_network(
            ep.dialer(),
            InboundDemux::new(8),
            *b"-CV0001-leechleechle",
            quick_swarm(),
            "leecher-b64".to_owned(),
        );

        // All of them, not a subset: what bounds the client is the peer
        // budget, not a count of torrents.
        assert_eq!(states(&mut registry), ["downloading"; 5]);

        // Pausing stops exactly one torrent and starts nothing in its place.
        registry.set_paused(&hashes[0], true).expect("pause");
        assert_eq!(
            states(&mut registry),
            [
                "paused",
                "downloading",
                "downloading",
                "downloading",
                "downloading"
            ]
        );

        // And resuming brings back that one, displacing nothing.
        registry.set_paused(&hashes[0], false).expect("resume");
        assert_eq!(states(&mut registry), ["downloading"; 5]);

        // Removing one leaves the others where they were.
        registry.remove(&hashes[3], false).expect("remove");
        assert_eq!(states(&mut registry), ["downloading"; 4]);

        // Losing the session takes everything offline for the one reason.
        registry.detach_network();
        assert_eq!(states(&mut registry), ["waiting-for-router"; 4]);
    }

    #[test]
    fn a_seeding_torrent_stops_at_its_ratio_and_says_why() {
        let data = TempDir::new("ratio");
        let limits = Limits {
            // 2.0, as thousandths.
            seed_ratio_milli: 2000,
            ..Limits::default()
        };
        let mut registry = Registry::<MockDialer>::open(&data.0, limits).unwrap();
        let (_, bytes) = fixture("seeded");
        let info_hash = add_and_scan(&mut registry, &bytes);

        // A torrent with no engine is not seeding, so nothing applies to it
        // however lopsided its counters are.
        {
            let hosted = registry.torrents.get_mut(&info_hash).expect("hosted");
            hosted.downloaded = 1000;
            hosted.uploaded = 5000;
        }
        registry.enforce_seed_limits();
        assert!(
            !matches!(
                registry.torrents[&info_hash].wanted,
                Wanted::Paused(Why::SeedRatio)
            ),
            "an offline torrent must not be stopped for its ratio"
        );

        // The ratio arithmetic itself, in thousandths and exact.
        {
            let hosted = &registry.torrents[&info_hash];
            assert_eq!(hosted.ratio_milli(), 5000);
            assert_eq!(hosted.effective_seed_ratio(2000), 2000);
        }

        // A torrent that downloaded nothing has no ratio to exceed — it was
        // added complete — and reports 0 rather than dividing by zero.
        {
            let hosted = registry.torrents.get_mut(&info_hash).expect("hosted");
            hosted.downloaded = 0;
            assert_eq!(hosted.ratio_milli(), 0);
        }

        // A per-torrent ratio wins over the daemon's, and 0 means "follow it".
        {
            let hosted = registry.torrents.get_mut(&info_hash).expect("hosted");
            hosted.seed_ratio_milli = 500;
            assert_eq!(hosted.effective_seed_ratio(2000), 500);
            hosted.seed_ratio_milli = 0;
            assert_eq!(hosted.effective_seed_ratio(2000), 2000);
        }

        // Raising the limit on a torrent stopped for its ratio restarts it —
        // otherwise the operator raises it, nothing happens, and why is
        // invisible.
        {
            let hosted = registry.torrents.get_mut(&info_hash).expect("hosted");
            hosted.downloaded = 1000;
            hosted.uploaded = 5000;
            hosted.wanted = Wanted::Paused(Why::SeedRatio);
        }
        registry.set_seed_ratio(&info_hash, 9000).expect("raise");
        assert_eq!(registry.torrents[&info_hash].wanted, Wanted::Running);

        // But an operator's pause is not undone by it: only the daemon's own
        // stop is reversible this way.
        {
            let hosted = registry.torrents.get_mut(&info_hash).expect("hosted");
            hosted.wanted = Wanted::Paused(Why::Operator);
        }
        registry.set_seed_ratio(&info_hash, 0).expect("clear");
        assert_eq!(
            registry.torrents[&info_hash].wanted,
            Wanted::Paused(Why::Operator)
        );

        // The reason reaches the operator, which is the whole point of
        // carrying it on the state rather than beside it.
        {
            let hosted = registry.torrents.get_mut(&info_hash).expect("hosted");
            hosted.wanted = Wanted::Paused(Why::SeedIdle);
        }
        let detail = registry.detail(&info_hash).expect("detail");
        assert_eq!(
            detail
                .get("paused_because")
                .and_then(clove_core::json::Value::as_str),
            Some("stopped: no peers for the idle limit")
        );

        // And it survives a restart, which is when the question actually gets
        // asked. Written the way `enforce_seed_limits` writes it — the
        // periodic `persist_progress` would skip this torrent, since it has no
        // engine, which is exactly why the stop path persists for itself
        // rather than leaving it to the next tick.
        let hosted = &registry.torrents[&info_hash];
        registry
            .write_resume(&info_hash, hosted)
            .expect("persist the stop");
        let mut reopened =
            Registry::<MockDialer>::open(&data.0, Limits::default()).expect("reopen");
        let detail = reopened.detail(&info_hash).expect("detail after reopen");
        assert_eq!(
            detail
                .get("paused_because")
                .and_then(clove_core::json::Value::as_str),
            Some("stopped: no peers for the idle limit")
        );
    }

    #[test]
    fn rates_start_at_zero_and_need_two_readings() {
        let data = TempDir::new("rates");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        let (_, bytes) = fixture("rated");
        add_and_scan(&mut registry, &bytes);

        // A torrent with no engine reports no rate, and the first refresh
        // takes a baseline rather than dividing a lifetime total by a tick —
        // which would report a week's average as the current speed.
        let listed = registry.list();
        let first = &listed.as_array().expect("an array")[0];
        assert_eq!(
            first
                .get("up_rate")
                .and_then(clove_core::json::Value::as_u64),
            Some(0)
        );
        assert_eq!(
            first
                .get("down_rate")
                .and_then(clove_core::json::Value::as_u64),
            Some(0)
        );

        let totals = registry.totals();
        assert_eq!((totals.up_rate, totals.down_rate), (0, 0));
        assert_eq!(totals.peers, 0);
        assert_eq!(totals.peer_limit, Limits::default().peer_limit);
    }

    #[test]
    fn a_prefix_reaches_a_hosted_torrent_too() {
        // The pending map is only half the picture: the resolver has to see
        // fully added torrents, which is where every command but `remove`
        // actually operates.
        let data = TempDir::new("resolve-hosted");
        let mut registry = Registry::<MockDialer>::open(&data.0, Limits::default()).unwrap();
        let (_, bytes) = fixture("resolvable");
        let info_hash = add_and_scan(&mut registry, &bytes);
        let full = hex(&info_hash);

        assert_eq!(registry.resolve(&full).expect("exact"), info_hash);
        let prefix = &full[..6];
        assert_eq!(registry.resolve(prefix).expect("prefix"), info_hash);

        // And it stops resolving once the torrent is gone.
        registry.remove(&info_hash, false).expect("remove");
        assert!(matches!(
            registry.resolve(prefix),
            Err(ResolveError::NotFound)
        ));
    }
}
