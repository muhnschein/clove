//! i2p peer exchange (`i2p_pex`), a BEP 10 extension.
//!
//! Peers periodically tell each other about other peers they know, so a
//! swarm can grow without every client hammering the tracker. The I2P form
//! (R4: i2psnark is normative, `docs/PROTOCOL.i2p-bt`) is a bencoded dict
//! with `added` and `dropped` byte strings, each a concatenation of 32-byte
//! destination hashes — no ports, and no IPv6 keys. The I2P specification
//! also allows an `added.f` flag string, which clove neither sends nor reads;
//! unknown keys are ignored, so a peer that sends one still interoperates
//! (`docs/PROTOCOL.i2p-bt` §4.3). Not clearnet `ut_pex`'s
//! flag bytes or IPv6 keys. Any extra keys a peer sends are ignored.
//!
//! Hostile input is a first-class concern here (§10: "PEX spam"): a message
//! advertising a huge peer set is rejected, and misaligned hash strings are
//! errors, not best-effort guesses.

use i2pnet::DestHash;

use crate::bencode::{self, Value};

/// Largest number of peers clove accepts in one PEX message across `added`
/// and `dropped` combined. i2psnark sends far fewer; a message over this is
/// treated as spam and rejected.
pub const MAX_PEX_PEERS: usize = 512;

/// A decoded (or to-be-sent) `i2p_pex` message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PexMessage {
    /// Peers the sender newly knows about.
    pub added: Vec<DestHash>,
    /// Peers the sender has dropped.
    pub dropped: Vec<DestHash>,
}

impl PexMessage {
    /// Whether this message carries nothing (skip sending it).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.dropped.is_empty()
    }

    /// Encode as the payload of the negotiated `i2p_pex` extended message.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut root = std::collections::BTreeMap::new();
        root.insert(b"added".to_vec(), Value::Bytes(pack(&self.added)));
        root.insert(b"dropped".to_vec(), Value::Bytes(pack(&self.dropped)));
        bencode::encode(&Value::Dict(root))
    }

    /// Parse an `i2p_pex` payload.
    ///
    /// # Errors
    ///
    /// Malformed bencode, an `added`/`dropped` string whose length is not a
    /// multiple of 32, or a combined peer count over [`MAX_PEX_PEERS`].
    pub fn parse(payload: &[u8]) -> Result<PexMessage, Error> {
        let root = bencode::decode(payload).map_err(|_| Error::Malformed)?;
        let added = unpack(root.get(b"added"))?;
        let dropped = unpack(root.get(b"dropped"))?;
        if added.len() + dropped.len() > MAX_PEX_PEERS {
            return Err(Error::TooManyPeers);
        }
        Ok(PexMessage { added, dropped })
    }
}

fn pack(peers: &[DestHash]) -> Vec<u8> {
    let mut out = Vec::with_capacity(peers.len() * 32);
    for p in peers {
        out.extend_from_slice(&p.0);
    }
    out
}

/// Decode a concatenated-32-byte-hash field. Absent field = empty.
fn unpack(field: Option<&Value>) -> Result<Vec<DestHash>, Error> {
    let bytes = match field {
        Some(Value::Bytes(b)) => b.as_slice(),
        Some(_) => return Err(Error::Malformed),
        None => return Ok(Vec::new()),
    };
    if !bytes.len().is_multiple_of(32) {
        return Err(Error::Malformed);
    }
    Ok(bytes
        .chunks_exact(32)
        .map(|c| {
            let mut h = [0u8; 32];
            h.copy_from_slice(c);
            DestHash(h)
        })
        .collect())
}

/// Why an `i2p_pex` message was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not valid bencode, wrong field type, or a misaligned hash string.
    Malformed,
    /// More peers than [`MAX_PEX_PEERS`] — treated as spam.
    TooManyPeers,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Malformed => f.write_str("i2p_pex: malformed message"),
            Error::TooManyPeers => f.write_str("i2p_pex: too many peers (spam)"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let msg = PexMessage {
            added: vec![DestHash([1; 32]), DestHash([2; 32])],
            dropped: vec![DestHash([3; 32])],
        };
        assert_eq!(PexMessage::parse(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn empty_message() {
        let msg = PexMessage::default();
        assert!(msg.is_empty());
        assert_eq!(PexMessage::parse(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn absent_fields_are_empty() {
        let payload = bencode::encode(&Value::Dict(std::collections::BTreeMap::new()));
        assert_eq!(PexMessage::parse(&payload).unwrap(), PexMessage::default());
    }

    #[test]
    fn rejects_misaligned_hashes() {
        let payload = bencode::encode(&{
            let mut m = std::collections::BTreeMap::new();
            m.insert(b"added".to_vec(), Value::Bytes(vec![0u8; 40])); // not %32
            Value::Dict(m)
        });
        assert_eq!(PexMessage::parse(&payload), Err(Error::Malformed));
    }

    #[test]
    fn rejects_pex_spam() {
        let payload = bencode::encode(&{
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                b"added".to_vec(),
                Value::Bytes(vec![0u8; 32 * (MAX_PEX_PEERS + 1)]),
            );
            Value::Dict(m)
        });
        assert_eq!(PexMessage::parse(&payload), Err(Error::TooManyPeers));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(PexMessage::parse(b"xxx"), Err(Error::Malformed));
    }
}
