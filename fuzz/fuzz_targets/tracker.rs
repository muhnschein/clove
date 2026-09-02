//! Fuzz announce-response parsing, and check that a hostile interval cannot
//! push us to announce inside the local floor.
#![no_main]
use libfuzzer_sys::fuzz_target;
use clove_core::tracker::{
    parse_response, AnnounceState, MAX_ANNOUNCE_INTERVAL, MIN_ANNOUNCE_INTERVAL,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(response) = parse_response(data) {
        let mut state = AnnounceState::new();
        let now = 1_000_000u64;
        // Drive it the way the daemon does: ask what the announce should
        // carry, then report that same event as sent.
        let sent = state.next_event(false);
        state.on_success(now, response.interval, sent);
        assert!(!state.due(now + MIN_ANNOUNCE_INTERVAL.as_secs() - 1));
        // Nor can an interval mean "never again".
        assert!(state.due(now + MAX_ANNOUNCE_INTERVAL.as_secs()));
        let mut with_min = AnnounceState::new();
        with_min.on_success_with_min(now, response.interval, response.min_interval, sent);
        assert!(!with_min.due(now + MIN_ANNOUNCE_INTERVAL.as_secs() - 1));
        assert!(with_min.due(now + MAX_ANNOUNCE_INTERVAL.as_secs()));
    }
});
