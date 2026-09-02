//! .torrent metainfo parsing (BEP 3, BEP 12) for the I2P dialect.
//!
//! Announce URLs that are not I2P are counted and dropped at parse time —
//! never resolved, never stored, never logged beyond the skip count
//! (SCOPE §3). Path components are validated here so nothing downstream
//! has to reason about `..`, separators, or NUL bytes.

use std::fmt;
use std::sync::Arc;

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

/// Most files a torrent may list.
///
/// The daemon opens every file of every hosted torrent at once, so a file is a
/// descriptor for as long as the torrent is up, and the service units pin
/// `RLIMIT_NOFILE` at 8192. A `.torrent` is a hostile surface (`SECURITY.md`),
/// and one listing a few hundred thousand entries would take the SAM sockets
/// down with it. A hundred thousand is more than any real torrent carries and
/// still a number an operator can reason about against their limits.
pub const MAX_FILES: usize = 100_000;

/// Longest path component a torrent may name, in bytes — `NAME_MAX` on
/// Linux and every filesystem clove is likely to meet. Refused here so the
/// `ENAMETOOLONG` surfaces at the torrent rather than at the first block
/// written, far from its cause.
pub const MAX_COMPONENT_BYTES: usize = 255;

/// Longest file path a torrent may name below the download root, in bytes
/// with separators — `PATH_MAX`, less whatever the root itself takes, which
/// is the operator's to keep short.
pub const MAX_PATH_BYTES: usize = 4096;

/// Most announce URLs a torrent keeps, counted across every tier.
///
/// Each kept URL is a naming lookup for the router and an announce per cycle
/// for the announcer, one after another. A 2 MiB `.torrent` can carry some
/// fifty thousand `.i2p` URLs, which is days per announce cycle and fifty
/// thousand lookups — for a swarm reached through a handful of them at
/// most. Sixteen covers every real announce-list; the rest are counted as
/// skipped, the way a non-I2P URL is.
pub const MAX_TRACKERS: usize = 16;

