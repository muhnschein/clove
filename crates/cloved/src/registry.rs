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

use clove_core::bitfield::Bitfield;
use clove_core::json::Value;
use clove_core::metainfo::{self, MetaInfo};
use clove_core::picker::Mode;
use clove_core::resume::Resume;
use clove_core::storage::Storage;
use clove_core::swarm::{InboundDemux, Swarm, SwarmConfig};
use clove_core::torrent::Torrent;
use i2pnet::{DestHash, I2pDialer};

/// The set of hosted torrents plus where their state lives.
pub(crate) struct Registry<D: I2pDialer + Clone + Send + 'static>
where
    D::Stream: 'static,
{
    state_dir: PathBuf,
    downloads_dir: PathBuf,
    torrents: BTreeMap<[u8; 20], Hosted>,
    network: Option<Network<D>>,
}

/// The attached network backend: everything needed to bring a torrent live.
struct Network<D> {
    dialer: D,
    demux: Arc<InboundDemux>,
    peer_id: [u8; 20],
    swarm_config: SwarmConfig,
}

/// One hosted torrent's in-memory summary.
struct Hosted {
    meta: MetaInfo,
    have: Bitfield,
    priorities: Vec<u8>,
    uploaded: u64,
    downloaded: u64,
    paused: bool,
    live: Option<Live>,
}

/// A torrent's running engine half.
struct Live {
    torrent: Arc<Torrent>,
    swarm: Swarm,
}

