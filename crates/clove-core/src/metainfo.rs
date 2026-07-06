//! .torrent metainfo parsing (BEP 3, BEP 12) for the I2P dialect.
//!
//! Announce URLs that are not I2P are counted and dropped at parse time —
//! never resolved, never stored, never logged beyond the skip count
//! (SCOPE §3). Path components are validated here so nothing downstream
//! has to reason about `..`, separators, or NUL bytes.

use std::fmt;

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};

/// SHA-1 of the raw bencoded `info` dictionary — the torrent's identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InfoHash(pub [u8; 20]);

impl fmt::Display for InfoHash {
    /// Lowercase hex, as used in logs and magnet links.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Smallest piece length clove accepts (16 KiB, the block size).
pub const MIN_PIECE_LENGTH: u32 = 16 * 1024;
/// Largest piece length clove accepts (128 MiB).
pub const MAX_PIECE_LENGTH: u32 = 128 * 1024 * 1024;

/// One file within a torrent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Validated path components relative to the download root. For a
    /// single-file torrent this is `[name]`; for multi-file torrents it is
    /// `[name, subdir…, file]` — storage joins them under the data
    /// directory either way.
    pub path: Vec<String>,
    /// File size in bytes; zero-length files are legal.
    pub length: u64,
}

/// A validated .torrent.
#[derive(Clone, Debug)]
pub struct MetaInfo {
    /// The torrent's identity on trackers and the wire.
    pub info_hash: InfoHash,
    /// The `info.name` field: torrent display name and root path component.
    pub name: String,
    /// Bytes per piece (except possibly the last).
    pub piece_length: u32,
    /// SHA-1 expectation for every piece, in order.
    pub pieces: Vec<[u8; 20]>,
    /// The torrent's files, in on-wire order.
    pub files: Vec<FileEntry>,
    /// Sum of all file lengths.
    pub total_length: u64,
    /// BEP 27 private flag (`info.private == 1`); common on I2P.
    pub private: bool,
    /// Announce URL tiers (BEP 12) after the I2P-only filter. May be empty:
    /// a torrent can still be joined via PEX or magnet paths.
    pub trackers: Vec<Vec<String>>,
    /// How many non-I2P announce URLs were dropped. The only trace they
    /// leave: callers may log "skipped N non-I2P trackers", nothing more.
    pub skipped_trackers: usize,
}

/// Why a .torrent was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not valid bencode.
    Bencode(bencode::Error),
    /// Structurally invalid; the message names the offending field.
    Invalid(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bencode(e) => write!(f, "torrent: {e}"),
            Error::Invalid(what) => write!(f, "torrent: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<bencode::Error> for Error {
    fn from(e: bencode::Error) -> Self {
        Error::Bencode(e)
    }
}

impl MetaInfo {
    /// Build a [`MetaInfo`] from a bare `info` dictionary — the bytes fetched
    /// over BEP 9 for a magnet link, which have no surrounding torrent dict
    /// or trackers.
    ///
    /// The info-hash is computed over `info_bytes` exactly (so it matches the
    /// magnet's `btih`); the caller should compare it to the expected hash.
    /// The result has no trackers (peers come from the magnet's `tr=` list,
    /// PEX, or DHT later).
    ///
    /// # Errors
    ///
    /// The same structural errors as [`parse`](Self::parse); the bytes must
    /// be a valid, self-consistent info dictionary.
    pub fn from_info_dict(info_bytes: &[u8]) -> Result<Self, Error> {
        // Wrap as `d4:info<info_bytes>e` and reuse the validated path. The
        // wrapper's raw `info` span is exactly `info_bytes`, so the info-hash
        // is unchanged.
        let mut torrent = Vec::with_capacity(info_bytes.len() + 8);
        torrent.extend_from_slice(b"d4:info");
        torrent.extend_from_slice(info_bytes);
        torrent.push(b'e');
        Self::parse(&torrent)
    }

    /// Parse and validate a .torrent file.
    ///
    /// # Errors
    ///
    /// Malformed bencode, or any structural violation: missing/mistyped
    /// fields, piece length out of `MIN_PIECE_LENGTH..=MAX_PIECE_LENGTH`,
    /// piece count disagreeing with the total length, or unsafe path
    /// components.
    pub fn parse(input: &[u8]) -> Result<Self, Error> {
        let root = bencode::decode(input)?;
        let info = root
            .get(b"info")
            .ok_or(Error::Invalid("missing info dictionary"))?;
        if info.as_dict().is_none() {
            return Err(Error::Invalid("info is not a dictionary"));
        }

        // Hash the exact bytes: re-encoding non-canonical input would
        // change the identity i2psnark peers agreed on.
        let info_range =
            bencode::raw_entry(input, b"info")?.ok_or(Error::Invalid("missing info dictionary"))?;
        let info_hash = InfoHash(Sha1::digest(&input[info_range]).into());

        let name = info
            .get(b"name")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid("missing or non-UTF-8 name"))?;
        check_component(name)?;

        let piece_length_raw = info
            .get(b"piece length")
            .and_then(Value::as_int)
            .ok_or(Error::Invalid("missing piece length"))?;
        let piece_length = u32::try_from(piece_length_raw)
            .ok()
            .filter(|&n| (MIN_PIECE_LENGTH..=MAX_PIECE_LENGTH).contains(&n))
            .ok_or(Error::Invalid("piece length out of accepted range"))?;

        let pieces_raw = info
            .get(b"pieces")
            .and_then(Value::as_bytes)
            .ok_or(Error::Invalid("missing pieces"))?;
        if pieces_raw.is_empty() || pieces_raw.len() % 20 != 0 {
            return Err(Error::Invalid("pieces is not a multiple of 20 bytes"));
        }
        let pieces: Vec<[u8; 20]> = pieces_raw
            .chunks_exact(20)
            .map(|chunk| {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(chunk);
                hash
            })
            .collect();

        let (files, total_length) = parse_files(info, name)?;
        let expected_pieces = total_length.div_ceil(u64::from(piece_length));
        if expected_pieces != pieces.len() as u64 {
            return Err(Error::Invalid("piece count disagrees with total length"));
        }

        let private = info.get(b"private").and_then(Value::as_int) == Some(1);
        let (trackers, skipped_trackers) = parse_trackers(&root)?;

        Ok(MetaInfo {
            info_hash,
            name: name.to_owned(),
            piece_length,
            pieces,
            files,
            total_length,
            private,
            trackers,
            skipped_trackers,
        })
    }
}