/// Most pieces a torrent may have: as many as a `bitfield` message can carry.
///
/// The wire caps a message body at [`crate::wire::MAX_MESSAGE_LEN`], and a
/// bitfield is one id byte followed by a bit per piece. A torrent with more
/// pieces than that could be added but never exchange a bitfield with anyone,
/// which is a torrent that silently never starts; refusing it here says why.
pub const MAX_PIECES: u32 = (crate::wire::MAX_MESSAGE_LEN - 1) * 8;

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
    ///
    /// Shared, not owned: [`Storage`](crate::storage::Storage) wants the same
    /// list, nothing mutates it after parsing, and 20 bytes per piece per copy
    /// adds up across a catalogue of torrents.
    pub pieces: Arc<[[u8; 20]]>,
    /// The torrent's files, in on-wire order.
    pub files: Vec<FileEntry>,
    /// Sum of all file lengths.
    pub total_length: u64,
    /// BEP 27 private flag (`info.private == 1`); common on I2P.
    pub private: bool,
    /// Announce URL tiers (BEP 12) after the I2P-only filter, at most
    /// [`MAX_TRACKERS`] URLs across them. May be empty: a torrent can still
    /// be joined via PEX or magnet paths.
    pub trackers: Vec<Vec<String>>,
    /// How many announce URLs were dropped — non-I2P ones, and I2P ones past
    /// [`MAX_TRACKERS`]. The only trace they leave: callers may log "skipped
    /// N trackers", nothing more.
    pub skipped_trackers: usize,
    /// The raw bencoded `info` dictionary these fields came from — the exact
    /// bytes the info-hash covers. Kept so we can serve BEP 9 metadata to
    /// magnet peers and re-emit the torrent without re-encoding.
    ///
    /// Shared for the same reason as [`pieces`](MetaInfo::pieces), and the
    /// larger of the two: this dictionary is mostly the piece hashes again.
    pub raw_info: Arc<[u8]>,
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
    /// Turn per-file priorities into per-piece ones: each piece takes the
    /// highest priority among the files it overlaps.
    ///
    /// Files do not respect piece boundaries, so a piece can hold the tail of
    /// one file and the head of the next. Taking the maximum is what makes
    /// skipping safe: a piece shared between a skipped file and a wanted one
    /// stays wanted, because the wanted file cannot be completed without it.
    /// The cost is that the skipped file receives those bytes anyway — its
    /// first and last piece's worth — which is the same bargain every client
    /// making this offer strikes, and the alternative is a file that can never
    /// finish.
    ///
    /// `per_file` shorter than the file list leaves the remaining files at
    /// normal priority; longer, and the extra entries are ignored. The daemon
    /// validates the length before it ever gets here, so this is deliberately
    /// total for the one caller that cannot: a resume file written by another
    /// version, which must not be able to make a torrent unloadable.
    #[must_use]
    pub fn piece_priorities(&self, per_file: &[u8]) -> Vec<u8> {
        let piece_length = u64::from(self.piece_length);
        let mut out = vec![0u8; self.pieces.len()];
        if piece_length == 0 {
            return out;
        }
        let mut offset = 0u64;
        for (i, file) in self.files.iter().enumerate() {
            let priority = per_file.get(i).copied().unwrap_or(1);
            let start = offset;
            offset = offset.saturating_add(file.length);
            if priority == 0 || file.length == 0 {
                continue;
            }
            // The half-open byte range [start, offset) maps to pieces
            // [start / plen, (offset - 1) / plen].
            let first = start / piece_length;
            let last = (offset - 1) / piece_length;
            for piece in first..=last {
                let Ok(idx) = usize::try_from(piece) else {
                    break;
                };
                let Some(slot) = out.get_mut(idx) else {
                    break;
                };
                *slot = (*slot).max(priority);
            }
        }
        out
    }

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
        // One pass for both the tree and the span of the `info` entry. This
        // used to decode twice — once for the tree and once to find the
        // bytes — with the first tree alive throughout, which doubled the
        // transient cost of the largest input a peer can hand us.
        let (root, info_range) = bencode::decode_with_entry(input, b"info")?;
        let info = root
            .get(b"info")
            .ok_or(Error::Invalid("missing info dictionary"))?;
        if info.as_dict().is_none() {
            return Err(Error::Invalid("info is not a dictionary"));
        }

        // Hash the exact bytes: re-encoding non-canonical input would
        // change the identity i2psnark peers agreed on.
        let info_range = info_range.ok_or(Error::Invalid("missing info dictionary"))?;
        let raw_info: Arc<[u8]> = Arc::from(&input[info_range]);
        let info_hash = InfoHash(Sha1::digest(&raw_info).into());

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
        let pieces: Arc<[[u8; 20]]> = pieces_raw
            .chunks_exact(20)
            .map(|chunk| {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(chunk);
                hash
            })
            .collect();

        let (files, total_length) = parse_files(info, name)?;
        let expected_pieces = total_length.div_ceil(u64::from(piece_length));
        // Checked on the count the lengths imply, before it is compared with
        // the hash list: the message is then about the cap rather than about
        // a disagreement, and the check costs nothing however long the hash
        // list is.
        if expected_pieces > u64::from(MAX_PIECES) {
            return Err(Error::Invalid(
                "more pieces than a bitfield message can carry",
            ));
        }
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
            raw_info,
        })
    }
}

/// An announce URL that has been parsed and accepted: `http://` to a host
/// ending in `.i2p`, with everything a request needs already separated out.
///
/// One parse for both questions clove asks of a tracker URL — "is this ours to
/// talk to" and "what request does it mean" — because two parsers that
/// disagree let a URL through the filter that the builder then reads
/// differently. They did: the filter cut the authority at `/`, `?` or `#`
/// while the builder cut only at `/`, so `http://tracker.i2p?x=1` was accepted
/// as a tracker and then dialed as a *host* named `tracker.i2p?x=1`, and a
/// port survived the filter only to be handed to naming lookup as part of the
/// hostname. Neither could work, and both failed far from the URL that caused
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerUrl {
    /// The host alone, lowercased — what SAM naming resolves. Never carries a
    /// port: the router has no use for one, and I2P destinations do not have
    /// them.
    pub host: String,
    /// The authority as written, host plus any port, for the `Host` header.
    pub authority: String,
    /// Path and query, always beginning with `/`.
    pub path_and_query: String,
}

