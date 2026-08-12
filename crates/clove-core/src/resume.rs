//! Per-torrent resume data — versioned bencode (Q2, `docs/DECISIONS.md`).
//!
//! This module is only the format. The atomic write-temp-rename that puts it
//! on disk belongs to whoever owns the data directory — `cloved`'s registry —
//! and the written spec is `docs/STATE-FORMAT.md`. The format is an API
//! (`SQLite` doctrine): any semantic
//! change bumps [`VERSION`]. Newer clove always reads older files; older
//! clove refuses newer files cleanly — a clear error, no write, no
//! corruption. Unknown keys are likewise refused: resume files are
//! machine-written, so an unexpected key means version discipline failed
//! somewhere, and surfacing that beats guessing.

use std::collections::BTreeMap;
use std::fmt;

use crate::bencode::{self, Value};

/// Current resume-format version. Bump on any semantic change.
///
/// History: v1 initial; v2 added the optional `paused` flag; v3 added the
/// optional `sequential` flag; v4 added the optional `added` timestamp;
/// v5 added the optional `pause_reason` and `seed_ratio` (an earlier file
/// reads as paused by the operator, with no per-torrent ratio).
pub const VERSION: i64 = 5;

/// Everything clove needs to pick a torrent back up after a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resume {
    /// Identity of the torrent this state belongs to.
    pub info_hash: [u8; 20],
    /// Piece count, so the bitfields below are unambiguous.
    pub num_pieces: u32,
    /// Pieces present on disk. MSB-first bitfield, BEP 3 convention;
    /// trailing bits of the final byte must be zero.
    pub have: Vec<u8>,
    /// Pieces that have passed SHA-1 verification (subset of `have` in
    /// intent; stored separately so a crash between download and verify
    /// costs re-verification, never trust).
    pub verified: Vec<u8>,
    /// Per-file priority, on-wire file order: 0 = skip, 1 = normal, 2 = high.
    pub priorities: Vec<u8>,
    /// Lifetime bytes uploaded.
    pub uploaded: u64,
    /// Lifetime bytes downloaded.
    pub downloaded: u64,
    /// Announce tiers in their current (BEP 12 shuffled) order.
    pub trackers: Vec<Vec<String>>,
    /// Whether the torrent is paused. Optional on disk (added in v2); a v1
    /// file, or any file omitting it, reads as `false`.
    pub paused: bool,
    /// Whether pieces are picked in order rather than rarest-first (SCOPE §3's
    /// per-torrent sequential flag). Optional on disk (added in v3); an
    /// earlier file, or any file omitting it, reads as `false`.
    pub sequential: bool,
    /// When the torrent was added, as **milliseconds** since the Unix epoch.
    ///
    /// The listing's order, and nothing else. Torrents were keyed by info-hash
    /// and listed in that order, which is to say shuffled — and reshuffled the
    /// moment one is added, so the row under an operator's eye moves while
    /// they are reading it.
    ///
    /// Milliseconds rather than seconds because the resolution is the whole
    /// point: at one-second granularity every torrent of a bulk add — a
    /// scripted loop, say — shares a timestamp, falls through to the
    /// info-hash tie-break, and comes out
    /// shuffled again. Adds are serialised by the registry lock and each does
    /// file I/O, so a millisecond collision needs two adds inside the same
    /// millisecond; ties still break on info-hash, so the order stays total
    /// and stable either way.
    ///
    /// Optional on disk (added in v4); an earlier file, or any file omitting
    /// it, reads as 0 and therefore sorts before everything added since. That
    /// is the right answer for an upgrade: those torrents *were* added first.
    pub added: u64,
    /// Why the torrent is paused, when it is: `0` the operator, `1` its seed
    /// ratio, `2` the seed idle limit.
    ///
    /// Meaningless unless `paused` is set, and stored as a small integer
    /// because that is what a bencode file can hold. An unknown code reads as
    /// the operator: a paused torrent is paused whatever the label says, and
    /// refusing to load one over a label would be the wrong trade.
    ///
    /// Optional on disk (added in v5); an earlier file reads as `0`.
    pub pause_reason: i64,
    /// This torrent's own seeding ratio, in **thousandths**, or `0` to follow
    /// the daemon's `seed_ratio`.
    ///
    /// Thousandths because bencode has integers and no floats, and a ratio is
    /// a two-decimal quantity in practice: `1500` is 1.5. `0` is "no override"
    /// rather than "stop immediately", which is the same choice the config
    /// makes for the global key.
    ///
    /// Optional on disk (added in v5); an earlier file reads as `0`.
    pub seed_ratio_milli: u64,
}