/// True when `url` announces to an I2P destination: `http(s)://` with a
/// host ending in `.i2p` (which covers `.b32.i2p`). Anything else — IPs,
/// clearnet hosts, other schemes — is not ours to talk to.
#[must_use]
pub fn is_i2p_tracker(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.contains('@') {
        return false; // userinfo has no business in an announce URL
    }
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => authority,
    };
    host.to_ascii_lowercase().ends_with(".i2p")
}

fn parse_files(info: &Value, name: &str) -> Result<(Vec<FileEntry>, u64), Error> {
    let single = info.get(b"length");
    let multi = info.get(b"files");
    match (single, multi) {
        (Some(_), Some(_)) => Err(Error::Invalid("both length and files present")),
        (None, None) => Err(Error::Invalid("neither length nor files present")),
        (Some(len), None) => {
            let length = as_size(len).ok_or(Error::Invalid("bad file length"))?;
            if length == 0 {
                return Err(Error::Invalid("single-file torrent of zero bytes"));
            }
            let entry = FileEntry {
                path: vec![name.to_owned()],
                length,
            };
            Ok((vec![entry], length))
        }
        (None, Some(files)) => {
            let list = files
                .as_list()
                .ok_or(Error::Invalid("files is not a list"))?;
            if list.is_empty() {
                return Err(Error::Invalid("files list is empty"));
            }
            let mut entries = Vec::with_capacity(list.len());
            let mut total: u64 = 0;
            for file in list {
                let length = file
                    .get(b"length")
                    .and_then(as_size)
                    .ok_or(Error::Invalid("bad file length"))?;
                let raw_path = file
                    .get(b"path")
                    .and_then(Value::as_list)
                    .ok_or(Error::Invalid("bad file path"))?;
                if raw_path.is_empty() {
                    return Err(Error::Invalid("bad file path"));
                }
                let mut path = Vec::with_capacity(raw_path.len() + 1);
                path.push(name.to_owned());
                for part in raw_path {
                    let part = part
                        .as_str()
                        .ok_or(Error::Invalid("non-UTF-8 path component"))?;
                    check_component(part)?;
                    path.push(part.to_owned());
                }
                total = total
                    .checked_add(length)
                    .ok_or(Error::Invalid("total length overflows"))?;
                entries.push(FileEntry { path, length });
            }
            if total == 0 {
                return Err(Error::Invalid("torrent of zero total bytes"));
            }
            Ok((entries, total))
        }
    }
}

