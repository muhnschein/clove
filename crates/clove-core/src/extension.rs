//! BEP 10 extension protocol handshake (the `Extended` id-0 message).
//!
//! Shared negotiation layer for the extensions clove speaks: `i2p_pex`
//! (peer exchange, [`crate::pex`]) and `ut_metadata` (BEP 9 metadata,
//! [`crate::metadata`]). Each side sends an id-0 extended message whose
//! bencoded payload carries an `m` dict mapping extension *names* to the
//! message ids that side will listen on; you send an extension using the
//! id the *peer* advertised for it.
//!
//! Pure codec — the peer connection (torrent layer) owns when to send the
//! handshake and how to route subsequent ids.

use std::collections::BTreeMap;

use crate::bencode::{self, Value};

/// Extension name for i2p peer exchange (R4: i2psnark is normative).
pub const I2P_PEX: &str = "i2p_pex";
/// Extension name for BEP 9 metadata exchange.
pub const UT_METADATA: &str = "ut_metadata";

/// A parsed (or to-be-sent) extension handshake.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handshake {
    /// Extension name -> the message id the sender listens on for it.
    pub ids: BTreeMap<String, u8>,
    /// Total size of the info dictionary, advertised by peers that can
    /// serve metadata (BEP 9). `None` if absent.
    pub metadata_size: Option<usize>,
    /// The sender's client name/version (`v` key), if present.
    pub client: Option<String>,
}

impl Handshake {
    /// The message id the peer listens on for extension `name`, if it
    /// advertised support.
    #[must_use]
    pub fn id_for(&self, name: &str) -> Option<u8> {
        self.ids.get(name).copied()
    }

    /// Encode as the payload of an id-0 extended message.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut m = BTreeMap::new();
        for (name, id) in &self.ids {
            m.insert(name.clone().into_bytes(), Value::Int(i64::from(*id)));
        }
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), Value::Dict(m));
        if let Some(size) = self.metadata_size {
            root.insert(
                b"metadata_size".to_vec(),
                Value::Int(i64::try_from(size).unwrap_or(i64::MAX)),
            );
        }
        if let Some(client) = &self.client {
            root.insert(b"v".to_vec(), Value::Bytes(client.clone().into_bytes()));
        }
        bencode::encode(&Value::Dict(root))
    }

    /// Parse the payload of an id-0 extended message.
    ///
    /// # Errors
    ///
    /// Malformed bencode, or an `m` value that is not a dict of small
    /// non-negative integer ids.
    pub fn parse(payload: &[u8]) -> Result<Handshake, Error> {
        let root = bencode::decode(payload).map_err(|_| Error::Malformed)?;
        let mut ids = BTreeMap::new();
        // `m` is required by BEP 10 but tolerate its absence as "no
        // extensions" rather than erroring — some peers send a bare dict.
        if let Some(Value::Dict(m)) = root.get(b"m") {
            for (name, value) in m {
                let id = value.as_int().ok_or(Error::Malformed)?;
                // id 0 means "not supported / disabled"; skip it.
                if id == 0 {
                    continue;
                }
                let id = u8::try_from(id).map_err(|_| Error::Malformed)?;
                let name = String::from_utf8(name.clone()).map_err(|_| Error::Malformed)?;
                ids.insert(name, id);
            }
        }
        let metadata_size = root
            .get(b"metadata_size")
            .and_then(Value::as_int)
            .and_then(|n| usize::try_from(n).ok());
        let client = root.get(b"v").and_then(Value::as_str).map(str::to_owned);
        Ok(Handshake {
            ids,
            metadata_size,
            client,
        })
    }
}

/// Why an extension handshake could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not valid bencode, or the `m` dict was malformed.
    Malformed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("extension: malformed BEP 10 handshake")
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut ids = BTreeMap::new();
        ids.insert(I2P_PEX.to_owned(), 1);
        ids.insert(UT_METADATA.to_owned(), 2);
        let hs = Handshake {
            ids,
            metadata_size: Some(4096),
            client: Some("clove/0.1".to_owned()),
        };
        let parsed = Handshake::parse(&hs.encode()).unwrap();
        assert_eq!(parsed, hs);
        assert_eq!(parsed.id_for(I2P_PEX), Some(1));
        assert_eq!(parsed.id_for(UT_METADATA), Some(2));
        assert_eq!(parsed.id_for("nonesuch"), None);
    }

    #[test]
    fn id_zero_means_unsupported() {
        // A peer disabling ut_metadata by advertising id 0.
        let payload = bencode::encode(&{
            let mut m = BTreeMap::new();
            m.insert(b"ut_metadata".to_vec(), Value::Int(0));
            m.insert(b"i2p_pex".to_vec(), Value::Int(3));
            let mut root = BTreeMap::new();
            root.insert(b"m".to_vec(), Value::Dict(m));
            Value::Dict(root)
        });
        let hs = Handshake::parse(&payload).unwrap();
        assert_eq!(hs.id_for("ut_metadata"), None);
        assert_eq!(hs.id_for("i2p_pex"), Some(3));
    }

    #[test]
    fn tolerates_missing_m() {
        let payload = bencode::encode(&Value::Dict(BTreeMap::new()));
        let hs = Handshake::parse(&payload).unwrap();
        assert!(hs.ids.is_empty());
        assert_eq!(hs.metadata_size, None);
    }

    #[test]
    fn rejects_garbage_and_bad_ids() {
        assert_eq!(Handshake::parse(b"not bencode"), Err(Error::Malformed));
        // id out of u8 range.
        let payload = bencode::encode(&{
            let mut m = BTreeMap::new();
            m.insert(b"x".to_vec(), Value::Int(9999));
            let mut root = BTreeMap::new();
            root.insert(b"m".to_vec(), Value::Dict(m));
            Value::Dict(root)
        });
        assert_eq!(Handshake::parse(&payload), Err(Error::Malformed));
    }
}
