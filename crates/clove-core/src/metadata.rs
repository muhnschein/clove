//! BEP 9 metadata exchange (`ut_metadata`).
//!
//! When clove starts from a magnet link it has the info-hash but not the
//! info dictionary; peers serve it in 16 KiB pieces over the `ut_metadata`
//! extension. This module is the message codec plus a [`MetadataAssembler`]
//! that collects pieces, then verifies the reassembled bytes against the
//! expected info-hash before they are trusted — a lying peer cannot slip us
//! a different torrent (`crate::metainfo::MetaInfo::from_info_dict` parses
//! the verified bytes).
//!
//! Data messages are a bencoded header followed by raw piece bytes, split
//! via [`crate::bencode::decode_prefix`]. Hostile input is bounded: an
//! absurd advertised `total_size` is refused up front, and every piece's
//! length is checked, so a peer cannot make us allocate arbitrarily or
//! accept a short/oversized piece.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};

/// Metadata piece size (BEP 9 fixes this at 16 KiB).
pub const METADATA_PIECE_LEN: usize = 16 * 1024;

/// Largest info dictionary clove will fetch. Generous for any realistic I2P
/// torrent; guards against a peer advertising a huge `total_size`.
pub const MAX_METADATA_SIZE: usize = 8 * 1024 * 1024;

/// A `ut_metadata` message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataMessage {
    /// Please send metadata piece `piece` (`msg_type` 0).
    Request {
        /// Zero-based metadata piece index.
        piece: u32,
    },
    /// A metadata piece (`msg_type` 1).
    Data {
        /// Piece index.
        piece: u32,
        /// Total size of the whole info dictionary.
        total_size: u32,
        /// The piece bytes.
        data: Vec<u8>,
    },
    /// The peer will not serve `piece` (`msg_type` 2).
    Reject {
        /// Piece index.
        piece: u32,
    },
}

impl MetadataMessage {
    /// Encode as the payload of the negotiated `ut_metadata` extended message.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut dict = BTreeMap::new();
        let (msg_type, piece, extra) = match self {
            MetadataMessage::Request { piece } => (0, *piece, None),
            MetadataMessage::Reject { piece } => (2, *piece, None),
            MetadataMessage::Data {
                piece,
                total_size,
                data,
            } => (1, *piece, Some((*total_size, data))),
        };
        dict.insert(b"msg_type".to_vec(), Value::Int(msg_type));
        dict.insert(b"piece".to_vec(), Value::Int(i64::from(piece)));
        if let Some((total_size, _)) = extra {
            dict.insert(b"total_size".to_vec(), Value::Int(i64::from(total_size)));
        }
        let mut out = bencode::encode(&Value::Dict(dict));
        if let Some((_, data)) = extra {
            out.extend_from_slice(data);
        }
        out
    }

    /// Parse a `ut_metadata` payload (bencoded header, then raw data for a
    /// data message).
    ///
    /// # Errors
    ///
    /// Malformed bencode header, an unknown `msg_type`, a missing field, or
    /// a data piece larger than [`METADATA_PIECE_LEN`].
    pub fn parse(payload: &[u8]) -> Result<MetadataMessage, Error> {
        let (header, consumed) = bencode::decode_prefix(payload).map_err(|_| Error::Malformed)?;
        let piece = header
            .get(b"piece")
            .and_then(Value::as_int)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(Error::Malformed)?;
        match header.get(b"msg_type").and_then(Value::as_int) {
            Some(0) => Ok(MetadataMessage::Request { piece }),
            Some(2) => Ok(MetadataMessage::Reject { piece }),
            Some(1) => {
                let total_size = header
                    .get(b"total_size")
                    .and_then(Value::as_int)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or(Error::Malformed)?;
                let data = payload[consumed..].to_vec();
                if data.len() > METADATA_PIECE_LEN {
                    return Err(Error::PieceTooLarge);
                }
                Ok(MetadataMessage::Data {
                    piece,
                    total_size,
                    data,
                })
            }
            _ => Err(Error::Malformed),
        }
    }
}

