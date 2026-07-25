//! Fuzz the bencode codec: the parser under every torrent, resume file and
//! tracker reply. Asserts the round-trip property as well as no-panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = clove_core::bencode::decode(data) {
        let reencoded = clove_core::bencode::encode(&value);
        let again = clove_core::bencode::decode(&reencoded)
            .expect("re-encoded bencode must decode");
        assert_eq!(again, value, "bencode round trip disagreed");
    }
    let _ = clove_core::bencode::decode_prefix(data);
});
