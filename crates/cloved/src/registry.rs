//! The torrent registry: the daemon's set of hosted torrents and their
//! on-disk state (Phase F, `docs/PHASE-F.md`; format in `docs/STATE-FORMAT.md`).
//!
//! This slice manages membership and persistence — add, list, remove, and
//! reload-on-restart. The live engine (creating a `Torrent`, opening its
//! `Storage`, and attaching peers over SAM) wires in a later slice; until then
//! a hosted torrent's progress is whatever verified on disk when it was added.
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

use clove_core::bitfield::Bitfield;
use clove_core::json::Value;
use clove_core::metainfo::{self, MetaInfo};
use clove_core::resume::Resume;
use clove_core::storage::Storage;

/// The set of hosted torrents plus where their state lives.
pub(crate) struct Registry {
    state_dir: PathBuf,
    downloads_dir: PathBuf,
    torrents: BTreeMap<[u8; 20], Hosted>,
}

/// One hosted torrent's in-memory summary.
struct Hosted {
    meta: MetaInfo,
    have: u32,
    total_pieces: u32,
    priorities: Vec<u8>,
    uploaded: u64,
    downloaded: u64,
    paused: bool,
}

/// Why adding a torrent failed (mapped to an HTTP status by the caller).
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

impl Registry {
    /// Open the registry under `data_dir`, creating the `state`/`downloads`
    /// directories and loading any previously added torrents.
    ///
    /// # Errors
    ///
    /// The state or downloads directory cannot be created.
    pub(crate) fn open(data_dir: &Path) -> io::Result<Registry> {
        let state_dir = data_dir.join("state");
        let downloads_dir = data_dir.join("downloads");
        fs::create_dir_all(&state_dir)?;
        fs::create_dir_all(&downloads_dir)?;
        let mut registry = Registry {
            state_dir,
            downloads_dir,
            torrents: BTreeMap::new(),
        };
        registry.load_all();
        Ok(registry)
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
        let total_pieces = piece_count(&meta);
        let priorities = vec![1u8; meta.files.len()];

        let hex = hex(&info_hash);
        atomic_write(&self.state_dir.join(format!("{hex}.torrent")), bytes)
            .map_err(AddError::Io)?;
        let resume = Resume {
            info_hash,
            num_pieces: total_pieces,
            have: have.as_bytes().to_vec(),
            // verify_all only sets a bit once the piece hashes, so verified == have.
            verified: have.as_bytes().to_vec(),
            priorities: priorities.clone(),
            uploaded: 0,
            downloaded: 0,
            trackers: meta.trackers.clone(),
        };
        atomic_write(
            &self.state_dir.join(format!("{hex}.resume")),
            &resume.encode(),
        )
        .map_err(AddError::Io)?;

        self.torrents.insert(
            info_hash,
            Hosted {
                meta,
                have: have.count(),
                total_pieces,
                priorities,
                uploaded: 0,
                downloaded: 0,
                paused: false,
            },
        );
        Ok(info_hash)
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
    pub(crate) fn list(&self) -> Value {
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
            .map(|b| b.count())
            .map_err(|_| "resume have-bitfield is inconsistent".to_owned())?;
        self.torrents.insert(
            info_hash,
            Hosted {
                have,
                total_pieces: piece_count(&meta),
                priorities: resume.priorities,
                uploaded: resume.uploaded,
                downloaded: resume.downloaded,
                paused: false,
                meta,
            },
        );
        Ok(())
    }
}

impl Hosted {
    fn to_json(&self) -> Value {
        let progress = if self.total_pieces == 0 {
            0.0
        } else {
            f64::from(self.have) / f64::from(self.total_pieces)
        };
        let state = if self.paused {
            "paused"
        } else if self.have == self.total_pieces {
            "complete"
        } else {
            "downloading"
        };
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
            (
                "pieces".to_owned(),
                Value::UInt(u64::from(self.total_pieces)),
            ),
            ("have".to_owned(), Value::UInt(u64::from(self.have))),
            ("progress".to_owned(), Value::Float(progress)),
            ("uploaded".to_owned(), Value::UInt(self.uploaded)),
            ("downloaded".to_owned(), Value::UInt(self.downloaded)),
            ("state".to_owned(), Value::from(state)),
            ("priorities".to_owned(), Value::Array(priorities)),
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

/// Piece count of a torrent, saturating (any torrent clove accepts fits u32).
fn piece_count(meta: &MetaInfo) -> u32 {
    u32::try_from(meta.pieces.len()).unwrap_or(u32::MAX)
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