/// Collects metadata pieces and verifies the whole against the info-hash.
pub struct MetadataAssembler {
    total_size: usize,
    pieces: Vec<Option<Vec<u8>>>,
    have: usize,
}

impl MetadataAssembler {
    /// Start assembling a metadata blob of `total_size` bytes (from a data
    /// message's `total_size` or the extension handshake's `metadata_size`).
    ///
    /// # Errors
    ///
    /// [`Error::BadSize`] if `total_size` is zero or over
    /// [`MAX_METADATA_SIZE`].
    pub fn new(total_size: usize) -> Result<Self, Error> {
        if total_size == 0 || total_size > MAX_METADATA_SIZE {
            return Err(Error::BadSize);
        }
        let num_pieces = total_size.div_ceil(METADATA_PIECE_LEN);
        Ok(MetadataAssembler {
            total_size,
            pieces: vec![None; num_pieces],
            have: 0,
        })
    }

    /// Number of metadata pieces.
    #[must_use]
    pub fn num_pieces(&self) -> u32 {
        u32::try_from(self.pieces.len()).unwrap_or(u32::MAX)
    }

    /// Expected byte length of piece `index` (the last piece is short).
    #[must_use]
    pub fn piece_len(&self, index: u32) -> usize {
        // The index is the peer's. On a 32-bit `usize` the multiplication
        // wraps at index 2^18, which would name a piece inside the blob
        // that does not exist; an offset that cannot be computed is out of
        // range, the same as one that can and is.
        let start = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(METADATA_PIECE_LEN));
        let Some(start) = start else {
            return 0;
        };
        if start >= self.total_size {
            return 0;
        }
        (self.total_size - start).min(METADATA_PIECE_LEN)
    }

    /// Indices of pieces still needed, for issuing requests.
    pub fn missing(&self) -> impl Iterator<Item = u32> + '_ {
        self.pieces
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.is_none().then_some(u32::try_from(i).unwrap_or(u32::MAX)))
    }

    /// Store a received piece.
    ///
    /// # Errors
    ///
    /// [`Error::BadPiece`] if the index is out of range or the data length
    /// does not match the expected piece length.
    pub fn add_piece(&mut self, index: u32, data: &[u8]) -> Result<(), Error> {
        let slot = usize::try_from(index)
            .ok()
            .filter(|&i| i < self.pieces.len());
        let Some(slot) = slot else {
            return Err(Error::BadPiece);
        };
        if data.len() != self.piece_len(index) {
            return Err(Error::BadPiece);
        }
        if self.pieces[slot].is_none() {
            self.pieces[slot] = Some(data.to_vec());
            self.have += 1;
        }
        Ok(())
    }

    /// Whether every piece has arrived.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.have == self.pieces.len()
    }

    /// If complete and the reassembled bytes hash to `expected_info_hash`,
    /// return them; otherwise `None` (incomplete, or a hash mismatch — a
    /// peer served the wrong data).
    #[must_use]
    pub fn finish(&self, expected_info_hash: [u8; 20]) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut bytes = Vec::with_capacity(self.total_size);
        for piece in &self.pieces {
            bytes.extend_from_slice(piece.as_ref()?);
        }
        let got: [u8; 20] = Sha1::digest(&bytes).into();
        (got == expected_info_hash).then_some(bytes)
    }
}

/// How many `ut_metadata` requests one peer may have served per
/// [`METADATA_REQUEST_WINDOW`].
///
/// A honest fetch asks for each piece once — [`MAX_METADATA_SIZE`] is 512
/// pieces — and again only after a stall, so this is a rate an honest peer
/// never notices. A peer asking in a loop is a bandwidth amplifier at no
/// cost to itself: ~16 KiB out per ~30-byte frame in.
pub const METADATA_REQUEST_LIMIT: u32 = 128;

