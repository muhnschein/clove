//! `BitTorrent` peer wire protocol (BEP 3), extension protocol framing
//! (BEP 10), and the fast extension (BEP 6, per Q3) — for the I2P dialect,
//! which is byte-identical to clearnet BT above the stream (no ports ever
//! appear in these messages; peers are I2P destinations at a lower layer).
//!
//! Two layers, kept separate so the parser is pure and fuzzable:
//!
//! - [`Message::parse`] / [`Message::encode_into`] work on bytes with no
//!   I/O. `parse` is hostile-input hardened: message and block lengths are
//!   bounded before anything is allocated.
//! - [`read_frame`] / [`write_message`] add blocking framing over any
//!   `Read`/`Write` (an `i2pnet` stream in production, the mock in tests).
//!
//! Peer-connection *semantics* — when a message is legal, what it does to
//! choke/interest state — live in `peer`; this module only speaks the
//! grammar.

use std::io::{self, Read, Write};

/// The BEP 3 handshake protocol string.
pub const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";

/// Standard block size (16 KiB). Requests and piece blocks may not exceed
/// it; i2psnark uses exactly this.
pub const BLOCK_LEN: u32 = 16 * 1024;

/// Hard cap on a single message body, guarding allocation against a hostile
/// length prefix. Large enough for a bitfield of ~8 million pieces plus any
/// block; the peer layer additionally rejects bitfields that disagree with
/// the torrent's real piece count.
pub const MAX_MESSAGE_LEN: u32 = 1024 * 1024;

/// A contiguous block within a piece — the unit of `request`, `cancel`, and
/// `reject`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockRequest {
    /// Piece index.
    pub index: u32,
    /// Byte offset of the block within the piece.
    pub begin: u32,
    /// Block length in bytes.
    pub length: u32,
}

/// A decoded peer wire message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Message {
    /// Zero-length keep-alive.
    KeepAlive,
    /// Sender will not serve the receiver (id 0).
    Choke,
    /// Sender will serve the receiver (id 1).
    Unchoke,
    /// Receiver has pieces the sender wants (id 2).
    Interested,
    /// Receiver has nothing the sender wants (id 3).
    NotInterested,
    /// Sender now has piece `index` (id 4).
    Have(u32),
    /// Sender's complete piece set (id 5).
    Bitfield(Vec<u8>),
    /// Please send this block (id 6).
    Request(BlockRequest),
    /// A block of piece data (id 7).
    Piece {
        /// Piece index.
        index: u32,
        /// Offset within the piece.
        begin: u32,
        /// The block bytes.
        block: Vec<u8>,
    },
    /// Withdraw an earlier request (id 8).
    Cancel(BlockRequest),
    /// BEP 6: sender has every piece (id 14).
    HaveAll,
    /// BEP 6: sender has no pieces (id 15).
    HaveNone,
    /// BEP 6: sender suggests the receiver download this piece (id 13).
    SuggestPiece(u32),
    /// BEP 6: sender refuses a request it will not serve (id 16).
    RejectRequest(BlockRequest),
    /// BEP 6: receiver may request this piece even while choked (id 17).
    AllowedFast(u32),
    /// BEP 10 extended message (id 20): sub-id 0 is the extension
    /// handshake, other ids are per-session negotiated. Semantics (PEX,
    /// metadata) are handled in later phases; the codec only frames it.
    Extended {
        /// Extended message sub-id (0 = handshake).
        id: u8,
        /// Bencoded (handshake) or extension-defined payload.
        payload: Vec<u8>,
    },
}

/// Why a message could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The message id is not one clove speaks.
    UnknownId(u8),
    /// A fixed-layout message had the wrong body length.
    BadLength,
    /// A request/piece block length exceeds [`BLOCK_LEN`].
    BlockTooLarge,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownId(id) => write!(f, "wire: unknown message id {id}"),
            Error::BadLength => write!(f, "wire: message body has the wrong length"),
            Error::BlockTooLarge => write!(f, "wire: block length exceeds the 16 KiB cap"),
        }
    }
}

impl std::error::Error for Error {}