/// Why a resume file was refused. Refusal never writes anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not valid bencode.
    Bencode(bencode::Error),
    /// Written by a newer clove than this one.
    FutureVersion(i64),
    /// Structurally invalid; the message names the offending field.
    Invalid(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bencode(e) => write!(f, "resume: {e}"),
            Error::FutureVersion(v) => write!(
                f,
                "resume: file has format version {v}, this clove reads up to {VERSION}; refusing to touch it (upgrade clove or restore the older state file)"
            ),
            Error::Invalid(what) => write!(f, "resume: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<bencode::Error> for Error {
    fn from(e: bencode::Error) -> Self {
        Error::Bencode(e)
    }
}

/// Number of bytes a BEP 3 bitfield needs for `num_pieces` pieces.
#[must_use]
pub fn bitfield_len(num_pieces: u32) -> usize {
    num_pieces.div_ceil(8) as usize
}

const KEYS: [&[u8]; 14] = [
    b"version",
    b"info_hash",
    b"num_pieces",
    b"have",
    b"verified",
    b"priorities",
    b"uploaded",
    b"downloaded",
    b"trackers",
    b"paused",
    b"sequential",
    b"added",
    b"pause_reason",
    b"seed_ratio_milli",
];

/// The fields added after v1, each defaulting the way an older file should
/// read.
///
/// Split out of [`Resume::decode`] because it is a different job: everything
/// above it either validates or refuses, while every one of these is a
/// question about what a file that predates the field should mean, answered
/// the forgiving way.
struct Optional {
    paused: bool,
    sequential: bool,
    added: u64,
    pause_reason: i64,
    seed_ratio_milli: u64,
}

fn optional_fields(root: &Value) -> Optional {
    Optional {
        // Since v2: a v1 file (or any omitting it) reads as not paused.
        paused: root
            .get(b"paused")
            .and_then(Value::as_int)
            .is_some_and(|n| n != 0),
        // Since v3; an older file reads as rarest-first, which is what it was
        // downloading with.
        sequential: root
            .get(b"sequential")
            .and_then(Value::as_int)
            .is_some_and(|n| n != 0),
        // Since v4. A negative value is a clock that ran backwards rather than
        // a corrupt file, and the listing's order is not worth refusing to
        // start over, so it clamps to 0 — the same place an upgraded torrent
        // lands.
        added: root
            .get(b"added")
            .and_then(Value::as_int)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0),
        // Since v5. Both clamp rather than refuse: neither decides whether a
        // torrent's data is trustworthy, and a resume file is not worth
        // failing to start over a label or a limit.
        pause_reason: root
            .get(b"pause_reason")
            .and_then(Value::as_int)
            .unwrap_or(0),
        seed_ratio_milli: root
            .get(b"seed_ratio_milli")
            .and_then(Value::as_int)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0),
    }
}

