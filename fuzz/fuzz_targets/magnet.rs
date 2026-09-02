//! Fuzz magnet parsing, asserting the clearnet tracker filter holds.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(link) = clove_core::magnet::Magnet::parse(text) {
        assert!(link.trackers.len() <= clove_core::metainfo::MAX_TRACKERS);
        for url in &link.trackers {
            assert!(url.to_ascii_lowercase().contains(".i2p"));
        }
    }
});