impl TrackerUrl {
    /// Parse an announce URL, or `None` if clove has no business dialing it.
    ///
    /// Accepts `http://` only. `https://` is refused deliberately: clove speaks
    /// plain HTTP over an I2P stream (the tunnel is the encryption) and has no
    /// TLS stack to speak anything else with, so a URL kept here that the
    /// announcer could never dial would fail forever with nothing to show the
    /// operator.
    ///
    /// Also refused, and each for a reason a `.torrent` from a stranger makes
    /// concrete:
    ///
    /// - **Control characters and whitespace anywhere.** The path lands in a
    ///   request line verbatim, so a `\r\n` in it appends headers of the
    ///   sender's choosing to an announce we sign our name to — and the same
    ///   URL is written to a log, where a newline forges a line.
    /// - **Userinfo** (`user@host`): no business in an announce URL, and it
    ///   hides the real host behind something that reads like one.
    /// - **Fragments**: never sent to a server, so keeping one means the URL we
    ///   dial is not the URL we were given.
    /// - **Malformed percent escapes**, an empty host label, and a port that is
    ///   not a number in range.
    #[must_use]
    pub fn parse(url: &str) -> Option<TrackerUrl> {
        let rest = url.strip_prefix("http://")?;
        if rest.bytes().any(|b| b <= 0x20 || b == 0x7f) {
            return None;
        }
        if rest.contains('#') {
            return None;
        }
        let (authority, path_and_query) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_owned()),
            None => match rest.find('?') {
                // A query with no path: the target is `/?…`, not a host called
                // `tracker.i2p?x=1`. This is the split the two old parsers
                // disagreed about.
                Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
                None => (rest, "/".to_owned()),
            },
        };
        if authority.contains('@') {
            return None;
        }
        if !valid_percent_escapes(&path_and_query) {
            return None;
        }

        // Split a trailing `:port`. Anything else after a colon is not an
        // authority we understand, rather than a host to try anyway.
        let host = match authority.rsplit_once(':') {
            Some((host, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                port.parse::<u16>().ok().filter(|&p| p > 0)?;
                host
            }
            None => authority,
        };
        // By label rather than by suffix: `.i2p` has to be the last *label*,
        // every label has to be non-empty, and there has to be one in front of
        // it — so `i2p`, `.i2p` and `a..i2p` are all refused, and a host that
        // merely ends in those characters is not mistaken for one.
        let host = host.to_ascii_lowercase();
        let mut labels = host.rsplit('.');
        if labels.next() != Some("i2p") {
            return None;
        }
        if labels.next().is_none_or(str::is_empty) {
            return None;
        }
        if host.split('.').any(str::is_empty) {
            return None;
        }
        Some(TrackerUrl {
            host,
            authority: authority.to_owned(),
            path_and_query,
        })
    }
}

/// Whether every `%` in `s` begins a complete two-hex-digit escape.
fn valid_percent_escapes(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let Some(pair) = bytes.get(i + 1..i + 3) else {
                return false;
            };
            if !pair.iter().all(u8::is_ascii_hexdigit) {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// True when `url` announces to an I2P destination: `http://` with a host
/// ending in `.i2p` (which covers `.b32.i2p`). Anything else — IPs, clearnet
/// hosts, other schemes — is not ours to talk to.
///
/// Exactly [`TrackerUrl::parse`] succeeding, which is what keeps this and
/// [`crate::tracker::build_announce`] in the agreement they claim: a tracker we
/// cannot talk to is dropped at parse time and counted, like any other.
#[must_use]
pub fn is_i2p_tracker(url: &str) -> bool {
    TrackerUrl::parse(url).is_some()
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
            if list.len() > MAX_FILES {
                return Err(Error::Invalid("too many files"));
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
                // Each component fits a name; the whole has to fit a path.
                let bytes: usize = path.iter().map(|part| part.len() + 1).sum();
                if bytes > MAX_PATH_BYTES {
                    return Err(Error::Invalid("file path longer than a path can be"));
                }
                total = total
                    .checked_add(length)
                    .ok_or(Error::Invalid("total length overflows"))?;
                entries.push(FileEntry { path, length });
            }
            if total == 0 {
                return Err(Error::Invalid("torrent of zero total bytes"));
            }
            check_distinct_paths(&entries)?;
            Ok((entries, total))
        }
    }
}

