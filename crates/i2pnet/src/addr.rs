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
        let bytes = destination_bytes(dest)?;
        Some(DestHash(Sha256::digest(&bytes).into()))
    }
}

/// Offset of the certificate inside a destination: 256-byte public key plus
/// 128-byte signing key.
const CERT_OFFSET: usize = 384;

/// A destination is the key material plus a certificate: three bytes of
/// header (type, then a two-byte big-endian payload length) and the payload.
const CERT_HEADER: usize = 3;

/// The **public destination** at the front of a SAM `DESTINATION=` field, as
/// raw bytes.
///
/// This exists because `SAMv3`'s `SESSION STATUS` reply does not carry the
/// destination. It carries the session's **private key blob**, of which the
/// public destination is merely the first 387-or-so bytes; the rest is the
/// private crypto and signing keys. The distinction is invisible if you never
/// look — both are one long base64 run — and clove did not look.
///
/// The cost of not looking, measured against a live tracker on 2026-07-27:
///
/// - Every announce sent our **private keys** to the tracker in the `ip`
///   parameter. postman's tracker refused each one as "in violation of the
///   site's policy", which was the correct and generous response.
/// - Every `DestHash` we derived for ourselves — the identity printed at
///   startup, published to peers over PEX, and dialled by the loopback tests
///   — was the SHA-256 of the private blob rather than of the destination. It
///   named nothing. A router asked to resolve it could only fail, which is
///   what "leaseSet not found" had been telling us since the first live run,
///   while we read it as a router's fault (`PROTOCOL.i2p-bt` §5.1c).
///
/// The length is not fixed: the certificate's own header says how long its
/// payload is, so a destination is `387 + payload` bytes. Anything shorter
/// than a certificate header is not a destination at all.
///
/// Returns `None` rather than guessing, because a wrong answer here is a
/// wrong identity, and a wrong identity fails silently everywhere at once.
#[must_use]
pub fn destination_bytes(dest: &str) -> Option<Vec<u8>> {
    // SAM sometimes suffixes a base32 label; the blob proper is the leading
    // base64 run. Cut at the first character outside the I2P base64 alphabet
    // (e.g. a stray '.' or newline).
    let body: String = dest
        .trim()
        .chars()
        .take_while(|&c| c == '=' || I2P_B64_ALPHABET.contains(&(c as u8)))
        .collect();
    let mut bytes = i2p_base64_decode(&body)?;
    let len = destination_len(&bytes)?;
    bytes.truncate(len);
    Some(bytes)
}

