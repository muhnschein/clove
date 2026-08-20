//! Making somebody else's text safe to put in front of a person.
//!
//! Almost every string clove displays came from a stranger, and a terminal is
//! an interpreter. Scrubbing happens where bytes become some interpreter's
//! input, not at the source: the stored name is the torrent's actual name, and
//! `json::write_string` already escapes it for `--json` consumers. This module
//! exists so every one of those points scrubs the same way.

/// Replace every character that a terminal would act on rather than draw.
///
/// Two families go, both to `.`: the `Cc` category (C0, `DEL`, C1 — escape
/// sequences, carriage returns, newlines), and the bidi overrides and
/// isolates, which draw nothing and reorder the text *around* them, so
/// `…rat.exe` can render as `…exe.tar`. Everything else passes through;
/// deciding what alphabets a torrent may be named in is not our business.
///
/// Replaced rather than removed: a name that silently loses characters is one
/// an operator cannot match against what they expected.
#[must_use]
pub fn scrub(text: &str) -> String {
    text.chars().map(scrub_char).collect()
}

/// [`scrub`], and no more than `max_chars` characters of it — with a `…` when
/// something was cut.
///
/// The bound matters for anything a *remote* party sizes: a tracker does not
/// get to decide how much of an operator's log it occupies.
#[must_use]
pub fn scrub_bounded(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).map(scrub_char).collect();
    if text.chars().nth(max_chars).is_some() {
        out.push('…');
    }
    out
}

/// One character's worth of [`scrub`].
fn scrub_char(c: char) -> char {
    match c {
        c if c.is_control() => '.',
        // LRM/RLM, the LRE/RLE/PDF/LRO/RLO run, and the isolates.
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '.',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_go() {
        assert_eq!(scrub("a\tb\nc\r\n"), "a.b.c..");
        assert_eq!(scrub("\u{1b}[2J"), ".[2J");
        assert_eq!(scrub("\u{1b}]0;title\u{7}"), ".]0;title.");
        // C1 and DEL are controls too, and are easy to forget.
        for c in ['\u{0}', '\u{7f}', '\u{80}', '\u{9b}'] {
            assert_eq!(scrub(&c.to_string()), ".", "{c:?} survived");
        }
    }

    #[test]
    fn bidi_overrides_go() {
        // The classic filename spoof: RLO makes this render as "safe.exe.gnp".
        assert_eq!(scrub("safe\u{202e}gnp.exe"), "safe.gnp.exe");
        for c in [
            '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}',
            '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            assert_eq!(scrub(&c.to_string()), ".", "{c:?} survived");
        }
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for ok in [
            "plain-name_1.0.iso",
            "café",
            "日本語のファイル",
            "Ünïcödé",
            "a b c",
            "",
        ] {
            assert_eq!(scrub(ok), ok, "{ok:?} was altered");
        }
    }

    #[test]
    fn bounded_truncates_and_says_so() {
        assert_eq!(scrub_bounded("short", 10), "short");
        // Exactly at the bound is not truncated, so no ellipsis.
        assert_eq!(scrub_bounded("abcde", 5), "abcde");
        assert_eq!(scrub_bounded("abcdef", 5), "abcde…");
        // Characters, not bytes: cutting mid-character would panic on a slice.
        let wide = "é".repeat(10);
        assert_eq!(scrub_bounded(&wide, 10), wide);
        assert_eq!(scrub_bounded(&wide, 4), "ééét…".replace('t', "é"));
        // Scrubbing still applies to what survives the cut.
        assert_eq!(scrub_bounded("a\u{1b}bcdef", 3), "a.b…");
        // A zero bound is degenerate but must not panic.
        assert_eq!(scrub_bounded("abc", 0), "…");
        assert_eq!(scrub_bounded("", 0), "");
    }
}
