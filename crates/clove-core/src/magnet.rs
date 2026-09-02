//! Magnet link parsing (BEP 9 `xt=urn:btih:` form).
//!
//! A magnet gives us an info-hash but no metadata; the [`crate::metadata`]
//! exchange fetches the info dictionary from peers, and the `tr=` trackers
//! (I2P-only, filtered like `.torrent` announce URLs) plus PEX supply peers.
//!
//! The btih value is accepted as 40 hex characters (v1) or 32 base32
//! characters (both encode the same 20-byte hash). I2P's own `maggot://`
//! links are a separate, underspecified format left open in
//! `docs/PROTOCOL.i2p-bt` until confirmed against real examples — we do not
//! guess at a grammar we cannot verify.

use crate::bencode::{self, Value};
use crate::http;
use crate::metainfo::{MAX_TRACKERS, is_i2p_tracker};

/// A parsed magnet link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Magnet {
    /// The 20-byte info-hash from `xt=urn:btih:`.
    pub info_hash: [u8; 20],
    /// The display name (`dn=`), if given.
    pub display_name: Option<String>,
    /// I2P announce URLs (`tr=`), at most [`MAX_TRACKERS`] of them; non-I2P
    /// trackers are dropped.
    pub trackers: Vec<String>,
    /// How many `tr=` values were dropped: non-I2P ones, and I2P ones past
    /// [`MAX_TRACKERS`] — the same accounting a `.torrent` keeps.
    pub skipped_trackers: usize,
}

/// Why a magnet link could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a `magnet:?` URI.
    NotMagnet,
    /// No `xt=urn:btih:` parameter.
    NoInfoHash,
    /// The btih value was not 40 hex or 32 base32 characters.
    BadInfoHash,
    /// Two `xt=urn:btih:` parameters that name different hashes. A link is
    /// one torrent; which of two it means is not ours to guess.
    ConflictingInfoHash,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotMagnet => f.write_str("magnet: not a magnet: URI"),
            Error::NoInfoHash => f.write_str("magnet: no xt=urn:btih: info-hash"),
            Error::BadInfoHash => f.write_str("magnet: info-hash is not 40 hex or 32 base32 chars"),
            Error::ConflictingInfoHash => {
                f.write_str("magnet: two xt=urn:btih: parameters name different info-hashes")
            }
        }
    }
}

impl std::error::Error for Error {}

impl Magnet {
    /// Parse a `magnet:?…` link.
    ///
    /// # Errors
    ///
    /// [`Error::NotMagnet`] if the scheme is wrong, [`Error::NoInfoHash`] if
    /// there is no btih parameter, [`Error::BadInfoHash`] if it is malformed.
    pub fn parse(uri: &str) -> Result<Magnet, Error> {
        let query = uri.strip_prefix("magnet:?").ok_or(Error::NotMagnet)?;
        let mut info_hash = None;
        let mut display_name = None;
        let mut trackers = Vec::new();
        let mut skipped_trackers = 0usize;

        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "xt" => {
                    if let Some(hash) = value.strip_prefix("urn:btih:") {
                        let hash = parse_btih(hash)?;
                        if info_hash.is_some_and(|seen| seen != hash) {
                            return Err(Error::ConflictingInfoHash);
                        }
                        info_hash = Some(hash);
                    }
                }
                "dn" => {
                    display_name =
                        Some(String::from_utf8_lossy(&http::percent_decode(value)).into_owned());
                }
                "tr" => {
                    let url = String::from_utf8_lossy(&http::percent_decode(value)).into_owned();
                    // Capped for the reason `metainfo` caps a torrent's: a
                    // link is as much a stranger's text as a file is.
                    if is_i2p_tracker(&url) && trackers.len() < MAX_TRACKERS {
                        trackers.push(url);
                    } else {
                        skipped_trackers += 1;
                    }
                }
                _ => {}
            }
        }

        Ok(Magnet {
            info_hash: info_hash.ok_or(Error::NoInfoHash)?,
            display_name,
            trackers,
            skipped_trackers,
        })
    }
}

/// Decode a btih value: 40 hex chars or 32 base32 chars, to 20 bytes.
fn parse_btih(value: &str) -> Result<[u8; 20], Error> {
    if value.len() == 40 {
        let mut out = [0u8; 20];
        for (i, byte) in out.iter_mut().enumerate() {
            let hi = hex_nibble(value.as_bytes()[i * 2])?;
            let lo = hex_nibble(value.as_bytes()[i * 2 + 1])?;
            *byte = (hi << 4) | lo;
        }
        Ok(out)
    } else if value.len() == 32 {
        // BitTorrent magnet base32 is uppercase RFC 4648; our decoder is
        // lowercase, so normalize first.
        let lower = value.to_ascii_lowercase();
        let bytes = i2pnet::addr::base32_decode(&lower).ok_or(Error::BadInfoHash)?;
        bytes.try_into().map_err(|_| Error::BadInfoHash)
    } else {
        Err(Error::BadInfoHash)
    }
}