/// The window [`METADATA_REQUEST_LIMIT`] is counted over.
pub const METADATA_REQUEST_WINDOW: Duration = Duration::from_secs(10);

/// Per-peer budget for serving `ut_metadata` requests: `limit` requests per
/// `window`, then `Reject` until the window turns over.
///
/// Time is a parameter so the budget is deterministic under test; the
/// caller passes `Instant::now()`. Refused requests do not count — a peer
/// past its budget is not pushed further into it by asking again — and the
/// window is fixed rather than sliding, which is simpler and no less a
/// bound: at most `2 × limit` in any span of `window`.
#[derive(Clone, Debug)]
pub struct MetadataRequestBudget {
    limit: u32,
    window: Duration,
    window_start: Option<Instant>,
    used: u32,
}

impl Default for MetadataRequestBudget {
    fn default() -> Self {
        Self::new(METADATA_REQUEST_LIMIT, METADATA_REQUEST_WINDOW)
    }
}

impl MetadataRequestBudget {
    /// A budget of `limit` requests per `window`.
    #[must_use]
    pub fn new(limit: u32, window: Duration) -> Self {
        MetadataRequestBudget {
            limit,
            window,
            window_start: None,
            used: 0,
        }
    }

    /// Whether a request arriving at `now` may be served. `true` spends one
    /// of the window's requests; `false` means answer with `Reject`.
    pub fn allow(&mut self, now: Instant) -> bool {
        let expired = self
            .window_start
            .is_none_or(|start| now.saturating_duration_since(start) >= self.window);
        if expired {
            self.window_start = Some(now);
            self.used = 0;
        }
        if self.used >= self.limit {
            return false;
        }
        self.used += 1;
        true
    }

    /// Requests still available in the current window, as of the last
    /// [`allow`](Self::allow).
    #[must_use]
    pub fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used)
    }
}

