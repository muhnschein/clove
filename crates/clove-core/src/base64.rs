//! Standard-alphabet base64, decode only (RFC 4648 §4).
//!
//! I2P's own base64 lives in [`i2pnet::addr`] and uses a different alphabet
//! (`+`→`-`, `/`→`~`), so it cannot read this and this cannot read it. The two
//! are deliberately separate functions rather than one parameterised over an
//! alphabet: they decode different things arriving from different places, and
//! a shared implementation would be one edit away from letting a destination
//! parse as a torrent or the reverse.
//!
//! The only caller is the Transmission RPC surface, whose `torrent-add` carries
//! a `.torrent` as base64 in a JSON string. That input is a stranger's, so this
//! is a hostile-input parser like every other one here: it allocates only what
//! the input's length implies, returns [`None`] rather than panicking, and
//! refuses non-canonical encodings instead of guessing at them.
//!
//! There is no encoder. Nothing clove emits is base64 in this alphabet, and
//! writing one "for symmetry" is the sort of unused code `SCOPE.md` §9 asks us
//! to take pride in not having.

/// Decode standard base64, returning [`None`] for anything malformed.
///
/// ASCII whitespace — space, tab, CR, LF — is ignored anywhere in the input.
/// That leniency is deliberate and bounded: MIME-style encoders wrap at 76
/// columns (Python's `base64.encodebytes`, `base64(1)` without `-w0`), and a
/// script that pipes one of those into `torrent-add` is a normal way to reach
/// this function. Every *other* byte outside the alphabet is a rejection.
///
/// What it refuses, all of which some decoder somewhere accepts:
///
/// - characters outside `A–Z a–z 0–9 + /`, including I2P's `-` and `~`
/// - `=` anywhere but at the end, or more than two of them
/// - a symbol count of `4n + 1`, which encodes no whole byte
/// - padding that disagrees with the symbol count
/// - a final symbol whose unused low bits are not zero, e.g. `"AB"` — the
///   canonical encoding of that byte is `"AA"`, and accepting both means two
///   spellings of one value
#[must_use]
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let mut symbols = Vec::with_capacity(input.len());
    let mut padding = 0usize;
    for byte in input.bytes() {
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'=' => {
                padding += 1;
                if padding > 2 {
                    return None;
                }
            }
            _ => {
                // Padding is terminal: a symbol after one means the input is
                // two concatenated encodings, not one.
                if padding > 0 {
                    return None;
                }
                symbols.push(symbol_value(byte)?);
            }
        }
    }

    // Six bits alone encode nothing, whatever the padding claims.
    if symbols.len() % 4 == 1 {
        return None;
    }
    // Padding, when present at all, must complete the final quantum exactly.
    if padding > 0 && !(symbols.len() + padding).is_multiple_of(4) {
        return None;
    }

    let mut out = Vec::with_capacity(symbols.len() / 4 * 3 + 2);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for value in symbols {
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    // Leftover bits belong to no byte and must therefore be zero; anything
    // else is a second spelling of the value we just decoded.
    if bits > 0 && (buffer & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

/// The six-bit value of one base64 symbol, or [`None`] if it is not one.
fn symbol_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode;

    /// RFC 4648 §10's test vectors, which exercise all three padding cases.
    #[test]
    fn rfc4648_vectors() {
        for (encoded, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(
                decode(encoded).as_deref(),
                Some(plain.as_bytes()),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn padding_is_optional() {
        // Every vector above, with the padding stripped, decodes the same.
        assert_eq!(decode("Zg").as_deref(), Some(&b"f"[..]));
        assert_eq!(decode("Zm8").as_deref(), Some(&b"fo"[..]));
        assert_eq!(decode("Zm9vYg").as_deref(), Some(&b"foob"[..]));
    }

    #[test]
    fn whitespace_anywhere_is_ignored() {
        // What a MIME-wrapping encoder produces, and what a shell here-doc
        // does to it on the way in.
        assert_eq!(decode("Zm9v\r\nYmFy").as_deref(), Some(&b"foobar"[..]));
        assert_eq!(decode("  Zm9v Ym Fy\t\n").as_deref(), Some(&b"foobar"[..]));
    }

    #[test]
    fn the_two_bytes_that_differ_from_the_i2p_alphabet_are_refused() {
        // A destination pasted into a metainfo field must not half-decode.
        assert_eq!(decode("Zm9-"), None);
        assert_eq!(decode("Zm9~"), None);
    }

    #[test]
    fn malformed_inputs_are_refused() {
        for bad in [
            "Z",         // 4n+1 symbols encode no whole byte
            "Zm9vYg=",   // padding disagrees with the symbol count
            "Zm9vY===",  // more than two pad bytes
            "Zg==Zg==",  // two encodings concatenated
            "Zm9v!",     // outside the alphabet
            "Zm9v\u{0}", // NUL is not whitespace
            "AB",        // non-zero trailing bits; "AA" is the canonical form
        ] {
            assert_eq!(decode(bad), None, "{bad:?} should not decode");
        }
    }

    #[test]
    fn both_alphabet_specific_symbols_decode() {
        // `+` and `/` are the two the I2P alphabet spells differently, so a
        // decoder that quietly reused that table would fail exactly here.
        assert_eq!(decode("+/+/").as_deref(), Some(&[0xfb, 0xff, 0xbf][..]));
    }

    #[test]
    fn every_byte_value_round_trips_through_a_known_encoding() {
        // 0..=255 encoded by an independent implementation (Python's
        // base64.b64encode), so this checks us against something that is not
        // ourselves — a decoder tested only against its own encoder proves
        // nothing about the format.
        const ALL_BYTES_B64: &str = "\
AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4\
OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3Bx\
cnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmq\
q6ytrq+wsbKztLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj\
5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w==";
        let decoded = decode(ALL_BYTES_B64).expect("the vector is well formed");
        let expected: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn a_long_input_does_not_over_allocate() {
        // The capacity hint is derived from the input length, so a pathological
        // input of pure whitespace must not reserve gigabytes.
        let spaces = " ".repeat(100_000);
        assert_eq!(decode(&spaces).as_deref(), Some(&[][..]));
    }
}