fn parse_trackers(root: &Value) -> Result<(Vec<Vec<String>>, usize), Error> {
    let mut tiers: Vec<Vec<String>> = Vec::new();
    let mut skipped = 0usize;
    let mut kept_total = 0usize;
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
                if is_i2p_tracker(url) && kept_total < MAX_TRACKERS {
                    kept.push(url.to_owned());
                    kept_total += 1;
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

/// Reject a file list whose paths collide on disk.
///
/// Two entries with the same path alias the same file at different offsets in
/// the torrent's byte space: their writes overwrite each other, the pieces over
/// them can never verify, and the download retries forever against every peer
/// it meets. A path that is a strict prefix of another is the same problem in
/// the other direction — one entry wants a file where another wants a
/// directory — and fails at file-creation time instead.
///
/// Sorting makes this a scan of neighbours: if one path is a prefix of another,
/// everything sorting between them shares that prefix too, so a collision
/// always shows up in an adjacent pair.
///
/// Compared case-folded, because the filesystem may: `A.txt` and `a.txt` are
/// one file on a case-insensitive volume, which is the aliasing this exists
/// to refuse, and a torrent that only works on a case-sensitive disk is not
/// worth accepting on one. Unicode normalisation (NFC against NFD) is the
/// same problem again on some filesystems and is *not* folded here — clove
/// carries no normalisation tables, and a lower-casing is what `str` can do
/// without one.
fn check_distinct_paths(entries: &[FileEntry]) -> Result<(), Error> {
    let mut paths: Vec<Vec<String>> = entries
        .iter()
        .map(|e| e.path.iter().map(|part| part.to_lowercase()).collect())
        .collect();
    paths.sort_unstable();
    for pair in paths.windows(2) {
        if pair[0] == pair[1] {
            return Err(Error::Invalid(
                "two files share the same path, up to letter case",
            ));
        }
        if pair[1].starts_with(&pair[0]) {
            return Err(Error::Invalid("a file path is also a directory path"));
        }
    }
    Ok(())
}

/// A path component usable under the data directory: nonempty, no
/// separators, not `.`/`..`, no NUL, and no longer than a filename can be.
fn check_component(s: &str) -> Result<(), Error> {
    let bad = s.is_empty()
        || s == "."
        || s == ".."
        || s.contains('/')
        || s.contains('\\')
        || s.contains('\0');
    if bad {
        return Err(Error::Invalid("unsafe path component"));
    }
    if s.len() > MAX_COMPONENT_BYTES {
        return Err(Error::Invalid(
            "path component longer than a filename can be",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::encode;
    use std::collections::BTreeMap;
    use std::io::Write as _;

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

        // The hash is SHA-1 over the exact `info` bytes as they sit in the
        // input, which for a non-canonical torrent — unsorted keys, `info`
        // ahead of `announce`, an unsorted key order inside `info` too — are
        // not the bytes a re-encode would produce. Written by hand so that
        // the encoder cannot have canonicalised them first.
        let raw_info = b"d6:pieces20:\x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\
                         \x01\x01\x01\x01\x01\x01\x01\x01\x01\x01\
                         12:piece lengthi16384e4:name9:hello.txt6:lengthi5ee";
        let mut input = b"d4:info".to_vec();
        input.extend_from_slice(raw_info);
        input.extend_from_slice(b"8:announce21:http://t.i2p/announcee");
        let t = MetaInfo::parse(&input).expect("a non-canonical torrent parses");
        assert_eq!(&t.raw_info[..], &raw_info[..]);
        assert_eq!(t.info_hash.0, <[u8; 20]>::from(Sha1::digest(raw_info)));
        assert_ne!(
            &bencode::encode(&bencode::decode(raw_info).unwrap())[..],
            &raw_info[..],
            "the input has to be non-canonical for this to prove anything"
        );
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
        assert!(is_i2p_tracker("http://TRACKER.EXAMPLE.I2P/announce"));

        // https is not ours to speak: no TLS stack, and the announcer would
        // reject the URL later anyway. Dropped at parse time instead.
        assert!(!is_i2p_tracker("https://tracker.example.i2p/announce"));
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

    /// Every kept URL is a naming lookup and an announce per cycle; a
    /// torrent does not get to make that fifty thousand of each.
    #[test]
    fn caps_kept_trackers_across_tiers() {
        let tier = |from: usize, n: usize| {
            Value::List(
                (from..from + n)
                    .map(|i| bval(&format!("http://t{i}.i2p/announce")))
                    .collect(),
            )
        };
        let tiers = Value::List(vec![tier(0, 10), tier(10, 10), tier(20, 10)]);
        let input = single_file(vec![("announce-list", tiers)]);
        let t = MetaInfo::parse(&input).unwrap();
        let kept: usize = t.trackers.iter().map(Vec::len).sum();
        assert_eq!(kept, MAX_TRACKERS);
        // The first tier whole, the second cut, the third gone: the cap
        // counts across tiers, not per tier.
        assert_eq!(t.trackers.len(), 2);
        assert_eq!(t.trackers[0].len(), 10);
        assert_eq!(t.trackers[1].len(), MAX_TRACKERS - 10);
        assert_eq!(t.trackers[1][0], "http://t10.i2p/announce");
        assert_eq!(t.skipped_trackers, 30 - MAX_TRACKERS);

        // Exactly the cap is kept whole, and a clearnet URL does not use up
        // a slot — it was never a candidate.
        let mut at_cap = vec![tier(0, MAX_TRACKERS)];
        at_cap.push(Value::List(vec![bval("http://tracker.example.org/a")]));
        let input = single_file(vec![("announce-list", Value::List(at_cap))]);
        let t = MetaInfo::parse(&input).unwrap();
        assert_eq!(t.trackers.iter().map(Vec::len).sum::<usize>(), MAX_TRACKERS);
        assert_eq!(t.skipped_trackers, 1);
    }

    #[test]
    fn tracker_less_torrent_is_fine() {
        let t = MetaInfo::parse(&single_file(vec![])).unwrap();
        assert!(t.trackers.is_empty());
    }

    #[test]
    fn rejects_colliding_file_paths() {
        let piece_len = i64::from(MIN_PIECE_LENGTH);
        let file = |len: i64, parts: Vec<&str>| {
            dict(vec![
                ("length", Value::Int(len)),
                (
                    "path",
                    Value::List(parts.into_iter().map(bval).collect::<Vec<_>>()),
                ),
            ])
        };
        let torrent = |files: Vec<Value>| {
            let total: i64 = piece_len;
            let _ = total;
            encode(&dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("album")),
                    ("piece length", Value::Int(piece_len)),
                    ("pieces", Value::Bytes(vec![0u8; 20])),
                    ("files", Value::List(files)),
                ]),
            )]))
        };

        // Two entries naming one file: their writes would alias, and the
        // pieces over them could never verify.
        let dup = torrent(vec![
            file(piece_len - 10, vec!["same.bin"]),
            file(10, vec!["same.bin"]),
        ]);
        assert!(
            matches!(MetaInfo::parse(&dup), Err(Error::Invalid(_))),
            "duplicate file paths were accepted"
        );

        // A file where another entry wants a directory.
        let shadow = torrent(vec![
            file(piece_len - 10, vec!["a"]),
            file(10, vec!["a", "b"]),
        ]);
        assert!(
            matches!(MetaInfo::parse(&shadow), Err(Error::Invalid(_))),
            "a file path shadowing a directory was accepted"
        );
        // Order must not matter.
        let shadow_rev = torrent(vec![
            file(10, vec!["a", "b"]),
            file(piece_len - 10, vec!["a"]),
        ]);
        assert!(matches!(
            MetaInfo::parse(&shadow_rev),
            Err(Error::Invalid(_))
        ));

        // Names that merely share a prefix are fine, and so are same-named
        // files in different directories.
        let ok = torrent(vec![
            file(piece_len - 30, vec!["a"]),
            file(10, vec!["ab"]),
            file(10, vec!["d", "a"]),
            file(10, vec!["e", "a"]),
        ]);
        MetaInfo::parse(&ok).expect("distinct paths that share prefixes are legal");
    }

    /// The bytes of a multi-file torrent with `count` one-byte files, written
    /// directly rather than through the encoder: a hundred thousand `Value`s
    /// is a slow way to spell a test input.
    fn many_files(count: usize) -> Vec<u8> {
        let total = u64::try_from(count).unwrap();
        let pieces = total.div_ceil(u64::from(MIN_PIECE_LENGTH));
        let mut out = Vec::new();
        out.extend_from_slice(b"d4:infod5:filesl");
        for i in 0..count {
            let name = format!("f{i}");
            let _ = write!(&mut out, "d6:lengthi1e4:pathl{}:{name}ee", name.len());
        }
        let _ = write!(
            &mut out,
            "e4:name5:album12:piece lengthi{MIN_PIECE_LENGTH}e6:pieces{}:",
            pieces * 20
        );
        out.resize(out.len() + usize::try_from(pieces * 20).unwrap(), 0);
        out.extend_from_slice(b"ee");
        out
    }

    /// Every file is an open descriptor for as long as the torrent is hosted,
    /// so the count is bounded here rather than discovered at `EMFILE`.
    #[test]
    fn caps_the_number_of_files() {
        assert_eq!(
            MetaInfo::parse(&many_files(MAX_FILES + 1)).err(),
            Some(Error::Invalid("too many files"))
        );
        // The cap itself is a torrent, not a refusal.
        let at_cap = MetaInfo::parse(&many_files(MAX_FILES)).expect("a torrent at the file cap");
        assert_eq!(at_cap.files.len(), MAX_FILES);
    }

    /// More pieces than a `bitfield` message can carry is a torrent that could
    /// never exchange one, and is refused on the count the lengths imply —
    /// before the hash list is consulted, so the reason given is the cap.
    #[test]
    fn caps_the_number_of_pieces_at_what_a_bitfield_can_carry() {
        let piece_len = i64::from(MIN_PIECE_LENGTH);
        let torrent = |pieces: u32| {
            encode(&dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("huge.bin")),
                    ("piece length", Value::Int(piece_len)),
                    // A short hash list on purpose: the cap has to fire on
                    // the arithmetic, not after a 160 MiB string is checked.
                    ("pieces", Value::Bytes(vec![0u8; 20])),
                    ("length", Value::Int(i64::from(pieces) * piece_len)),
                ]),
            )]))
        };
        assert_eq!(
            MetaInfo::parse(&torrent(MAX_PIECES + 1)).err(),
            Some(Error::Invalid(
                "more pieces than a bitfield message can carry"
            ))
        );
        // At the cap the count is legal and the complaint is the ordinary one.
        assert_eq!(
            MetaInfo::parse(&torrent(MAX_PIECES)).err(),
            Some(Error::Invalid("piece count disagrees with total length"))
        );
        // The cap is what the wire can carry: an id byte and a bit per piece.
        assert_eq!(1 + MAX_PIECES.div_ceil(8), crate::wire::MAX_MESSAGE_LEN);
    }

    /// `A.txt` and `a.txt` are one file on a case-insensitive volume, which
    /// is exactly the aliasing the distinct-path check exists to refuse.
    #[test]
    fn rejects_paths_that_collide_up_to_case() {
        let piece_len = i64::from(MIN_PIECE_LENGTH);
        let file = |len: i64, parts: Vec<&str>| {
            dict(vec![
                ("length", Value::Int(len)),
                (
                    "path",
                    Value::List(parts.into_iter().map(bval).collect::<Vec<_>>()),
                ),
            ])
        };
        let torrent = |files: Vec<Value>| {
            encode(&dict(vec![(
                "info",
                dict(vec![
                    ("name", bval("album")),
                    ("piece length", Value::Int(piece_len)),
                    ("pieces", Value::Bytes(vec![0u8; 20])),
                    ("files", Value::List(files)),
                ]),
            )]))
        };
        for (a, b) in [
            (vec!["A.txt"], vec!["a.txt"]),
            (vec!["Dir", "x"], vec!["dir", "x"]),
            (vec!["Ärger.txt"], vec!["ärger.txt"]),
            // A file where another entry wants a directory, up to case.
            (vec!["A"], vec!["a", "b"]),
        ] {
            let input = torrent(vec![file(piece_len - 10, a.clone()), file(10, b.clone())]);
            assert!(
                matches!(MetaInfo::parse(&input), Err(Error::Invalid(_))),
                "{a:?} and {b:?} were both accepted"
            );
        }
        // Different names that merely share letters are still distinct.
        let ok = torrent(vec![file(piece_len - 10, vec!["ab"]), file(10, vec!["ba"])]);
        MetaInfo::parse(&ok).expect("distinct names");
    }

    /// A component longer than a filename, or a path longer than a path,
    /// is refused at the torrent rather than as ENAMETOOLONG at the first
    /// block written.
    #[test]
    fn rejects_over_long_components_and_paths() {
        let piece_len = i64::from(MIN_PIECE_LENGTH);
        let file = |parts: Vec<String>| {
            dict(vec![
                ("length", Value::Int(piece_len)),
                (
                    "path",
                    Value::List(parts.iter().map(|p| bval(p)).collect::<Vec<_>>()),
                ),
            ])
        };
        let torrent = |name: &str, files: Vec<Value>| {
            encode(&dict(vec![(
                "info",
                dict(vec![
                    ("name", bval(name)),
                    ("piece length", Value::Int(piece_len)),
                    ("pieces", Value::Bytes(vec![0u8; 20])),
                    ("files", Value::List(files)),
                ]),
            )]))
        };

        let long = "x".repeat(MAX_COMPONENT_BYTES + 1);
        let fits = "x".repeat(MAX_COMPONENT_BYTES);
        assert_eq!(
            MetaInfo::parse(&torrent("album", vec![file(vec![long.clone()])])).err(),
            Some(Error::Invalid(
                "path component longer than a filename can be"
            ))
        );
        // The name is a component too.
        assert_eq!(
            MetaInfo::parse(&torrent(&long, vec![file(vec!["a".into()])])).err(),
            Some(Error::Invalid(
                "path component longer than a filename can be"
            ))
        );
        MetaInfo::parse(&torrent("album", vec![file(vec![fits.clone()])]))
            .expect("a component at the limit");
        // Bytes, not characters: a multibyte name is measured as the
        // filesystem measures it.
        let wide = "é".repeat(MAX_COMPONENT_BYTES / 2 + 1);
        assert!(wide.len() > MAX_COMPONENT_BYTES);
        assert!(MetaInfo::parse(&torrent("album", vec![file(vec![wide])])).is_err());

        // Every component fits, the path does not.
        let deep: Vec<String> = (0..=MAX_PATH_BYTES / MAX_COMPONENT_BYTES)
            .map(|_| fits.clone())
            .collect();
        assert_eq!(
            MetaInfo::parse(&torrent("album", vec![file(deep)])).err(),
            Some(Error::Invalid("file path longer than a path can be"))
        );
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