impl Resume {
    /// Canonically encode for writing (the writer is storage's job).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut map = BTreeMap::new();
        let mut put = |key: &[u8], value: Value| {
            map.insert(key.to_vec(), value);
        };
        put(b"version", Value::Int(VERSION));
        put(b"info_hash", Value::Bytes(self.info_hash.to_vec()));
        put(b"num_pieces", Value::Int(i64::from(self.num_pieces)));
        put(b"have", Value::Bytes(self.have.clone()));
        put(b"verified", Value::Bytes(self.verified.clone()));
        put(b"priorities", Value::Bytes(self.priorities.clone()));
        // Stats saturate rather than fail: a counter past i64::MAX bytes
        // is not a state worth crashing over.
        put(
            b"uploaded",
            Value::Int(i64::try_from(self.uploaded).unwrap_or(i64::MAX)),
        );
        put(
            b"downloaded",
            Value::Int(i64::try_from(self.downloaded).unwrap_or(i64::MAX)),
        );
        let tiers = self
            .trackers
            .iter()
            .map(|tier| {
                Value::List(
                    tier.iter()
                        .map(|url| Value::Bytes(url.clone().into_bytes()))
                        .collect(),
                )
            })
            .collect();
        put(b"trackers", Value::List(tiers));
        put(b"paused", Value::Int(i64::from(self.paused)));
        put(b"sequential", Value::Int(i64::from(self.sequential)));
        put(
            b"added",
            Value::Int(i64::try_from(self.added).unwrap_or(i64::MAX)),
        );
        put(b"pause_reason", Value::Int(self.pause_reason));
        put(
            b"seed_ratio_milli",
            Value::Int(i64::try_from(self.seed_ratio_milli).unwrap_or(i64::MAX)),
        );
        bencode::encode(&Value::Dict(map))
    }

    /// Decode and validate a resume file.
    ///
    /// # Errors
    ///
    /// Malformed bencode, a version from the future, unknown keys, or any
    /// internal inconsistency (bitfield lengths, stray trailing bits,
    /// out-of-range priorities).
    pub fn decode(input: &[u8]) -> Result<Self, Error> {
        let root = bencode::decode(input)?;
        let dict = root
            .as_dict()
            .ok_or(Error::Invalid("top level is not a dictionary"))?;

        let version = root
            .get(b"version")
            .and_then(Value::as_int)
            .ok_or(Error::Invalid("missing version"))?;
        if version > VERSION {
            return Err(Error::FutureVersion(version));
        }
        if version < 1 {
            return Err(Error::Invalid("nonsense version"));
        }
        for key in dict.keys() {
            if !KEYS.contains(&key.as_slice()) {
                return Err(Error::Invalid("unknown key: version discipline violated"));
            }
        }

        let info_hash_raw = root
            .get(b"info_hash")
            .and_then(Value::as_bytes)
            .ok_or(Error::Invalid("missing info_hash"))?;
        let info_hash: [u8; 20] = info_hash_raw
            .try_into()
            .map_err(|_| Error::Invalid("info_hash is not 20 bytes"))?;

        let num_pieces = root
            .get(b"num_pieces")
            .and_then(Value::as_int)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n >= 1)
            .ok_or(Error::Invalid("bad num_pieces"))?;

        let have = bitfield(&root, b"have", num_pieces)?;
        let verified = bitfield(&root, b"verified", num_pieces)?;
        // `verified` is the subset of `have` we know passed SHA-1 against what
        // is actually on disk (`docs/STATE-FORMAT.md`), so a bit set here and
        // not there describes a piece that is verified but not held — which is
        // not a state clove can be in. A file claiming it is either corrupt or
        // written by something that did not understand the format, and either
        // way believing it would mean trusting a piece we never fetched.
        // Both are the same length and their spare bits are already required to
        // be zero, so a bytewise test is the whole check.
        if verified.iter().zip(&have).any(|(&v, &h)| v & !h != 0) {
            return Err(Error::Invalid("verified claims a piece have does not"));
        }

        let priorities = root
            .get(b"priorities")
            .and_then(Value::as_bytes)
            .ok_or(Error::Invalid("missing priorities"))?
            .to_vec();
        if priorities.iter().any(|&p| p > 2) {
            return Err(Error::Invalid("priority out of range"));
        }

        let uploaded = counter(&root, b"uploaded")?;
        let downloaded = counter(&root, b"downloaded")?;

        let mut trackers = Vec::new();
        let tiers = root
            .get(b"trackers")
            .and_then(Value::as_list)
            .ok_or(Error::Invalid("missing trackers"))?;
        for tier in tiers {
            let tier = tier
                .as_list()
                .ok_or(Error::Invalid("tracker tier is not a list"))?;
            let mut urls = Vec::with_capacity(tier.len());
            for url in tier {
                urls.push(
                    url.as_str()
                        .ok_or(Error::Invalid("tracker URL is not UTF-8"))?
                        .to_owned(),
                );
            }
            trackers.push(urls);
        }

        let Optional {
            paused,
            sequential,
            added,
            pause_reason,
            seed_ratio_milli,
        } = optional_fields(&root);

        Ok(Resume {
            info_hash,
            num_pieces,
            have,
            verified,
            priorities,
            uploaded,
            downloaded,
            trackers,
            paused,
            sequential,
            added,
            pause_reason,
            seed_ratio_milli,
        })
    }
}

fn bitfield(root: &Value, key: &[u8], num_pieces: u32) -> Result<Vec<u8>, Error> {
    let bytes = root
        .get(key)
        .and_then(Value::as_bytes)
        .ok_or(Error::Invalid("missing bitfield"))?;
    if bytes.len() != bitfield_len(num_pieces) {
        return Err(Error::Invalid("bitfield length disagrees with num_pieces"));
    }
    let used = num_pieces % 8;
    if used != 0 {
        let mask = 0xFF_u8 >> used;
        if bytes.last().is_some_and(|&last| last & mask != 0) {
            return Err(Error::Invalid("bitfield has bits set past num_pieces"));
        }
    }
    Ok(bytes.to_vec())
}

