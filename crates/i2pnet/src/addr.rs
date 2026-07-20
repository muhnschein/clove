//! I2P address encoding: converting between our 32-byte [`DestHash`] and the
//! textual forms SAM speaks.
//!
//! The engine identifies peers only by [`DestHash`] (the 32-byte SHA-256 of
//! a full I2P destination). SAM, however, dials and reports *strings*:
//!
//! - **Outbound** — to dial a peer we only hold the hash for, we form its
//!   `<b32>.b32.i2p` address: RFC 4648 base32 (lowercase, unpadded) of the
//!   32 hash bytes. The router resolves that to the full destination.
//! - **Inbound / naming** — SAM hands us a peer's full base64 destination
//!   (I2P's `-`/`~` base64 alphabet); its [`DestHash`] is the SHA-256 of the
//!   decoded destination bytes.
//!
//! These compose: `to_b32` of `from_b64_destination(dest)` is the canonical
//! b32 address of `dest`, because the b32 label *is* base32(SHA-256(dest)).
//! All of this is pure and unit-tested with RFC 4648 vectors — no router
//! needed.

use sha2::{Digest, Sha256};

use crate::DestHash;

/// RFC 4648 base32 alphabet, lowercase (I2P b32 addresses are lowercase).
const B32_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// I2P's base64 alphabet: standard base64 with `+`→`-` and `/`→`~`.
const I2P_B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-~";

impl DestHash {
    /// This hash's `<b32>.b32.i2p` address — what we hand SAM to dial a peer.
    #[must_use]
    pub fn to_b32(&self) -> String {
        let mut s = base32_encode(&self.0);
        s.push_str(".b32.i2p");
        s
    }

    /// Parse a b32 peer address — `<52 base32 chars>` with or without the
    /// `.b32.i2p` suffix — back into its [`DestHash`]. The inverse of
    /// [`to_b32`](DestHash::to_b32); `None` if the label is not exactly 32
    /// decoded bytes.
    #[must_use]
    pub fn from_b32(text: &str) -> Option<DestHash> {
        let label = text.trim().strip_suffix(".b32.i2p").unwrap_or(text.trim());
        let bytes = base32_decode(label)?;
        let hash: [u8; 32] = bytes.try_into().ok()?;
        Some(DestHash(hash))
    }

    /// The [`DestHash`] of a full base64 I2P destination (as returned by SAM
    /// naming lookups and inbound streams): SHA-256 of the decoded bytes.
    ///
    /// Any trailing `.i2p`-style suffix or whitespace SAM may append is
    /// trimmed; returns `None` if the base64 body is malformed.
    #[must_use]
    pub fn from_b64_destination(dest: &str) -> Option<DestHash> {
        let trimmed = dest.trim();
        // SAM sometimes suffixes a base32 label; the destination proper is
        // the leading base64 run. Cut at the first character outside the
        // I2P base64 alphabet (e.g. a stray '.' or newline).
        let body: String = trimmed
            .chars()
            .take_while(|&c| c == '=' || I2P_B64_ALPHABET.contains(&(c as u8)))
            .collect();
        let bytes = i2p_base64_decode(&body)?;
        if bytes.is_empty() {
            return None;
        }
        Some(DestHash(Sha256::digest(&bytes).into()))
    }
}

/// RFC 4648 base32 encode, lowercase, unpadded.
#[must_use]
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1f) as usize;
            out.push(char::from(B32_ALPHABET[idx]));
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(char::from(B32_ALPHABET[idx]));
    }
    out
}

