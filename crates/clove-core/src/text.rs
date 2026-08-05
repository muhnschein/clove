//! Making somebody else's text safe to put in front of a person.
//!
//! Almost every string clove displays came from a stranger: a `.torrent`'s
//! `info.name` and file paths, a tracker's `failure reason`, an announce URL, a
//! SAM bridge's account of why a session died. None of it is trusted, and all of
//! it ends up somewhere a person reads — the daemon's stderr, which is to say a
//! journal, a log shipper and eventually a bug report; or `clove list`, which is
//! to say a terminal.
//!
//! A terminal is an interpreter. `"\x1b[2J"` clears the reader's screen,
//! `"\x1b]0;…\x07"` retitles their window, and a bare newline forges a log line —
//! an operator reading `cloved: …` has no way to tell which of those lines clove
//! wrote. None of that is a parser bug: `metainfo::check_component` refuses
//! separators, `.`, `..` and NUL because those are what a *path* must not
//! contain, and it has no opinion about `ESC` because `ESC` is not a path
//! problem. It is a display problem, and this is where display problems are
//! fixed.
//!
//! **Scrub at the boundary, not at the source.** The stored name is the
//! torrent's actual name and the API's JSON has to keep it — `json::write_string`
//! escapes everything below `0x20`, so a `--json` consumer is inert and correct.
//! Sanitising in the daemon's model would misreport what the torrent is called.
//! So the substitution happens at each point where bytes become some
//! interpreter's input, and this module exists so that all of those points do it
//! the same way. Three separate implementations of this had already drifted:
//! two replacement characters between them, one of them missing the
//! bidirectional overrides entirely, and two output paths in the CLI that
//! reached the terminal without passing through any of them.

/// Replace every character that a terminal would act on rather than draw.
///
/// Two families go, both to `.`:
///
/// - the `Cc` category — C0, `DEL` and C1 — which is where the escape sequences,
///   the carriage returns and the newlines live; and
/// - the bidirectional overrides and isolates, which draw nothing themselves and
///   reorder the text *around* them, so `…rat.exe` can be made to render as
///   `…exe.tar` in the very listing an operator is reading to decide what to
///   trust.
///
/// Everything else passes through, including the non-ASCII that makes column
/// alignment approximate: the alternative is deciding what alphabets a torrent
/// may be named in, which is not this function's business.
///
/// Replaced rather than removed, so the text still reads as having had something
/// there. A name that silently loses characters is a name an operator cannot
/// match against what they were expecting.
#[must_use]
pub fn scrub(text: &str) -> String {
    text.chars().map(scrub_char).collect()
}

/// [`scrub`], and no more than `max_chars` characters of it — with a `…` when
/// something was cut.
///
/// The bound is the half that matters for anything a *remote* party sizes. A
/// tracker does not get to decide how much of an operator's log it occupies, and
/// a 10 000-character `failure reason` is not a diagnosis.
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
        // Exactly at the bound is not truncated, so no ellipsis is added.
        assert_eq!(scrub_bounded("abcde", 5), "abcde");
        assert_eq!(scrub_bounded("abcdef", 5), "abcde…");
        // The bound counts characters, not bytes: a multi-byte string must not
        // be cut mid-character (which would panic on a byte slice) nor counted
        // as longer than it reads.
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