/// How many leading bytes of `blob` are the public destination.
#[must_use]
pub fn destination_len(blob: &[u8]) -> Option<usize> {
    let header_end = CERT_OFFSET + CERT_HEADER;
    if blob.len() < header_end {
        return None;
    }
    let payload = usize::from(u16::from_be_bytes([
        blob[CERT_OFFSET + 1],
        blob[CERT_OFFSET + 2],
    ]));
    let total = header_end.checked_add(payload)?;
    (blob.len() >= total).then_some(total)
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
/// Strict about the leftover bits of the final character. 52 characters carry
/// 260 bits but a destination hash is 256, and a lax decoder therefore maps
/// sixteen different-looking labels onto one identity — `…ljnj` and `…ljna`
/// would name the same peer. clove's encoder never emits those bits set, so
/// input that has them set did not come from clove and is refused, on the
/// same principle as a resume bitfield's trailing bits (`docs/STATE-FORMAT.md`)
/// and an unknown config key: a decoder must not accept what its encoder
/// cannot produce.
///
/// # Errors
/// Returns `None` on any character outside the lowercase base32 alphabet, or
/// on a final character whose spare bits are not zero.
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
    if bits > 0 && (buffer & ((1 << bits) - 1)) != 0 {
        return None;
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
/// Strict in the same two ways [`base32_decode`] is, and for the same reason: a
/// decoder must not accept what its own encoder cannot produce. A single
/// dangling symbol carries six bits and encodes no byte, and the spare bits of
/// the final group are always zero in a real encoding — so a lax decoder maps
/// several different-looking destinations onto one identity, which for a peer
/// address means two names for the same peer, or worse, one name the router and
/// clove disagree about.
///
/// # Errors
/// Returns `None` on any character outside the alphabet (padding aside), on a
/// truncated final group (one leftover symbol), or when the final group's spare
/// bits are not zero.
#[must_use]
pub fn i2p_base64_decode(s: &str) -> Option<Vec<u8>> {
    let symbols: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    if symbols.len() % 4 == 1 {
        return None; // six bits alone encode nothing
    }
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
    if bits > 0 && (buffer & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic destination blob: 256-byte key field, 128-byte signing
    /// field, then a KEY certificate. `extra` bytes of private key material
    /// are appended, which is what SAM actually hands back and what must
    /// never reach the wire or the identity (see [`destination_bytes`]).
    pub(super) fn dest_blob(fill: u8, cert_payload: &[u8], extra: usize) -> Vec<u8> {
        let mut blob = vec![fill; CERT_OFFSET];
        blob.push(0x05);
        let len = u16::try_from(cert_payload.len()).expect("small cert");
        blob.extend_from_slice(&len.to_be_bytes());
        blob.extend_from_slice(cert_payload);
        blob.extend(std::iter::repeat_n(0xAA, extra));
        blob
    }

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
    fn i2p_base64_refuses_what_it_cannot_have_encoded() {
        // One dangling symbol carries six bits and encodes no byte at all;
        // returning an empty vec for it (as a lax decoder does) makes every
        // such string decode to "nothing" rather than to an error.
        assert!(i2p_base64_decode("A").is_none());
        assert!(i2p_base64_decode("A===").is_none());
        assert_eq!(i2p_base64_decode("").unwrap(), Vec::<u8>::new());

        // Spare bits of the final group are zero in any real encoding, so two
        // spellings must never decode to one value.
        let one = i2p_base64_encode(&[0xFF]); // "~w==" style: 2 symbols
        assert_eq!(i2p_base64_decode(&one).unwrap(), vec![0xFF]);
        let mut lax = one.clone().into_bytes();
        // Bump the last symbol: same leading byte, spare bits now set.
        let last = lax
            .iter()
            .rposition(|&b| b != b'=')
            .expect("a symbol to bump");
        let idx = I2P_B64_ALPHABET
            .iter()
            .position(|&a| a == lax[last])
            .expect("in alphabet");
        lax[last] = I2P_B64_ALPHABET[idx + 1];
        let lax = String::from_utf8(lax).expect("ascii");
        assert_ne!(lax, one);
        assert!(
            i2p_base64_decode(&lax).is_none(),
            "{lax} decoded despite spare bits the encoder never sets"
        );

        // Valid lengths still work: 2, 3 and 4 symbols per group.
        for len in [1usize, 2, 3, 4, 5, 48, 387] {
            let data = vec![0x5A; len];
            let text = i2p_base64_encode(&data);
            assert_eq!(i2p_base64_decode(&text).unwrap(), data, "len {len}");
        }
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

    /// The bug that made every live run fail, pinned with the shape SAM
    /// actually returns.
    ///
    /// A `SAMv3` `SESSION STATUS DESTINATION=` field is the session's private
    /// key blob: the public destination, then the private crypto and signing
    /// keys. Captured from i2pd on 2026-07-27 it was 679 bytes — 391 of
    /// destination (a KEY certificate with a 4-byte payload) and 288 of key
    /// material that must never leave the process.
    ///
    /// Two things must hold, and neither did:
    ///   - our identity is the hash of the destination, not of the blob;
    ///   - what we publish is the destination, not the blob.
    #[test]
    fn a_sam_destination_field_is_a_private_key_blob_and_is_cut_to_its_destination() {
        let cert = [0x00, 0x07, 0x00, 0x00];
        let blob = dest_blob(0x42, &cert, 288);
        assert_eq!(blob.len(), 679, "the shape i2pd returned");

        let dest_len = destination_len(&blob).expect("a destination is in there");
        assert_eq!(
            dest_len, 391,
            "384 key bytes + 3 cert header + 4 cert payload"
        );

        let b64 = i2p_base64_encode(&blob);
        let cut = destination_bytes(&b64).expect("parses");
        assert_eq!(cut.len(), dest_len);
        assert_eq!(cut, blob[..dest_len], "the private half is dropped");

        // The identity is the destination's hash. Hashing the whole blob —
        // which is what clove did — yields a name nothing can resolve, and a
        // router asked to look it up can only answer "leaseSet not found".
        assert_eq!(
            DestHash::from_b64_destination(&b64).unwrap(),
            DestHash(Sha256::digest(&blob[..dest_len]).into())
        );
        assert_ne!(
            DestHash::from_b64_destination(&b64).unwrap(),
            DestHash(Sha256::digest(&blob).into()),
            "hashing the private keys too is the bug this test exists for"
        );
    }

    /// Certificates vary in length, so the destination does too; and anything
    /// that cannot state its own length is not a destination.
    #[test]
    fn destination_length_comes_from_the_certificate_not_a_constant() {
        assert_eq!(destination_len(&dest_blob(1, &[], 0)), Some(387));
        assert_eq!(destination_len(&dest_blob(1, &[0; 4], 0)), Some(391));
        assert_eq!(destination_len(&dest_blob(1, &[0; 64], 999)), Some(451));

        // Too short to hold a certificate header at all.
        assert_eq!(destination_len(&[0u8; 386]), None);
        assert_eq!(destination_len(&[]), None);
        // A certificate claiming more payload than the blob carries.
        let mut truncated = dest_blob(1, &[0; 64], 0);
        truncated.truncate(400);
        assert_eq!(destination_len(&truncated), None);
        // And the short forms that used to be accepted as whole destinations.
        assert!(DestHash::from_b64_destination(&i2p_base64_encode(&[0x42; 48])).is_none());
    }

    #[test]
    fn dest_hash_from_b64_matches_sha256() {
        // A destination is the key material plus its certificate; its
        // DestHash is the SHA-256 of exactly those bytes.
        let dest_bytes = dest_blob(0x42, &[0x00, 0x07, 0x00, 0x00], 0);
        let b64 = i2p_base64_encode(&dest_bytes);
        let expected = DestHash(Sha256::digest(&dest_bytes).into());
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

#[cfg(test)]
mod hostile_tests {
    //! Adversarial coverage for the address parsers.
    //!
    //! These are attacker-reachable and were outside every existing sweep:
    //! `clove-core/tests/hostile.rs` only reaches parsers in `clove-core`,
    //! and these live here. A b32 label arrives from a magnet link, a PEX
    //! message or `clove peer`; a full base64 destination arrives from a
    //! non-compact tracker response and from the router on every inbound
    //! stream. All of it is bytes someone else chose.
    //!
    //! The contract is the same one the rest of the project holds parsers
    //! to: parse or return `None` — never panic, never loop, never accept
    //! something the encoder would not have produced.

    use super::tests::dest_blob;
    use super::*;

    /// A deterministic xorshift, so a failure reproduces from its seed
    /// rather than being a once-a-month CI flake.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % n as u64).unwrap_or(0)
        }
    }

    #[test]
    fn b32_labels_of_the_wrong_length_are_refused() {
        let valid = DestHash([0x42; 32]).to_b32();
        let label = valid.strip_suffix(".b32.i2p").expect("suffix");
        assert_eq!(label.len(), 52, "32 bytes is 52 base32 characters");
        assert!(DestHash::from_b32(&valid).is_some());
        assert!(DestHash::from_b32(label).is_some());

        // One character short or long decodes to the wrong byte count, which
        // must be refused rather than silently truncated or zero-padded: a
        // near-miss address that resolved to *some* peer would be worse than
        // no address at all.
        assert!(DestHash::from_b32(&label[..51]).is_none());
        assert!(DestHash::from_b32(&format!("{label}a")).is_none());
        assert!(DestHash::from_b32("").is_none());
        assert!(DestHash::from_b32(".b32.i2p").is_none());
    }

    #[test]
    fn b32_rejects_everything_outside_its_alphabet() {
        let label: String = DestHash([7; 32])
            .to_b32()
            .strip_suffix(".b32.i2p")
            .expect("suffix")
            .to_owned();
        // RFC 4648 base32 as I2P uses it is lowercase a-z and 2-7. Uppercase,
        // digits 0/1/8/9, padding and punctuation are all outside it.
        for bad in ['A', 'Z', '0', '1', '8', '9', '=', '.', '/', ' ', '\n'] {
            let mut mutated = label.clone();
            mutated.replace_range(10..11, &bad.to_string());
            assert!(
                DestHash::from_b32(&mutated).is_none(),
                "{bad:?} was accepted in a b32 label"
            );
        }
        // Non-ASCII must not panic on a byte-oriented decoder.
        assert!(DestHash::from_b32("é".repeat(52).as_str()).is_none());
    }

    #[test]
    fn the_b32_suffix_is_stripped_once_and_only_at_the_end() {
        let hash = DestHash([0x99; 32]);
        let label = hash.to_b32();
        assert_eq!(DestHash::from_b32(&label), Some(hash));
        // Surrounding whitespace is tolerated; a doubled suffix is not a
        // valid address and must not be peeled twice.
        assert_eq!(DestHash::from_b32(&format!("  {label}  ")), Some(hash));
        assert!(DestHash::from_b32(&format!("{label}.b32.i2p")).is_none());
    }

    #[test]
    fn base64_destinations_reject_the_standard_alphabet() {
        // I2P base64 swaps '+' -> '-' and '/' -> '~'. A destination carrying
        // the standard characters is not ours, and accepting it would hash a
        // different byte string than the router did — a peer identity that
        // silently disagrees with everyone else's.
        let dest = i2p_base64_encode(&dest_blob(0xFB, &[0x00, 0x07, 0x00, 0x00], 0));
        assert!(DestHash::from_b64_destination(&dest).is_some());
        assert!(i2p_base64_decode("+").is_none());
        assert!(i2p_base64_decode("/").is_none());
        assert!(i2p_base64_decode(&dest.replace('-', "+")).is_none() || !dest.contains('-'));
    }

    #[test]
    fn an_empty_or_unparseable_destination_is_none_not_a_hash_of_nothing() {
        // SHA-256 of the empty string is a perfectly good hash, and would be
        // a catastrophic peer identity: every malformed destination would
        // collide into one peer.
        assert!(DestHash::from_b64_destination("").is_none());
        assert!(DestHash::from_b64_destination("   ").is_none());
        assert!(DestHash::from_b64_destination("====").is_none());
        assert!(DestHash::from_b64_destination(".i2p").is_none());
        assert!(DestHash::from_b64_destination("\n\n").is_none());
    }

    #[test]
    fn destination_parsing_stops_at_the_first_foreign_character() {
        // SAM may append a base32 label or params after the destination.
        // Everything from the first character outside the alphabet is
        // ignored, and the prefix alone decides the hash.
        let dest = i2p_base64_encode(&dest_blob(0x11, &[0x00, 0x07, 0x00, 0x00], 0));
        let expected = DestHash::from_b64_destination(&dest).expect("plain destination");
        for suffix in [
            " FROM_PORT=0 TO_PORT=0",
            ".b32.i2p",
            "\r\n",
            " garbage",
            "\u{0}trailing",
        ] {
            assert_eq!(
                DestHash::from_b64_destination(&format!("{dest}{suffix}")),
                Some(expected),
                "suffix {suffix:?} changed the identity"
            );
        }
    }

    #[test]
    fn mutating_a_real_address_never_panics_and_never_lies() {
        // A sweep in the shape of clove-core's hostile.rs, over the two
        // address parsers no other sweep reaches.
        let mut rng = Rng(0x5EED_1234_ABCD_0001);
        let hash = DestHash([0x5A; 32]);
        let b32 = hash.to_b32();
        let b64 = i2p_base64_encode(&[0x33; 387]);

        for round in 0..20_000u32 {
            let seed = &if round % 2 == 0 {
                b32.clone()
            } else {
                b64.clone()
            };
            let mut bytes = seed.clone().into_bytes();
            if bytes.is_empty() {
                continue;
            }
            match rng.below(3) {
                0 => {
                    let at = rng.below(bytes.len());
                    bytes[at] = u8::try_from(rng.below(256)).unwrap_or(0);
                }
                1 => {
                    let at = rng.below(bytes.len());
                    bytes.truncate(at);
                }
                _ => {
                    let at = rng.below(bytes.len());
                    bytes.insert(at, u8::try_from(rng.below(256)).unwrap_or(0));
                }
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            // The claim is only that these terminate and do not panic...
            let from32 = DestHash::from_b32(text);
            let from64 = DestHash::from_b64_destination(text);
            // ...and that anything they *do* accept round-trips: a b32 that
            // parses must re-encode to the same label, or the decoder is
            // accepting inputs its own encoder cannot produce.
            if let Some(h) = from32 {
                let canonical = h.to_b32();
                let label = text.trim().strip_suffix(".b32.i2p").unwrap_or(text.trim());
                assert_eq!(
                    canonical.strip_suffix(".b32.i2p").expect("suffix"),
                    label,
                    "round {round}: {text:?} parsed but does not re-encode"
                );
            }
            if let Some(h) = from64 {
                assert_ne!(h.0, [0u8; 32], "round {round}: all-zero hash from {text:?}");
            }
        }
    }
}