fn hex_nibble(b: u8) -> Result<u8, Error> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::BadInfoHash),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_infohash_with_name_and_trackers() {
        let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567\
                   &dn=Some%20Torrent\
                   &tr=http%3A%2F%2Ftracker.postman.i2p%2Fannounce\
                   &tr=http%3A%2F%2Fclearnet.example.org%2Fannounce";
        let m = Magnet::parse(uri).unwrap();
        assert_eq!(
            m.info_hash,
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef, 0x01, 0x23, 0x45, 0x67
            ]
        );
        assert_eq!(m.display_name.as_deref(), Some("Some Torrent"));
        // Only the I2P tracker survives, and the other is counted.
        assert_eq!(m.trackers, vec!["http://tracker.postman.i2p/announce"]);
        assert_eq!(m.skipped_trackers, 1);
    }

    /// A link can carry as many `tr=` values as a file can carry URLs, and
    /// each kept one costs the same lookup and announce.
    #[test]
    fn caps_kept_trackers() {
        let mut uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".to_owned();
        for i in 0..MAX_TRACKERS + 4 {
            uri.push_str("&tr=http%3A%2F%2Ft");
            uri.push_str(&i.to_string());
            uri.push_str(".i2p%2Fannounce");
        }
        let m = Magnet::parse(&uri).unwrap();
        assert_eq!(m.trackers.len(), MAX_TRACKERS);
        assert_eq!(m.trackers[0], "http://t0.i2p/announce");
        assert_eq!(
            m.trackers[MAX_TRACKERS - 1],
            format!("http://t{}.i2p/announce", MAX_TRACKERS - 1)
        );
        assert_eq!(m.skipped_trackers, 4);
    }

    #[test]
    fn parses_base32_infohash() {
        // base32 of 20 zero bytes = 32 'a's (uppercase in the wild).
        let uri = format!("magnet:?xt=urn:btih:{}", "A".repeat(32));
        let m = Magnet::parse(&uri).unwrap();
        assert_eq!(m.info_hash, [0u8; 20]);
    }

    #[test]
    fn hex_and_base32_agree() {
        let hex = "magnet:?xt=urn:btih:ffffffffffffffffffffffffffffffffffffffff";
        let b32 = format!("magnet:?xt=urn:btih:{}", "7".repeat(32)); // base32 all-ones
        let a = Magnet::parse(hex).unwrap().info_hash;
        let b = Magnet::parse(&b32).unwrap().info_hash;
        assert_eq!(a, [0xFF; 20]);
        assert_eq!(b, [0xFF; 20]);
    }

    /// The last of several `xt=` used to win silently; a link naming two
    /// torrents is refused, while the same hash spelled twice — hex and
    /// base32 of it, say — is one torrent and fine.
    #[test]
    fn conflicting_info_hashes_are_refused() {
        let a = "0123456789abcdef0123456789abcdef01234567";
        let b = "ffffffffffffffffffffffffffffffffffffffff";
        assert_eq!(
            Magnet::parse(&format!("magnet:?xt=urn:btih:{a}&xt=urn:btih:{b}")),
            Err(Error::ConflictingInfoHash)
        );
        let same = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{b}&xt=urn:btih:{}",
            "7".repeat(32)
        ))
        .expect("one hash, two spellings");
        assert_eq!(same.info_hash, [0xFF; 20]);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Magnet::parse("http://x"), Err(Error::NotMagnet));
        assert_eq!(Magnet::parse("magnet:?dn=x"), Err(Error::NoInfoHash));
        assert_eq!(
            Magnet::parse("magnet:?xt=urn:btih:tooshort"),
            Err(Error::BadInfoHash)
        );
        assert_eq!(
            Magnet::parse("magnet:?xt=urn:btih:zzzz456789abcdef0123456789abcdef01234567"),
            Err(Error::BadInfoHash)
        );
    }
}

/// Build `.torrent` bytes from a fetched raw info dictionary plus the
/// magnet's tracker URLs, so a magnet add persists and reloads exactly like
/// a file add. The info bytes are embedded verbatim (the info-hash covers
/// them), with an `announce-list` of one tier per URL.
#[must_use]
pub fn torrent_bytes(raw_info: &[u8], trackers: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw_info.len() + 128);
    out.push(b'd');
    if !trackers.is_empty() {
        out.extend_from_slice(b"13:announce-list");
        let tiers = Value::List(
            trackers
                .iter()
                .map(|url| Value::List(vec![Value::Bytes(url.clone().into_bytes())]))
                .collect(),
        );
        out.extend_from_slice(&bencode::encode(&tiers));
    }
    out.extend_from_slice(b"4:info");
    out.extend_from_slice(raw_info);
    out.push(b'e');
    out
}

#[cfg(test)]
mod torrent_bytes_tests {
    use super::*;
    use crate::metainfo::MetaInfo;

    #[test]
    fn synthesized_torrent_parses_with_same_hash_and_trackers() {
        // A minimal real info dict via the bencode encoder.
        use std::collections::BTreeMap;
        let mut info = BTreeMap::new();
        info.insert(b"length".to_vec(), Value::Int(16384));
        info.insert(b"name".to_vec(), Value::Bytes(b"demo".to_vec()));
        info.insert(b"piece length".to_vec(), Value::Int(16384));
        info.insert(b"pieces".to_vec(), Value::Bytes(vec![0u8; 20]));
        let raw_info = bencode::encode(&Value::Dict(info));

        let trackers = vec!["http://tracker.i2p/announce".to_owned()];
        let bytes = torrent_bytes(&raw_info, &trackers);
        let meta = MetaInfo::parse(&bytes).unwrap();
        assert_eq!(&meta.raw_info[..], &raw_info[..]);
        assert_eq!(meta.trackers, vec![trackers.clone()]);

        // Trackerless magnets synthesize too.
        let bare = torrent_bytes(&raw_info, &[]);
        let meta2 = MetaInfo::parse(&bare).unwrap();
        assert_eq!(meta2.info_hash, meta.info_hash);
        assert!(meta2.trackers.is_empty());
    }
}