// Message ids.
const ID_CHOKE: u8 = 0;
const ID_UNCHOKE: u8 = 1;
const ID_INTERESTED: u8 = 2;
const ID_NOT_INTERESTED: u8 = 3;
const ID_HAVE: u8 = 4;
const ID_BITFIELD: u8 = 5;
const ID_REQUEST: u8 = 6;
const ID_PIECE: u8 = 7;
const ID_CANCEL: u8 = 8;
const ID_SUGGEST: u8 = 13;
const ID_HAVE_ALL: u8 = 14;
const ID_HAVE_NONE: u8 = 15;
const ID_REJECT: u8 = 16;
const ID_ALLOWED_FAST: u8 = 17;
const ID_EXTENDED: u8 = 20;

impl Message {
    /// Parse a message from its body (everything after the 4-byte length
    /// prefix). An empty body is a keep-alive.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownId`] for ids clove does not speak, [`Error::BadLength`]
    /// for fixed-layout messages of the wrong size, and
    /// [`Error::BlockTooLarge`] for a request or block over [`BLOCK_LEN`].
    pub fn parse(body: &[u8]) -> Result<Message, Error> {
        let Some((&id, rest)) = body.split_first() else {
            return Ok(Message::KeepAlive);
        };
        match id {
            ID_CHOKE => empty(rest, Message::Choke),
            ID_UNCHOKE => empty(rest, Message::Unchoke),
            ID_INTERESTED => empty(rest, Message::Interested),
            ID_NOT_INTERESTED => empty(rest, Message::NotInterested),
            ID_HAVE => Ok(Message::Have(u32_at(rest)?)),
            ID_BITFIELD => Ok(Message::Bitfield(rest.to_vec())),
            ID_REQUEST => Ok(Message::Request(block_request(rest)?)),
            ID_CANCEL => Ok(Message::Cancel(block_request(rest)?)),
            ID_PIECE => {
                if rest.len() < 8 {
                    return Err(Error::BadLength);
                }
                let index = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                let begin = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]);
                let block = &rest[8..];
                if block.len() as u64 > u64::from(BLOCK_LEN) {
                    return Err(Error::BlockTooLarge);
                }
                Ok(Message::Piece {
                    index,
                    begin,
                    block: block.to_vec(),
                })
            }
            ID_SUGGEST => Ok(Message::SuggestPiece(u32_at(rest)?)),
            ID_HAVE_ALL => empty(rest, Message::HaveAll),
            ID_HAVE_NONE => empty(rest, Message::HaveNone),
            ID_REJECT => Ok(Message::RejectRequest(block_request(rest)?)),
            ID_ALLOWED_FAST => Ok(Message::AllowedFast(u32_at(rest)?)),
            ID_EXTENDED => {
                let Some((&sub, payload)) = rest.split_first() else {
                    return Err(Error::BadLength);
                };
                Ok(Message::Extended {
                    id: sub,
                    payload: payload.to_vec(),
                })
            }
            other => Err(Error::UnknownId(other)),
        }
    }

    /// Append the full on-wire frame (4-byte big-endian length prefix and
    /// body) to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0, 0, 0, 0]); // length placeholder
        match self {
            Message::KeepAlive => {}
            Message::Choke => out.push(ID_CHOKE),
            Message::Unchoke => out.push(ID_UNCHOKE),
            Message::Interested => out.push(ID_INTERESTED),
            Message::NotInterested => out.push(ID_NOT_INTERESTED),
            Message::Have(index) => {
                out.push(ID_HAVE);
                out.extend_from_slice(&index.to_be_bytes());
            }
            Message::Bitfield(bits) => {
                out.push(ID_BITFIELD);
                out.extend_from_slice(bits);
            }
            Message::Request(b) => put_block(out, ID_REQUEST, *b),
            Message::Cancel(b) => put_block(out, ID_CANCEL, *b),
            Message::Piece {
                index,
                begin,
                block,
            } => {
                out.push(ID_PIECE);
                out.extend_from_slice(&index.to_be_bytes());
                out.extend_from_slice(&begin.to_be_bytes());
                out.extend_from_slice(block);
            }
            Message::SuggestPiece(index) => {
                out.push(ID_SUGGEST);
                out.extend_from_slice(&index.to_be_bytes());
            }
            Message::HaveAll => out.push(ID_HAVE_ALL),
            Message::HaveNone => out.push(ID_HAVE_NONE),
            Message::RejectRequest(b) => put_block(out, ID_REJECT, *b),
            Message::AllowedFast(index) => {
                out.push(ID_ALLOWED_FAST);
                out.extend_from_slice(&index.to_be_bytes());
            }
            Message::Extended { id, payload } => {
                out.push(ID_EXTENDED);
                out.push(*id);
                out.extend_from_slice(payload);
            }
        }
        // Bodies we construct are far under u32::MAX (bitfields aside, all
        // are tiny; a bitfield for any real torrent is well under 4 GiB).
        // The saturating branch is unreachable but keeps us panic-free.
        let body_len = u32::try_from(out.len() - start - 4).unwrap_or(u32::MAX);
        out[start..start + 4].copy_from_slice(&body_len.to_be_bytes());
    }

    /// The full on-wire frame as a fresh buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }
}

