//! Fuzz the JSON parser used by the CLI to read daemon replies.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(value) = clove_core::json::parse(text) {
        let again = clove_core::json::parse(&value.encode()).expect("re-encoded JSON parses");
        assert_eq!(again, value, "JSON round trip disagreed");
    }
});
