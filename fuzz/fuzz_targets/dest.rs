//! Fuzz the SAM address codecs: the base64 destination blob the router
//! prepends to every forwarded connection, and the base32 label we hand back
//! to dial a peer.
//!
//! This is the one parser in clove whose mis-reading has already cost
//! something. A destination and a *private key blob* are both one long base64
//! run, and the difference is a length-prefixed certificate header 384 bytes
//! in; before `destination_len` existed, every announce put our private keys
//! in the `ip` parameter (`crates/i2pnet/src/addr.rs`, 2026-07-27). The
//! arithmetic that tells the two apart reads an attacker-supplied 16-bit
//! length and slices with it, which is exactly the shape a fuzzer is for.
#![no_main]
use i2pnet::DestHash;
use i2pnet::addr::{
    base32_decode, base32_encode, destination_bytes, destination_len, i2p_base64_decode,
    i2p_base64_encode,
};
use libfuzzer_sys::fuzz_target;

/// A destination is 256 bytes of public key, 128 of signing key, then a
/// three-byte certificate header. Nothing shorter can be one.
const MIN_DESTINATION: usize = 384 + 3;

fuzz_target!(|data: &[u8]| {
    // The encoders are total, and this is the direction we control: whatever
    // bytes we hold, the text we produce has to decode back to exactly them.
    // A codec that loses a byte here corrupts a peer identity rather than
    // failing, which is the failure that does not announce itself.
    let b32 = base32_encode(data);
    assert_eq!(
        base32_decode(&b32).as_deref(),
        Some(data),
        "base32 round trip"
    );
    let b64 = i2p_base64_encode(data);
    assert_eq!(
        i2p_base64_decode(&b64).as_deref(),
        Some(data),
        "i2p base64 round trip"
    );

    // Any 32 bytes are a DestHash somebody could have sent us, and its b32
    // address must survive being read back. This is the pair the dialler
    // depends on: we only ever hold the hash, so `to_b32` is how a peer gets
    // dialled at all.
    if data.len() >= 32 {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data[..32]);
        let dest = DestHash(hash);
        assert_eq!(
            DestHash::from_b32(&dest.to_b32()),
            Some(dest),
            "b32 address round trip"
        );
    }

    // And the decoders take whatever the router said, which is the direction
    // we do not control.
    let text = String::from_utf8_lossy(data);
    let _ = base32_decode(&text);
    let _ = i2p_base64_decode(&text);
    let _ = DestHash::from_b32(&text);

    // The certificate header: a 16-bit payload length read out of the blob
    // and added to a fixed offset. Whatever it says, the answer has to be a
    // length inside the blob we were handed.
    if let Some(len) = destination_len(data) {
        assert!(len <= data.len(), "destination_len ran past the blob");
        assert!(len >= MIN_DESTINATION, "a destination cannot be that short");
    }

    if let Some(bytes) = destination_bytes(&text) {
        // Truncating a blob to its destination must leave something that is
        // still a whole destination — otherwise the bytes we hash are a
        // prefix of the answer rather than the answer.
        assert_eq!(
            destination_len(&bytes),
            Some(bytes.len()),
            "the truncated blob is not a destination"
        );
        assert!(bytes.len() >= MIN_DESTINATION);
        // The hash and the bytes come from one decode, so they agree about
        // whether the input was a destination at all. They did not always:
        // that disagreement is what shipped the private key blob.
        assert!(
            DestHash::from_b64_destination(&text).is_some(),
            "destination_bytes accepted what from_b64_destination refused"
        );
    } else {
        assert!(
            DestHash::from_b64_destination(&text).is_none(),
            "from_b64_destination accepted what destination_bytes refused"
        );
    }
});