fn empty(rest: &[u8], msg: Message) -> Result<Message, Error> {
    if rest.is_empty() {
        Ok(msg)
    } else {
        Err(Error::BadLength)
    }
}

fn u32_at(rest: &[u8]) -> Result<u32, Error> {
    match rest {
        [a, b, c, d] => Ok(u32::from_be_bytes([*a, *b, *c, *d])),
        _ => Err(Error::BadLength),
    }
}

fn block_request(rest: &[u8]) -> Result<BlockRequest, Error> {
    let [i0, i1, i2, i3, b0, b1, b2, b3, l0, l1, l2, l3] = rest else {
        return Err(Error::BadLength);
    };
    let length = u32::from_be_bytes([*l0, *l1, *l2, *l3]);
    if length > BLOCK_LEN {
        return Err(Error::BlockTooLarge);
    }
    Ok(BlockRequest {
        index: u32::from_be_bytes([*i0, *i1, *i2, *i3]),
        begin: u32::from_be_bytes([*b0, *b1, *b2, *b3]),
        length,
    })
}

fn put_block(out: &mut Vec<u8>, id: u8, b: BlockRequest) {
    out.push(id);
    out.extend_from_slice(&b.index.to_be_bytes());
    out.extend_from_slice(&b.begin.to_be_bytes());
    out.extend_from_slice(&b.length.to_be_bytes());
}

/// The BEP 3 handshake, including negotiated extension bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Handshake {
    /// The torrent's info-hash.
    pub info_hash: [u8; 20],
    /// The sender's 20-byte peer id.
    pub peer_id: [u8; 20],
    /// Extension protocol bits the sender advertises.
    pub extensions: Extensions,
}

/// Extension bits carried in the handshake's reserved field.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Extensions {
    /// BEP 10 extension protocol supported.
    pub extended: bool,
    /// BEP 6 fast extension supported.
    pub fast: bool,
}

/// Full length of a handshake on the wire: 1 + 19 + 8 + 20 + 20.
pub const HANDSHAKE_LEN: usize = 68;

impl Handshake {
    /// Serialize the 68-byte handshake.
    #[must_use]
    pub fn encode(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        out[0] = 19;
        out[1..20].copy_from_slice(PROTOCOL);
        // Reserved bits: fast = reserved[7] & 0x04, extended = reserved[5] & 0x10.
        if self.extensions.fast {
            out[27] |= 0x04;
        }
        if self.extensions.extended {
            out[25] |= 0x10;
        }
        out[28..48].copy_from_slice(&self.info_hash);
        out[48..68].copy_from_slice(&self.peer_id);
        out
    }

    /// Parse a 68-byte handshake.
    ///
    /// # Errors
    ///
    /// Returns [`HandshakeError::BadProtocol`] if the length byte or
    /// protocol string is not the `BitTorrent` handshake.
    pub fn parse(buf: &[u8; HANDSHAKE_LEN]) -> Result<Handshake, HandshakeError> {
        if buf[0] != 19 || &buf[1..20] != PROTOCOL {
            return Err(HandshakeError::BadProtocol);
        }
        let mut info_hash = [0u8; 20];
        let mut peer_id = [0u8; 20];
        info_hash.copy_from_slice(&buf[28..48]);
        peer_id.copy_from_slice(&buf[48..68]);
        Ok(Handshake {
            info_hash,
            peer_id,
            extensions: Extensions {
                fast: buf[27] & 0x04 != 0,
                extended: buf[25] & 0x10 != 0,
            },
        })
    }
}

