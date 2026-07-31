//! Raw mode, window size, and keypress decoding for `clove top`
//! (`docs/DECISIONS.md` S2, `docs/PHASE-H.md` §9).
//!
//! The whole terminal layer, and it is deliberately small. A full-screen view
//! needs five things: raw mode, the window size, decoded keypresses, cursor
//! addressing and a repaint discipline. The last two are escape sequences the
//! caller writes; the first two are the only syscalls involved, and they come
//! from `rustix` — already a dependency, its `termios`/`stdio` features
//! resolving to crates that are already in the tree. The middle one is this
//! file's state machine.
//!
//! That split is the reason a TUI is affordable here and a TUI *framework* is
//! not: `ratatui` measures at 91 crates stripped and 181 with defaults against
//! clove's 48, and the parts of it clove would use are the parts it can write.
//!
//! # What raw mode costs, stated plainly
//!
//! [`RawMode`] restores the terminal when dropped, which covers a normal exit
//! and a panic that unwinds. It does **not** cover a signal that kills the
//! process outright: `SIGTERM`, `SIGHUP`, `SIGKILL`. Installing a handler for
//! the first two needs either `unsafe` or a crate, and this workspace takes
//! neither, so `kill`ing `clove top` leaves a terminal wanting `stty sane`.
//! That is documented in `clove(1)` rather than worked around.
//!
//! `SIGINT` is not in that list, and that is the one way raw mode makes life
//! easier: with `ISIG` cleared, Ctrl-C arrives as the byte `0x03` on stdin and
//! is handled in band like any other key, so the common way to quit is also
//! the clean one.

use std::io::{self, Read};

use rustix::termios::{self, OptionalActions, Termios};

/// Raw mode for the duration of the value's life.
///
/// Dropping it restores the terminal exactly as it was found — the saved
/// [`Termios`] is the one read at construction, not a reconstruction of what
/// it probably was.
pub struct RawMode {
    saved: Termios,
}

impl RawMode {
    /// Put stdin into raw mode.
    ///
    /// # Errors
    ///
    /// Whatever `tcgetattr`/`tcsetattr` say — most usefully `ENOTTY`, which is
    /// what a caller gets for piping input into a full-screen view and is
    /// worth refusing rather than half-doing.
    pub fn enter() -> io::Result<RawMode> {
        let stdin = rustix::stdio::stdin();
        let saved = termios::tcgetattr(stdin)?;
        let mut raw = saved.clone();
        raw.make_raw();
        // One byte is enough to return, and no inter-byte timer. A terminal
        // hands over a whole escape sequence in a single write, so the decoder
        // sees `\x1b[A` in one read rather than having to time out on the ESC
        // — which is what makes a timer unnecessary here.
        raw.special_codes[termios::SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[termios::SpecialCodeIndex::VTIME] = 0;
        termios::tcsetattr(stdin, OptionalActions::Now, &raw)?;
        Ok(RawMode { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Best effort by necessity: a Drop cannot report, and a terminal that
        // has gone away is not a failure worth pretending we can act on.
        let _ = termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &self.saved);
    }
}

/// The terminal's size in (columns, rows).
///
/// Falls back to 80×24 when the size cannot be had — a pipe, or a terminal
/// that does not answer. Eighty by twenty-four is the wrong answer far less
/// often than zero would be, and a view that renders nothing is worse than one
/// that renders narrow.
#[must_use]
pub fn window_size() -> (u16, u16) {
    termios::tcgetwinsize(rustix::stdio::stdout()).map_or((80, 24), |size| {
        let cols = if size.ws_col == 0 { 80 } else { size.ws_col };
        let rows = if size.ws_row == 0 { 24 } else { size.ws_row };
        (cols, rows)
    })
}

/// A decoded keypress.
///
/// Only the keys `clove top` acts on. Anything else decodes to [`Key::Other`]
/// and is ignored by the caller — deliberately, so an unrecognised escape
/// sequence is a no-op rather than a stray character interpreted as a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A printable character, or a control character that is not one of the
    /// named keys below.
    Char(char),
    /// Cursor up, or `k`.
    Up,
    /// Cursor down, or `j`.
    Down,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Home, or `g`.
    Home,
    /// End, or `G`.
    End,
    /// Return or Enter.
    Enter,
    /// A lone Escape.
    Escape,
    /// Ctrl-C, which in raw mode is a byte rather than a signal.
    Interrupt,
    /// Something recognised as a complete sequence but not acted on.
    Other,
}

/// Decode the first key in `buf`, and say how many bytes it used.
///
/// Returns `None` when `buf` holds the start of a sequence but not all of it,
/// which tells the caller to read more rather than to guess.
///
/// A pure function over bytes, which is what makes it testable and fuzzable.
/// The input is a keyboard rather than a hostile network, so the threat here
/// is a decoder that hangs, over-reads or panics on a truncated or unfamiliar
/// sequence — every terminal emits some — not an attacker. Bounded by
/// construction: no allocation, and no sequence is followed further than
/// [`MAX_SEQUENCE`] bytes before being abandoned as unrecognised.
#[must_use]
pub fn decode(buf: &[u8]) -> Option<(Key, usize)> {
    let (&first, rest) = buf.split_first()?;
    match first {
        0x03 => Some((Key::Interrupt, 1)),
        b'\r' | b'\n' => Some((Key::Enter, 1)),
        0x1b => decode_escape(rest),
        // A UTF-8 character may span several bytes, and a partial one means
        // read more rather than render a replacement character as a command.
        0x80.. => decode_utf8(buf),
        b => Some((Key::Char(char::from(b)), 1)),
    }
}

/// Longest escape sequence followed before giving up on it.
///
/// Real ones are far shorter; this is the bound that stops a stream of `[`
/// bytes after an ESC from being scanned forever.
pub const MAX_SEQUENCE: usize = 16;

/// Decode what follows an ESC. `rest` is everything after it.
fn decode_escape(rest: &[u8]) -> Option<(Key, usize)> {
    let Some((&intro, tail)) = rest.split_first() else {
        // ESC and nothing yet. A lone Escape and the start of a sequence are
        // the same bytes, and the only way to tell them apart is to wait —
        // which is what returning `None` asks the caller to do. `Keys` turns a
        // read that adds nothing into an Escape.
        return None;
    };
    match intro {
        // CSI (`ESC [`) and SS3 (`ESC O`), which is what a numeric keypad and
        // some terminals send for the arrows.
        b'[' | b'O' => decode_csi(tail).map(|(key, used)| (key, used + 2)),
        // Alt-<something>, and anything else after an ESC: recognised as a
        // complete two-byte sequence and ignored, rather than leaving the
        // second byte to be read as a command of its own.
        _ => Some((Key::Other, 2)),
    }
}

/// Decode the body of a CSI/SS3 sequence — everything after `ESC [` or `ESC O`.
fn decode_csi(tail: &[u8]) -> Option<(Key, usize)> {
    for (index, &byte) in tail.iter().enumerate() {
        if index >= MAX_SEQUENCE {
            // Not a sequence we will ever recognise. Consuming it whole is the
            // point: leaving it in the buffer would decode the same rubbish
            // forever.
            return Some((Key::Other, index));
        }
        // Parameter and intermediate bytes; the final byte is what names the
        // key, and it is the first outside these ranges.
        if matches!(byte, b'0'..=b'9' | b';' | b'?' | b' '..=b'/') {
            continue;
        }
        let key = match byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'H' => Key::Home,
            b'F' => Key::End,
            // `ESC [ 5 ~` and `ESC [ 6 ~`, with the digit among the parameter
            // bytes already skipped above.
            b'~' => match tail.first() {
                Some(b'1' | b'7') => Key::Home,
                Some(b'4' | b'8') => Key::End,
                Some(b'5') => Key::PageUp,
                Some(b'6') => Key::PageDown,
                _ => Key::Other,
            },
            _ => Key::Other,
        };
        return Some((key, index + 1));
    }
    // Ran out of bytes inside a sequence: incomplete, not unrecognised.
    None
}