impl Hosted {
    /// The resume record to persist for this torrent's current state.
    fn resume(&self, info_hash: [u8; 20]) -> Resume {
        Resume {
            info_hash,
            num_pieces: self.have.len(),
            have: self.have.as_bytes().to_vec(),
            // We only mark a piece present once it verifies, so verified == have.
            verified: self.have.as_bytes().to_vec(),
            priorities: self.priorities.clone(),
            uploaded: self.uploaded,
            downloaded: self.downloaded,
            trackers: self.meta.trackers.clone(),
            paused: self.paused,
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
    /// A torrent with this info-hash is already hosted (409).
    Duplicate,
    /// A filesystem error (500).
    Io(io::Error),
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddError::Parse(e) => write!(f, "{e}"),
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

impl<D: I2pDialer + Clone + Send + 'static> Registry<D>
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
    ) {
        self.network = Some(Network {
            dialer,
            demux,
            peer_id,
            swarm_config,
        });
        let hashes: Vec<[u8; 20]> = self.torrents.keys().copied().collect();
        for info_hash in hashes {
            if let Err(e) = self.start_live(&info_hash) {
                eprintln!("cloved: starting {}: {e}", hex(&info_hash));
            }
        }
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
        if hosted.paused || hosted.live.is_some() {
            return Ok(());
        }
        let storage = Arc::new(Storage::create(&hosted.meta, &self.downloads_dir, false)?);
        let torrent = Torrent::new(
            &hosted.meta,
            storage,
            &hosted.have,
            Mode::RarestFirst,
            network.peer_id,
        );
        network.demux.register(&torrent);
        let swarm = Swarm::dial_only(
            Arc::clone(&torrent),
            network.dialer.clone(),
            network.swarm_config,
        );
        hosted.live = Some(Live { torrent, swarm });
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
        hosted.have = live.torrent.have();
        live.swarm.request_stop();
    }

    /// Refresh each live torrent's progress snapshot into its summary.
    fn refresh(&mut self) {
        for hosted in self.torrents.values_mut() {
            if let Some(live) = &hosted.live {
                hosted.have = live.torrent.have();
            }
        }
    }

    /// Snapshot every live torrent's progress and persist its resume record.
    /// Called periodically by the daemon and around lifecycle transitions.
    pub(crate) fn persist_progress(&mut self) {
        self.refresh();
        for (info_hash, hosted) in &self.torrents {
            if hosted.live.is_some()
                && let Err(e) =
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
    pub(crate) fn add_torrent(&mut self, bytes: &[u8]) -> Result<[u8; 20], AddError> {
        let meta = MetaInfo::parse(bytes).map_err(AddError::Parse)?;
        let info_hash = meta.info_hash.0;
        if self.torrents.contains_key(&info_hash) {
            return Err(AddError::Duplicate);
        }

        // Lay out the files and see what (if anything) is already on disk.
        let storage = Storage::create(&meta, &self.downloads_dir, false).map_err(AddError::Io)?;
        let have = storage.verify_all().map_err(AddError::Io)?;
        let priorities = vec![1u8; meta.files.len()];

        let hosted = Hosted {
            meta,
            have,
            priorities,
            uploaded: 0,
            downloaded: 0,
            paused: false,
            live: None,
        };
        let hex = hex(&info_hash);
        atomic_write(&self.state_dir.join(format!("{hex}.torrent")), bytes)
            .map_err(AddError::Io)?;
        self.write_resume(&info_hash, &hosted)
            .map_err(AddError::Io)?;
        self.torrents.insert(info_hash, hosted);
        // With a network attached, the torrent goes live immediately.
        self.start_live(&info_hash).map_err(AddError::Io)?;
        Ok(info_hash)
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
        let resume = hosted.resume(*info_hash);
        write_resume_file(&self.state_dir, info_hash, &resume).map_err(ActionError::Io)?;
        Ok(count)
    }

    /// Re-verify a torrent's data against the piece hashes on disk, updating
    /// and persisting its have set. Returns the verified piece count.
    ///
    /// # Errors
    ///
    /// [`ActionError::NotFound`] or a filesystem error.
    pub(crate) fn verify(&mut self, info_hash: &[u8; 20]) -> Result<u32, ActionError> {
        let hosted = self
            .torrents
            .get_mut(info_hash)
            .ok_or(ActionError::NotFound)?;
        if hosted.live.is_some() {
            return Err(ActionError::BadInput(
                "pause the torrent before verifying (it is actively writing)",
            ));
        }
        let storage =
            Storage::create(&hosted.meta, &self.downloads_dir, false).map_err(ActionError::Io)?;
        hosted.have = storage.verify_all().map_err(ActionError::Io)?;
        let count = hosted.have.count();
        let resume = hosted.resume(*info_hash);
        write_resume_file(&self.state_dir, info_hash, &resume).map_err(ActionError::Io)?;
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
        Value::Array(self.torrents.values().map(Hosted::to_json).collect())
    }

    /// Load every previously added torrent from the state directory. A file
    /// that cannot be loaded is logged and skipped, never fatal.
    fn load_all(&mut self) {
        let Ok(entries) = fs::read_dir(&self.state_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("torrent") {
                continue;
            }
            if let Err(e) = self.load_one(&path) {
                eprintln!("cloved: skipping {}: {e}", path.display());
            }
        }
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
        let have = Bitfield::from_bytes(&resume.have, resume.num_pieces)
            .map_err(|_| "resume have-bitfield is inconsistent".to_owned())?;
        self.torrents.insert(
            info_hash,
            Hosted {
                have,
                priorities: resume.priorities,
                uploaded: resume.uploaded,
                downloaded: resume.downloaded,
                paused: resume.paused,
                meta,
                live: None,
            },
        );
        Ok(())
    }
}

impl Hosted {
    /// The state string shown in listings.
    fn state(&self) -> &'static str {
        let complete = self.have.count() == self.have.len();
        if self.paused {
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
        if self.have.is_empty() {
            0.0
        } else {
            f64::from(self.have.count()) / f64::from(self.have.len())
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
            ("state".to_owned(), Value::from(self.state())),
            ("private".to_owned(), Value::Bool(self.meta.private)),
            ("files".to_owned(), Value::Array(files)),
            ("trackers".to_owned(), Value::Array(trackers)),
        ])
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
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clove_core::bencode::{self, Value as Ben};
    use clove_core::wire::BLOCK_LEN;
    use i2pnet::mock::{MockDialer, MockNet};
    use sha1::{Digest, Sha1};
    use std::collections::BTreeMap as Map;
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
        );

        let info_hash = registry.add_torrent(&bytes).unwrap();
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
        );
        let info_hash = registry.add_torrent(&bytes).unwrap();

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
        let info_hash = registry.add_torrent(&bytes).unwrap();
        let state = registry
            .list()
            .as_array()
            .and_then(|items| items.first().cloned())
            .and_then(|item| item.get("state").and_then(|s| s.as_str().map(String::from)));
        assert_eq!(state.as_deref(), Some("waiting-for-router"));
        assert!(registry.add_peer(&info_hash, DestHash([0xEE; 32])).is_err());
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
