//! Fuzz the terminal key decoder behind `clove top`.
//!
//! The input here is a keyboard rather than a hostile network, so the bug
//! being hunted is not an attacker's: it is a decoder that stalls, consumes
//! nothing, over-reads or panics on a truncated or unfamiliar escape sequence
//! — and every terminal emits some. `docs/DECISIONS.md` S2 makes the target a
//! condition of hand-rolling the decoder at all.
#![no_main]
use libfuzzer_sys::fuzz_target;

use clove::term::{MAX_SEQUENCE, decode};

fuzz_target!(|data: &[u8]| {
    let mut at = 0;
    while at < data.len() {
        let Some((_key, used)) = decode(&data[at..]) else {
            // Incomplete: the decoder is asking for bytes that will never
            // come, which is the correct answer at the end of input.
            break;
        };
        // Progress, or the caller's loop never terminates.
        assert!(used > 0, "decode consumed nothing at offset {at}");
        assert!(
            used <= data.len() - at,
            "decode claimed {used} bytes with only {} left",
            data.len() - at
        );
        // No sequence is followed further than the documented bound, plus the
        // two-byte `ESC [` introducer and the final byte.
        assert!(
            used <= MAX_SEQUENCE + 3,
            "decode consumed {used} bytes, past the bound"
        );
        at += used;
    }
});
