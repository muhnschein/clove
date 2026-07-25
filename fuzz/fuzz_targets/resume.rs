//! Fuzz resume decoding: a corrupt or hostile state file must be refused,
//! never trusted into inconsistent piece accounting.
#![no_main]
use libfuzzer_sys::fuzz_target;
use clove_core::resume::{bitfield_len, Resume};

fuzz_target!(|data: &[u8]| {
    if let Ok(state) = Resume::decode(data) {
        assert_eq!(state.have.len(), bitfield_len(state.num_pieces));
        assert_eq!(state.verified.len(), bitfield_len(state.num_pieces));
        assert!(state.priorities.iter().all(|&p| p <= 2));
        let again = Resume::decode(&state.encode()).expect("re-encoded resume decodes");
        assert_eq!(again, state, "resume round trip disagreed");
    }
});