fn parse_trackers(root: &Value) -> Result<(Vec<Vec<String>>, usize), Error> {
    let mut tiers = Vec::new();
    let mut skipped = 0usize;
    if let Some(list) = root.get(b"announce-list") {
        // BEP 12: list of tiers, each a list of URLs. Takes precedence
        // over the flat announce key.
        let list = list
            .as_list()
            .ok_or(Error::Invalid("announce-list is not a list"))?;
        for tier in list {
            let tier = tier
                .as_list()
                .ok_or(Error::Invalid("announce-list tier is not a list"))?;
            let mut kept = Vec::new();
            for url in tier {
                let url = url
                    .as_str()
                    .ok_or(Error::Invalid("announce URL is not a UTF-8 string"))?;
                if is_i2p_tracker(url) {
                    kept.push(url.to_owned());
                } else {
                    skipped += 1;
                }
            }
            if !kept.is_empty() {
                tiers.push(kept);
            }
        }
    } else if let Some(url) = root.get(b"announce") {
        let url = url
            .as_str()
            .ok_or(Error::Invalid("announce URL is not a UTF-8 string"))?;
        if is_i2p_tracker(url) {
            tiers.push(vec![url.to_owned()]);
        } else {
            skipped += 1;
        }
    }
    Ok((tiers, skipped))
}

fn as_size(v: &Value) -> Option<u64> {
    v.as_int().and_then(|n| u64::try_from(n).ok())
}

