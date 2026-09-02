//! Fuzz the scrubber every foreign string crosses on its way to a terminal.
//!
//! The other targets ask "does the parser survive this input"; this one asks
//! "does the output keep the promise", because a scrubber that lets one
//! character through is not a crash, it is a forged log line. The oracle here
//! is deliberately a second, coarser statement of the rule than the table in
//! `text.rs` — the classes a terminal or a reader acts on — so a range dropped
//! from that table fails here rather than passing by agreeing with itself.
#![no_main]
use libfuzzer_sys::fuzz_target;

/// A character that draws nothing and steers what surrounds it: the bidi
/// controls, the zero-width family, the joiners, the line and paragraph
/// separators, and the byte-order mark.
fn steers_the_reader(c: char) -> bool {
    matches!(
        c,
        '\u{61c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let out = clove_core::text::scrub(&text);

    // Replaced, never removed: an operator has to be able to match a name
    // against what they see, character for character.
    assert_eq!(
        out.chars().count(),
        text.chars().count(),
        "scrub changed the length"
    );
    for c in out.chars() {
        assert!(!c.is_control(), "a control character survived: {c:?}");
        assert!(
            !steers_the_reader(c),
            "a reader-steering character survived: {c:?}"
        );
    }
    // Scrubbing is idempotent, so a string scrubbed at two boundaries reads
    // the same as one scrubbed at one.
    assert_eq!(clove_core::text::scrub(&out), out, "scrub is not idempotent");

    // The bounded form keeps the same promise and never exceeds its cap by
    // more than the ellipsis that says something was cut.
    let bounded = clove_core::text::scrub_bounded(&text, 16);
    assert!(
        bounded.chars().count() <= 17,
        "scrub_bounded exceeded its cap"
    );
    for c in bounded.chars() {
        assert!(!c.is_control() && !steers_the_reader(c), "{c:?} survived");
    }
});