/// Why a handshake was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// Length byte or protocol string was not the `BitTorrent` handshake.
    BadProtocol,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wire: not a BitTorrent handshake")
    }
}

impl std::error::Error for HandshakeError {}

/// Read one message frame (blocking): the 4-byte length prefix followed by
/// that many bytes, returned as the body (empty body = keep-alive). Use
/// [`Message::parse`] on the result.
///
/// `max_len` bounds the declared length before allocation; pass the
/// torrent's real ceiling (bitfield size or [`BLOCK_LEN`] plus header),
/// never more than [`MAX_MESSAGE_LEN`].
///
/// # Errors
///
/// I/O errors from the reader, or [`io::ErrorKind::InvalidData`] if the
/// declared length exceeds `max_len`.
pub fn read_frame<R: Read>(reader: &mut R, max_len: u32) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    read_frame_into(reader, max_len, &mut body)?;
    Ok(body)
}

/// [`read_frame`] into a caller-owned buffer, which is resized to the frame's
/// length and filled.
///
/// What a peer connection uses. A reader loop calling [`read_frame`] allocates
/// a buffer per message and frees it a moment later — 16 KiB per block, per
/// peer, for the life of a download — and a heap doing that on hundreds of
/// threads at once keeps the pages rather than returning them, so the daemon's
/// resident size drifts upward and stays there. One buffer per connection,
/// grown to the largest frame that connection has carried and reused after
/// that, has the same peak and no churn.
///
/// The buffer is still bounded by `max_len`: it is sized to a length the peer
/// declared, and the ceiling is checked before the resize, exactly as
/// [`read_frame`] does.
///
/// # Errors
///
/// As [`read_frame`].
pub fn read_frame_into<R: Read>(
    reader: &mut R,
    max_len: u32,
    body: &mut Vec<u8>,
) -> io::Result<()> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > max_len.min(MAX_MESSAGE_LEN) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wire: peer declared an oversized message",
        ));
    }
    body.clear();
    body.resize(len as usize, 0);
    reader.read_exact(body)
}