/// A path component usable under the data directory: nonempty, no
/// separators, not `.`/`..`, no NUL.
fn check_component(s: &str) -> Result<(), Error> {
    let bad = s.is_empty()
        || s == "."
        || s == ".."
        || s.contains('/')
        || s.contains('\\')
        || s.contains('\0');
    if bad {
        Err(Error::Invalid("unsafe path component"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::encode;
    use std::collections::BTreeMap;

    fn bval(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }

    fn dict(entries: Vec<(&str, Value)>) -> Value {
        let map: BTreeMap<Vec<u8>, Value> = entries
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v))
            .collect();
        Value::Dict(map)
    }

    /// A minimal valid single-file torrent: 5 bytes, one piece.
    fn single_file(extra_root: Vec<(&str, Value)>) -> Vec<u8> {
        let info = dict(vec![
            ("name", bval("hello.txt")),
            ("piece length", Value::Int(i64::from(MIN_PIECE_LENGTH))),
            ("pieces", Value::Bytes(vec![0xab; 20])),
            ("length", Value::Int(5)),
        ]);
        let mut root = vec![("info", info)];
        root.extend(extra_root);
        encode(&dict(root))
    }

    #[test]
    fn parses_single_file() {
        let input = single_file(vec![(
            "announce",
            bval("http://tracker2.postman.i2p/announce.php"),
        )]);
        let t = MetaInfo::parse(&input).unwrap();
        assert_eq!(t.name, "hello.txt");
        assert_eq!(t.total_length, 5);
        assert_eq!(t.pieces.len(), 1);
        assert_eq!(
            t.files,
            vec![FileEntry {
                path: vec!["hello.txt".into()],
                length: 5
            }]
        );
        assert_eq!(
            t.trackers,
            vec![vec!["http://tracker2.postman.i2p/announce.php".to_owned()]]
        );
        assert_eq!(t.skipped_trackers, 0);
        assert!(!t.private);
        assert_eq!(t.info_hash.to_string().len(), 40);
    }

    #[test]
    fn info_hash_covers_raw_bytes() {
        // Same info dict, different outer keys: identical hash.
        let a = MetaInfo::parse(&single_file(vec![])).unwrap();
        let b = MetaInfo::parse(&single_file(vec![("comment", bval("x"))])).unwrap();
        assert_eq!(a.info_hash, b.info_hash);
    }

    #[test]
    fn parses_multi_file_and_checks_totals() {
        let piece_len = i64::from(MIN_PIECE_LENGTH);
        let files = Value::List(vec![
            dict(vec![
                ("length", Value::Int(i64::from(MIN_PIECE_LENGTH))),
                ("path", Value::List(vec![bval("sub"), bval("a.bin")])),
            ]),
            dict(vec![
                ("length", Value::Int(3)),
                ("path", Value::List(vec![bval("b.bin")])),
            ]),
        ]);
        let info = dict(vec![
            ("name", bval("album")),
            ("piece length", Value::Int(piece_len)),
            ("pieces", Value::Bytes(vec![0u8; 40])),
            ("files", files),
        ]);
        let input = encode(&dict(vec![("info", info)]));
        let t = MetaInfo::parse(&input).unwrap();
        assert_eq!(t.total_length, u64::from(MIN_PIECE_LENGTH) + 3);
        assert_eq!(t.files[0].path, vec!["album", "sub", "a.bin"]);
        assert_eq!(t.files[1].path, vec!["album", "b.bin"]);
    }

    #[test]
    fn i2p_tracker_filter() {
        assert!(is_i2p_tracker("http://tracker2.postman.i2p/announce.php"));
        assert!(is_i2p_tracker(
            "http://mb5ir7klpc2tj6ha3xhmrs3mseqvanauciuoiamx24mmomvkhaua.b32.i2p/a"
        ));
        assert!(is_i2p_tracker("http://opentracker.dg2.i2p:80/announce"));
        assert!(is_i2p_tracker("https://TRACKER.EXAMPLE.I2P/announce"));

        assert!(!is_i2p_tracker("http://tracker.example.org/announce"));
        assert!(!is_i2p_tracker("udp://tracker.example.i2p/announce"));
        assert!(!is_i2p_tracker("http://1.2.3.4:6969/announce"));
        assert!(!is_i2p_tracker("http://evil.example@host.i2p/announce"));
        assert!(!is_i2p_tracker("http://example.i2p.example.org/announce"));
        assert!(!is_i2p_tracker("example.i2p/announce"));
    }

    #[test]
    fn filters_and_counts_non_i2p_trackers() {
        let tiers = Value::List(vec![
            Value::List(vec![
                bval("http://tracker.example.org/announce"),
                bval("http://good.i2p/announce"),
            ]),
            Value::List(vec![bval("udp://other.example.org/announce")]),
        ]);
        let input = single_file(vec![("announce-list", tiers)]);
        let t = MetaInfo::parse(&input).unwrap();
        assert_eq!(
            t.trackers,
            vec![vec!["http://good.i2p/announce".to_owned()]]
        );
        assert_eq!(t.skipped_trackers, 2);
    }

    #[test]
    fn tracker_less_torrent_is_fine() {
        let t = MetaInfo::parse(&single_file(vec![])).unwrap();
        assert!(t.trackers.is_empty());
    }

    #[test]
    fn rejects_structural_garbage() {
        // Each case mutates the minimal torrent in one hostile way.
        let cases: Vec<Value> = vec![
            // missing info
            dict(vec![("announce", bval("http://t.i2p/a"))]),
            // piece length too small
            dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("x")),
                    ("piece length", Value::Int(1024)),
                    ("pieces", Value::Bytes(vec![0; 20])),
                    ("length", Value::Int(5)),
                ]),
            )]),
            // pieces not a multiple of 20
            dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("x")),
                    ("piece length", Value::Int(i64::from(MIN_PIECE_LENGTH))),
                    ("pieces", Value::Bytes(vec![0; 19])),
                    ("length", Value::Int(5)),
                ]),
            )]),
            // piece count disagrees with length
            dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("x")),
                    ("piece length", Value::Int(i64::from(MIN_PIECE_LENGTH))),
                    ("pieces", Value::Bytes(vec![0; 40])),
                    ("length", Value::Int(5)),
                ]),
            )]),
            // path traversal
            dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("x")),
                    ("piece length", Value::Int(i64::from(MIN_PIECE_LENGTH))),
                    ("pieces", Value::Bytes(vec![0; 20])),
                    (
                        "files",
                        Value::List(vec![dict(vec![
                            ("length", Value::Int(5)),
                            ("path", Value::List(vec![bval(".."), bval("etc")])),
                        ])]),
                    ),
                ]),
            )]),
            // name with a separator
            dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("a/b")),
                    ("piece length", Value::Int(i64::from(MIN_PIECE_LENGTH))),
                    ("pieces", Value::Bytes(vec![0; 20])),
                    ("length", Value::Int(5)),
                ]),
            )]),
            // negative length
            dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("x")),
                    ("piece length", Value::Int(i64::from(MIN_PIECE_LENGTH))),
                    ("pieces", Value::Bytes(vec![0; 20])),
                    ("length", Value::Int(-5)),
                ]),
            )]),
        ];
        for (i, case) in cases.iter().enumerate() {
            let input = encode(case);
            assert!(
                matches!(MetaInfo::parse(&input), Err(Error::Invalid(_))),
                "case {i} was accepted"
            );
        }
        assert!(matches!(
            MetaInfo::parse(b"not bencode"),
            Err(Error::Bencode(_))
        ));
    }
}