fn counter(root: &Value, key: &[u8]) -> Result<u64, Error> {
    root.get(key)
        .and_then(Value::as_int)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or(Error::Invalid("bad byte counter"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Resume {
        Resume {
            info_hash: [7; 20],
            num_pieces: 10,
            have: vec![0b1010_1010, 0b1100_0000],
            verified: vec![0b1010_1010, 0b1000_0000],
            priorities: vec![1, 0, 2],
            uploaded: 12345,
            downloaded: 67890,
            trackers: vec![vec!["http://t.i2p/a".into()], vec!["http://u.i2p/a".into()]],
            paused: true,
            sequential: true,
            added: 1_800_000_000,
            pause_reason: 1,
            seed_ratio_milli: 1_500,
        }
    }

    #[test]
    fn round_trips() {
        let r = sample();
        assert_eq!(Resume::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn paused_round_trips_both_ways() {
        for paused in [false, true] {
            let mut r = sample();
            r.paused = paused;
            assert_eq!(Resume::decode(&r.encode()).unwrap().paused, paused);
        }
    }

    #[test]
    fn a_file_without_paused_reads_as_not_paused() {
        // Strip the `paused` entry to simulate a v1 file.
        let mut r = sample();
        r.paused = false;
        let encoded = r.encode();
        let needle = b"6:pausedi0e";
        let pos = encoded
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        let mut stripped = encoded[..pos].to_vec();
        stripped.extend_from_slice(&encoded[pos + needle.len()..]);
        assert!(!Resume::decode(&stripped).unwrap().paused);
    }

    #[test]
    fn a_file_without_sequential_reads_as_rarest_first() {
        // Strip the `sequential` entry to simulate a v2 file.
        let mut r = sample();
        r.sequential = false;
        let encoded = r.encode();
        let needle = b"10:sequentiali0e";
        let pos = encoded
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        let mut stripped = encoded[..pos].to_vec();
        stripped.extend_from_slice(&encoded[pos + needle.len()..]);
        assert!(!Resume::decode(&stripped).unwrap().sequential);
    }

    #[test]
    fn a_file_without_added_sorts_first() {
        // Strip `added` to simulate a v3 file. It reads as 0, which puts it
        // ahead of anything added since — which is true of it.
        let mut r = sample();
        r.added = 0;
        let encoded = r.encode();
        let needle = b"5:addedi0e";
        let pos = encoded
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("the encoder writes `added`");
        let mut stripped = encoded[..pos].to_vec();
        stripped.extend_from_slice(&encoded[pos + needle.len()..]);
        assert_eq!(Resume::decode(&stripped).unwrap().added, 0);

        // And a clock that ran backwards costs an ordering, not a start: the
        // listing's sort is not worth refusing to load a torrent over.
        let mut negative = sample();
        negative.added = 0;
        let encoded = negative.encode();
        let mut patched = Vec::new();
        let pos = encoded
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("the encoder writes `added`");
        patched.extend_from_slice(&encoded[..pos]);
        patched.extend_from_slice(b"5:addedi-5e");
        patched.extend_from_slice(&encoded[pos + needle.len()..]);
        assert_eq!(Resume::decode(&patched).unwrap().added, 0);
    }

    #[test]
    fn refuses_the_future_cleanly() {
        // Written against VERSION rather than against its current digits, so
        // the next format bump does not have to come here and edit a literal —
        // which is how this test came to assert `3` while the code said 4.
        let encoded = sample().encode();
        let current = format!("7:versioni{VERSION}e").into_bytes();
        let future = format!("7:versioni{}e", VERSION + 1).into_bytes();
        let pos = encoded
            .windows(current.len())
            .position(|w| w == current.as_slice())
            .expect("the encoder writes the current version");
        let mut r = encoded[..pos].to_vec();
        r.extend_from_slice(&future);
        r.extend_from_slice(&encoded[pos + current.len()..]);

        match Resume::decode(&r) {
            Err(Error::FutureVersion(v)) if v == VERSION + 1 => {}
            other => panic!("expected FutureVersion({}), got {other:?}", VERSION + 1),
        }
        // The current version is of course still readable.
        assert!(Resume::decode(&encoded).is_ok());
    }

    #[test]
    fn refuses_unknown_keys() {
        // Splice an extra key into an otherwise valid file.
        let valid = sample().encode();
        let mut evil = b"d5:extrai1e".to_vec();
        evil.extend_from_slice(&valid[1..]);
        assert_eq!(
            Resume::decode(&evil),
            Err(Error::Invalid("unknown key: version discipline violated"))
        );
    }

    #[test]
    fn validates_bitfields() {
        // Wrong length.
        let mut r = sample();
        r.have = vec![0xFF];
        assert!(matches!(
            Resume::decode(&r.encode()),
            Err(Error::Invalid(_))
        ));

        // Stray bit past num_pieces (10 pieces -> 6 trailing bits must be 0).
        let mut r = sample();
        r.verified = vec![0xFF, 0b1100_0001];
        assert!(matches!(
            Resume::decode(&r.encode()),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn validates_fields() {
        let mut r = sample();
        r.priorities = vec![3];
        assert!(matches!(
            Resume::decode(&r.encode()),
            Err(Error::Invalid(_))
        ));

        for garbage in [&b""[..], b"le", b"d1:ai1ee", b"i42e"] {
            assert!(Resume::decode(garbage).is_err(), "accepted {garbage:?}");
        }
    }

    #[test]
    fn truncation_or_flips_never_pass() {
        // Paranoia sweep: no prefix of a valid file decodes.
        let valid = sample().encode();
        for cut in 0..valid.len() {
            assert!(
                Resume::decode(&valid[..cut]).is_err(),
                "prefix {cut} decoded"
            );
        }
    }
}