/// Why a metadata message or assembly step failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Malformed header or unknown message type.
    Malformed,
    /// A data piece exceeded [`METADATA_PIECE_LEN`].
    PieceTooLarge,
    /// Advertised total size was zero or over [`MAX_METADATA_SIZE`].
    BadSize,
    /// Piece index out of range or wrong length.
    BadPiece,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Malformed => f.write_str("ut_metadata: malformed message"),
            Error::PieceTooLarge => f.write_str("ut_metadata: piece exceeds 16 KiB"),
            Error::BadSize => f.write_str("ut_metadata: bad advertised total size"),
            Error::BadPiece => f.write_str("ut_metadata: piece index or length invalid"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trips() {
        for msg in [
            MetadataMessage::Request { piece: 3 },
            MetadataMessage::Reject { piece: 7 },
            MetadataMessage::Data {
                piece: 0,
                total_size: 100,
                data: vec![9; 50],
            },
        ] {
            assert_eq!(MetadataMessage::parse(&msg.encode()).unwrap(), msg);
        }
    }

    #[test]
    fn data_message_separates_header_from_bytes() {
        let msg = MetadataMessage::Data {
            piece: 1,
            total_size: 20000,
            data: vec![0xAB; 3616],
        };
        let encoded = msg.encode();
        // The trailing raw bytes are exactly the piece data.
        assert!(encoded.ends_with(&[0xAB; 3616]));
        assert_eq!(MetadataMessage::parse(&encoded).unwrap(), msg);
    }

    #[test]
    fn rejects_oversized_piece_and_garbage() {
        let msg = MetadataMessage::Data {
            piece: 0,
            total_size: 999_999,
            data: vec![0; METADATA_PIECE_LEN + 1],
        };
        assert_eq!(
            MetadataMessage::parse(&msg.encode()),
            Err(Error::PieceTooLarge)
        );
        assert_eq!(MetadataMessage::parse(b"garbage"), Err(Error::Malformed));
    }

    /// A peer gets its budget, then `Reject` until the window turns over;
    /// asking while refused does not push the window out.
    #[test]
    fn request_budget_limits_a_peer_per_window() {
        let window = Duration::from_secs(10);
        let mut budget = MetadataRequestBudget::new(3, window);
        let t0 = Instant::now();
        assert_eq!(budget.remaining(), 3);
        assert!(budget.allow(t0));
        assert!(budget.allow(t0 + Duration::from_secs(1)));
        assert!(budget.allow(t0 + Duration::from_secs(2)));
        assert_eq!(budget.remaining(), 0);
        // Spent: refused for the rest of the window, however often asked.
        for s in 3..10 {
            assert!(!budget.allow(t0 + Duration::from_secs(s)), "at +{s}s");
        }
        // The window turns over from its start, not from the last refusal.
        assert!(budget.allow(t0 + window));
        assert_eq!(budget.remaining(), 2);
        // A long silence starts a fresh window rather than carrying debt.
        assert!(budget.allow(t0 + window * 5));
        assert_eq!(budget.remaining(), 2);

        // The default is what a honest fetch never notices: every piece of
        // the largest allowed blob, once, fits in a few windows.
        let mut default = MetadataRequestBudget::default();
        let served = (0..=METADATA_REQUEST_LIMIT)
            .filter(|_| default.allow(t0))
            .count();
        assert_eq!(served, usize::try_from(METADATA_REQUEST_LIMIT).unwrap());
        assert!(
            MAX_METADATA_SIZE / METADATA_PIECE_LEN
                <= usize::try_from(METADATA_REQUEST_LIMIT).unwrap() * 4
        );
    }

    #[test]
    fn assembler_rejects_absurd_size() {
        assert_eq!(MetadataAssembler::new(0).err(), Some(Error::BadSize));
        assert_eq!(
            MetadataAssembler::new(MAX_METADATA_SIZE + 1).err(),
            Some(Error::BadSize)
        );
    }

    #[test]
    fn assembles_and_verifies() {
        // Two-piece metadata: 16384 + 100 bytes.
        let mut info: Vec<u8> = (0..METADATA_PIECE_LEN)
            .map(|i| u8::try_from(i % 256).unwrap_or(0))
            .collect();
        info.extend(std::iter::repeat_n(0xEE, 100));
        let hash: [u8; 20] = Sha1::digest(&info).into();

        let mut asm = MetadataAssembler::new(info.len()).unwrap();
        assert_eq!(asm.num_pieces(), 2);
        assert_eq!(asm.missing().collect::<Vec<_>>(), vec![0, 1]);

        asm.add_piece(1, &info[METADATA_PIECE_LEN..]).unwrap();
        assert!(!asm.is_complete());
        assert_eq!(asm.missing().collect::<Vec<_>>(), vec![0]);
        asm.add_piece(0, &info[..METADATA_PIECE_LEN]).unwrap();
        assert!(asm.is_complete());

        assert_eq!(asm.finish(hash), Some(info.clone()));
        // Wrong hash -> refused, even though complete.
        assert_eq!(asm.finish([0; 20]), None);
    }

    #[test]
    fn assembler_rejects_bad_pieces() {
        let mut asm = MetadataAssembler::new(200).unwrap();
        assert_eq!(asm.add_piece(5, &[0; 200]), Err(Error::BadPiece)); // OOB index
        // A huge index is out of range, not an offset that wrapped into
        // range: `index * 16 KiB` overflows a 32-bit usize from 2^18 up.
        // Unobservable on the 64-bit targets the tests run on, where the
        // product simply exceeds the size; asserted here so the arithmetic
        // stays checked.
        for huge in [1 << 18, u32::MAX / 2, u32::MAX] {
            assert_eq!(asm.piece_len(huge), 0, "index {huge}");
            assert_eq!(asm.add_piece(huge, &[]), Err(Error::BadPiece));
        }
        assert_eq!(asm.add_piece(0, &[0; 199]), Err(Error::BadPiece)); // wrong length
        assert!(asm.add_piece(0, &[0; 200]).is_ok());
    }
}