/// Encode and write one message frame (blocking).
///
/// # Errors
///
/// I/O errors from the writer.
pub fn write_message<W: Write>(writer: &mut W, msg: &Message) -> io::Result<()> {
    writer.write_all(&msg.encode())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: &Message) {
        let frame = msg.encode();
        // First four bytes are the body length.
        let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
        assert_eq!(
            declared as usize,
            frame.len() - 4,
            "length prefix for {msg:?}"
        );
        let parsed = Message::parse(&frame[4..]).unwrap();
        assert_eq!(&parsed, msg);
    }

    #[test]
    fn all_messages_round_trip() {
        let block = BlockRequest {
            index: 3,
            begin: 16384,
            length: BLOCK_LEN,
        };
        for msg in [
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(42),
            Message::Bitfield(vec![0xAB, 0xCD]),
            Message::Request(block),
            Message::Piece {
                index: 1,
                begin: 0,
                block: vec![9; 100],
            },
            Message::Cancel(block),
            Message::HaveAll,
            Message::HaveNone,
            Message::SuggestPiece(7),
            Message::RejectRequest(block),
            Message::AllowedFast(7),
            Message::Extended {
                id: 0,
                payload: b"d1:md6:ut_pexi1eee".to_vec(),
            },
        ] {
            roundtrip(&msg);
        }
    }

    #[test]
    fn keep_alive_is_empty_frame() {
        assert_eq!(Message::KeepAlive.encode(), vec![0, 0, 0, 0]);
        assert_eq!(Message::parse(&[]).unwrap(), Message::KeepAlive);
    }

    #[test]
    fn rejects_malformed_bodies() {
        assert_eq!(Message::parse(&[ID_CHOKE, 0]), Err(Error::BadLength));
        assert_eq!(Message::parse(&[ID_HAVE, 0, 0]), Err(Error::BadLength));
        assert_eq!(Message::parse(&[ID_REQUEST, 0, 0]), Err(Error::BadLength));
        assert_eq!(Message::parse(&[ID_EXTENDED]), Err(Error::BadLength));
        assert_eq!(Message::parse(&[99]), Err(Error::UnknownId(99)));
    }

    #[test]
    fn rejects_oversized_blocks() {
        // request with length = BLOCK_LEN + 1
        let mut body = vec![ID_REQUEST];
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(BLOCK_LEN + 1).to_be_bytes());
        assert_eq!(Message::parse(&body), Err(Error::BlockTooLarge));

        // piece with an oversized block
        let mut body = vec![ID_PIECE];
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend(vec![0u8; BLOCK_LEN as usize + 1]);
        assert_eq!(Message::parse(&body), Err(Error::BlockTooLarge));
    }

    #[test]
    fn handshake_round_trip_with_extensions() {
        let hs = Handshake {
            info_hash: [1; 20],
            peer_id: *b"-CV0001-abcdefghijkl",
            extensions: Extensions {
                extended: true,
                fast: true,
            },
        };
        let encoded = hs.encode();
        assert_eq!(encoded.len(), HANDSHAKE_LEN);
        assert_eq!(Handshake::parse(&encoded).unwrap(), hs);

        // No extensions.
        let plain = Handshake {
            extensions: Extensions::default(),
            ..hs
        };
        assert_eq!(Handshake::parse(&plain.encode()).unwrap(), plain);
    }

    #[test]
    fn handshake_rejects_wrong_protocol() {
        let mut buf = [0u8; HANDSHAKE_LEN];
        buf[0] = 19;
        buf[1..20].copy_from_slice(b"NotTorrent protocol");
        assert_eq!(Handshake::parse(&buf), Err(HandshakeError::BadProtocol));
    }

    #[test]
    fn framing_over_a_reader() {
        let mut buf = Vec::new();
        write_message(&mut buf, &Message::Have(5)).unwrap();
        write_message(
            &mut buf,
            &Message::Piece {
                index: 0,
                begin: 0,
                block: vec![1, 2, 3],
            },
        )
        .unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let f1 = read_frame(&mut cursor, MAX_MESSAGE_LEN).unwrap();
        assert_eq!(Message::parse(&f1).unwrap(), Message::Have(5));
        let f2 = read_frame(&mut cursor, MAX_MESSAGE_LEN).unwrap();
        assert_eq!(
            Message::parse(&f2).unwrap(),
            Message::Piece {
                index: 0,
                begin: 0,
                block: vec![1, 2, 3]
            }
        );
    }

    /// A peer's reader keeps one buffer for the life of the connection, so a
    /// long frame followed by a short one must leave nothing of the long one
    /// behind — the failure mode is a message that parses as something the peer
    /// never sent.
    #[test]
    fn a_reused_frame_buffer_carries_nothing_between_messages() {
        let mut buf = Vec::new();
        write_message(
            &mut buf,
            &Message::Piece {
                index: 7,
                begin: 0,
                block: vec![0xAB; 4096],
            },
        )
        .unwrap();
        write_message(&mut buf, &Message::Have(5)).unwrap();
        write_message(&mut buf, &Message::KeepAlive).unwrap();
        write_message(&mut buf, &Message::Interested).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let mut body = Vec::new();
        let expected = [
            Message::Piece {
                index: 7,
                begin: 0,
                block: vec![0xAB; 4096],
            },
            Message::Have(5),
            Message::KeepAlive,
            Message::Interested,
        ];
        for want in expected {
            read_frame_into(&mut cursor, MAX_MESSAGE_LEN, &mut body).unwrap();
            assert_eq!(Message::parse(&body).unwrap(), want);
        }
        // And the stream is exhausted, not merely re-reading the last buffer.
        assert!(read_frame_into(&mut cursor, MAX_MESSAGE_LEN, &mut body).is_err());
    }

    #[test]
    fn framing_rejects_oversized_declared_length() {
        let mut evil = Vec::new();
        evil.extend_from_slice(&(5000u32).to_be_bytes());
        let mut cursor = std::io::Cursor::new(evil);
        let err = read_frame(&mut cursor, 100).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