/// Decode one UTF-8 character, or `None` if it is not all here yet.
fn decode_utf8(buf: &[u8]) -> Option<(Key, usize)> {
    let len = match buf[0] {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        // A continuation byte with no lead, or an invalid lead. Consumed so it
        // cannot jam the buffer.
        _ => return Some((Key::Other, 1)),
    };
    if buf.len() < len {
        return None;
    }
    match std::str::from_utf8(&buf[..len]) {
        Ok(text) => text
            .chars()
            .next()
            .map_or(Some((Key::Other, len)), |c| Some((Key::Char(c), len))),
        Err(_) => Some((Key::Other, len)),
    }
}

/// Keys read from a stream, buffered so a whole escape sequence is decoded
/// from the single read that delivered it.
pub struct Keys<R> {
    input: R,
    buf: Vec<u8>,
}

impl<R: Read> Keys<R> {
    /// Read keys from `input`.
    pub fn new(input: R) -> Keys<R> {
        Keys {
            input,
            buf: Vec::with_capacity(64),
        }
    }

    /// The next keypress, blocking until there is one.
    ///
    /// `None` at end of input.
    ///
    /// # Errors
    ///
    /// Any read error from the underlying stream.
    pub fn next_key(&mut self) -> io::Result<Option<Key>> {
        loop {
            if let Some((key, used)) = decode(&self.buf) {
                self.buf.drain(..used);
                return Ok(Some(key));
            }
            let mut chunk = [0u8; 64];
            let n = self.input.read(&mut chunk)?;
            if n == 0 {
                // End of input with something undecodable left over. A lone
                // ESC is the interesting case and the honest reading: the user
                // pressed Escape and nothing followed.
                let pending = std::mem::take(&mut self.buf);
                return Ok(pending.first().map(
                    |&b| {
                        if b == 0x1b { Key::Escape } else { Key::Other }
                    },
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a whole buffer into the keys it holds, plus what was left over.
    fn all(buf: &[u8]) -> (Vec<Key>, usize) {
        let mut keys = Vec::new();
        let mut at = 0;
        while let Some((key, used)) = decode(&buf[at..]) {
            assert!(used > 0, "a zero-length decode would loop forever");
            keys.push(key);
            at += used;
        }
        (keys, buf.len() - at)
    }

    #[test]
    fn the_keys_the_view_acts_on() {
        assert_eq!(decode(b"q"), Some((Key::Char('q'), 1)));
        assert_eq!(decode(b"\x03"), Some((Key::Interrupt, 1)));
        assert_eq!(decode(b"\r"), Some((Key::Enter, 1)));
        assert_eq!(decode(b"\n"), Some((Key::Enter, 1)));
        assert_eq!(decode(b"\x1b[A"), Some((Key::Up, 3)));
        assert_eq!(decode(b"\x1b[B"), Some((Key::Down, 3)));
        assert_eq!(decode(b"\x1b[H"), Some((Key::Home, 3)));
        assert_eq!(decode(b"\x1b[F"), Some((Key::End, 3)));
        assert_eq!(decode(b"\x1b[5~"), Some((Key::PageUp, 4)));
        assert_eq!(decode(b"\x1b[6~"), Some((Key::PageDown, 4)));
        assert_eq!(decode(b"\x1b[1~"), Some((Key::Home, 4)));
        assert_eq!(decode(b"\x1b[4~"), Some((Key::End, 4)));
        // SS3, which is what some terminals send for the arrows.
        assert_eq!(decode(b"\x1bOA"), Some((Key::Up, 3)));
        // Modified keys carry parameters before the final byte and still name
        // the same key.
        assert_eq!(decode(b"\x1b[1;5A"), Some((Key::Up, 6)));
    }

    #[test]
    fn an_incomplete_sequence_asks_for_more_rather_than_guessing() {
        // Every prefix of a real sequence must decode to None — deciding early
        // would turn a half-delivered arrow key into a `[` typed as a command.
        for partial in [&b"\x1b"[..], b"\x1b[", b"\x1b[5", b"\x1b[1;5"] {
            assert_eq!(decode(partial), None, "{partial:?} decoded too early");
        }
        // And a partial UTF-8 character likewise.
        assert_eq!(decode(&[0xE2, 0x82]), None);
        assert_eq!(decode(&[0xE2, 0x82, 0xAC]), Some((Key::Char('€'), 3)));
    }

    #[test]
    fn nothing_makes_the_decoder_stall_or_panic() {
        // The failure that matters is not an attacker — it is a keyboard — so
        // what is under test is that no input makes this loop forever, consume
        // nothing, or panic. `all` asserts the progress condition itself.
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"\x1b".to_vec(),
            b"\x1b\x1b\x1b\x1b".to_vec(),
            b"\x1b[".to_vec(),
            b"\x1b[[[[[[".to_vec(),
            b"\x1b[999999999999999999999999~".to_vec(),
            b"\x1bOOOO".to_vec(),
            vec![0x80; 8],
            vec![0xFF; 8],
            vec![0xF0, 0x9F, 0x8E],
            (0u8..=255).collect(),
            // A parameter run past the bound is abandoned rather than scanned
            // forever.
            {
                let mut v = b"\x1b[".to_vec();
                v.extend(std::iter::repeat_n(b'1', 1000));
                v.push(b'A');
                v
            },
        ];
        for case in cases {
            let (_keys, leftover) = all(&case);
            assert!(
                leftover <= case.len(),
                "decoder consumed more than it was given"
            );
        }
    }

    #[test]
    fn a_long_parameter_run_is_abandoned_at_the_bound() {
        let mut buf = b"\x1b[".to_vec();
        buf.extend(std::iter::repeat_n(b'1', 100));
        buf.push(b'A');
        let (key, used) = decode(&buf).expect("a decision, not a stall");
        assert_eq!(key, Key::Other, "past the bound it is not an arrow key");
        assert!(
            used <= MAX_SEQUENCE + 2,
            "consumed {used} bytes, past the bound"
        );
        assert!(used > 0);
    }

    #[test]
    fn keys_reads_sequences_split_across_reads() {
        // A terminal usually delivers a sequence in one write, but a slow link
        // can split it, and the decoder must not read `[` as a command.
        struct Dribble(Vec<Vec<u8>>);
        impl Read for Dribble {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let chunk = self.0.remove(0);
                out[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
        }
        let mut keys = Keys::new(Dribble(vec![
            b"\x1b".to_vec(),
            b"[".to_vec(),
            b"A".to_vec(),
            b"q".to_vec(),
        ]));
        assert_eq!(keys.next_key().unwrap(), Some(Key::Up));
        assert_eq!(keys.next_key().unwrap(), Some(Key::Char('q')));
        assert_eq!(keys.next_key().unwrap(), None);

        // A lone Escape at the end of input is an Escape, not a stall.
        let mut keys = Keys::new(io::Cursor::new(b"\x1b".to_vec()));
        assert_eq!(keys.next_key().unwrap(), Some(Key::Escape));
        assert_eq!(keys.next_key().unwrap(), None);
    }
}