/// RFC 4648 base32 decode of lowercase, unpadded input.
///
/// # Errors
/// Returns `None` on any character outside the lowercase base32 alphabet.
#[must_use]
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let val = u32::try_from(B32_ALPHABET.iter().position(|&a| a == c)?).ok()?;
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Encode bytes in I2P's base64 alphabet, padded with `=`.
#[must_use]
pub fn i2p_base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(I2P_B64_ALPHABET[(n >> 18) as usize & 0x3f]));
        out.push(char::from(I2P_B64_ALPHABET[(n >> 12) as usize & 0x3f]));
        if chunk.len() > 1 {
            out.push(char::from(I2P_B64_ALPHABET[(n >> 6) as usize & 0x3f]));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(I2P_B64_ALPHABET[n as usize & 0x3f]));
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode I2P base64 (the `-`/`~` alphabet), tolerating `=` padding.
///
/// # Errors
/// Returns `None` on any character outside the alphabet (padding aside) or
/// on a truncated final group.
#[must_use]
pub fn i2p_base64_decode(s: &str) -> Option<Vec<u8>> {
    let symbols: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(symbols.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for c in symbols {
        let val = u32::try_from(I2P_B64_ALPHABET.iter().position(|&a| a == c)?).ok()?;
        buffer = (buffer << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_rfc4648_vectors() {
        // RFC 4648 vectors, lowercased and unpadded.
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "my");
        assert_eq!(base32_encode(b"fo"), "mzxq");
        assert_eq!(base32_encode(b"foo"), "mzxw6");
        assert_eq!(base32_encode(b"foob"), "mzxw6yq");
        assert_eq!(base32_encode(b"fooba"), "mzxw6ytb");
        assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn base32_round_trips() {
        for input in [&b""[..], b"f", b"foobar", &[0u8; 32], &[0xFF; 32]] {
            let encoded = base32_encode(input);
            assert_eq!(base32_decode(&encoded).unwrap(), input, "input {input:?}");
        }
        assert!(base32_decode("MZXW6").is_none(), "uppercase not accepted");
        assert!(base32_decode("0189").is_none(), "0/1/8/9 not in alphabet");
    }

    #[test]
    fn i2p_base64_known_vector() {
        // "SGVsbG8=" is standard base64 of "Hello" with no +/ chars, so it
        // is also valid I2P base64.
        assert_eq!(i2p_base64_decode("SGVsbG8=").unwrap(), b"Hello");
    }

    #[test]
    fn i2p_base64_round_trips_and_uses_i2p_alphabet() {
        // Bytes chosen so standard base64 would yield '+' and '/', which
        // must appear as '-' and '~' here.
        let data = [0xFBu8, 0xFF, 0xBF, 0x00, 0x10, 0x83];
        let encoded = i2p_base64_encode(&data);
        assert!(
            encoded.contains('-') || encoded.contains('~'),
            "uses I2P alphabet"
        );
        assert!(!encoded.contains('+') && !encoded.contains('/'));
        assert_eq!(i2p_base64_decode(&encoded).unwrap(), data);
        // A standard-alphabet string with '+' must be rejected.
        assert!(i2p_base64_decode("ab+d").is_none());
    }

    #[test]
    fn dest_hash_to_b32() {
        let hash = DestHash([0x11; 32]);
        let addr = hash.to_b32();
        assert!(addr.ends_with(".b32.i2p"));
        let label = addr.strip_suffix(".b32.i2p").unwrap();
        assert_eq!(label.len(), 52); // 256 bits / 5, rounded up
        assert_eq!(base32_decode(label).unwrap(), hash.0);
    }

    #[test]
    fn dest_hash_b32_round_trips() {
        let hash = DestHash([0xA5; 32]);
        let addr = hash.to_b32();
        // With and without the suffix, plus surrounding whitespace.
        assert_eq!(DestHash::from_b32(&addr), Some(hash));
        let label = addr.strip_suffix(".b32.i2p").unwrap();
        assert_eq!(DestHash::from_b32(label), Some(hash));
        assert_eq!(DestHash::from_b32(&format!(" {addr}\n")), Some(hash));
        // Wrong length and bad alphabet are rejected.
        assert!(DestHash::from_b32("abc").is_none());
        assert!(DestHash::from_b32(&"A".repeat(52)).is_none());
    }

    #[test]
    fn dest_hash_from_b64_matches_sha256() {
        // A stand-in "destination": some bytes. Its DestHash is the SHA-256
        // of those bytes, regardless of encoding.
        let dest_bytes = [0x42u8; 48];
        let b64 = i2p_base64_encode(&dest_bytes);
        let expected = DestHash(Sha256::digest(dest_bytes).into());
        assert_eq!(DestHash::from_b64_destination(&b64).unwrap(), expected);

        // Trailing whitespace / suffix is tolerated.
        let with_suffix = format!("{b64}\n");
        assert_eq!(
            DestHash::from_b64_destination(&with_suffix).unwrap(),
            expected
        );

        assert!(DestHash::from_b64_destination("").is_none());
    }
}
