//! Fuzz announce-response parsing, and check that a hostile interval cannot
//! push us to announce inside the local floor.
#![no_main]
use libfuzzer_sys::fuzz_target;
use clove_core::tracker::{parse_response, AnnounceState, MIN_ANNOUNCE_INTERVAL};

fuzz_target!(|data: &[u8]| {
    if let Ok(response) = parse_response(data) {
        let mut state = AnnounceState::new();
        let now = 1_000_000u64;
        state.on_success(now, response.interval);
        assert!(!state.due(now + MIN_ANNOUNCE_INTERVAL.as_secs() - 1));
    }
});
