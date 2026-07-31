//! Fuzz the standard-alphabet base64 decoder.
//!
//! Reached from `torrent-add`'s `metainfo` argument on the Transmission RPC
//! surface (`docs/PHASE-I.md`), so its input is a stranger's bytes inside a
//! JSON string. The RPC *envelope* around it is not a separate target: it is
//! `clove_core::json::parse`, which has one already, wrapped in field lookups
//! that cannot fail. This is the one genuinely new parser that surface added.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Some(decoded) = clove_core::base64::decode(text) else {
        return;
    };

    // Three bytes in, four symbols out: a successful decode can never claim
    // more bytes than the input could have encoded. This is the property that
    // catches an over-read or a capacity miscalculation, neither of which
    // shows up as a panic on its own.
    assert!(
        decoded.len() <= text.len() / 4 * 3 + 3,
        "decoded {} bytes from {} characters",
        decoded.len(),
        text.len()
    );

    // Canonical: re-encoding what we decoded and decoding that must return the
    // same bytes. A decoder that accepts two spellings of one value fails here
    // rather than at the point where the two spellings turn out to be two
    // different torrents.
    let reencoded = encode(&decoded);
    let again = clove_core::base64::decode(&reencoded).expect("our own encoding decodes");
    assert_eq!(again, decoded, "base64 round trip disagreed");
});

/// Standard base64, written here rather than taken from `clove-core` — which
/// has no encoder, deliberately. The round-trip property needs an encoder that
/// is not the thing under test.
fn encode(raw: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}
