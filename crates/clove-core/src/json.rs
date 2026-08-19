//! A minimal hand-rolled JSON encoder + parser (SCOPE §9: no serde).
//!
//! `cloved` *emits* JSON (API responses); `clove` *parses* it to render tables
//! (`--json` passes the body through). Both directions are here, a few hundred
//! lines with exact control over escaping and hostile-input limits — all the
//! surface the local API and CLI require. The daemon still never parses JSON:
//! commands reach it as HTTP method + path + typed bodies.
//!
//! [`Value::Object`] preserves insertion order, so field order in the output
//! is whatever the caller wrote — stable across runs, diff-friendly, and
//! predictable in tests. [`parse`] preserves the order it read, too.

use std::fmt::{self, Write as _};

/// A JSON value to encode.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// JSON `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// An unsigned integer (torrent sizes, counts).
    UInt(u64),
    /// A floating-point number. Non-finite values encode as `null` (JSON has
    /// no `NaN`/`Infinity`).
    Float(f64),
    /// A string. Escaped per RFC 8259 on encode.
    Str(String),
    /// An array of values.
    Array(Vec<Value>),
    /// An object; keys are emitted in insertion order.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Encode to a JSON string.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::UInt(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(f) => {
                if f.is_finite() {
                    // A whole float would print as "1", which re-parses as an
                    // integer; keep the fraction so encode/parse round-trips.
                    // This holds at every magnitude: `{}` never uses exponent
                    // notation for f64, so a large whole float prints as a
                    // bare digit string that reads back as `Int`/`UInt` too.
                    if f.fract() == 0.0 {
                        let _ = write!(out, "{f:.1}");
                    } else {
                        let _ = write!(out, "{f}");
                    }
                } else {
                    out.push_str("null");
                }
            }
            Value::Str(s) => write_string(s, out),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Value::Object(fields) => {
                out.push('{');
                for (i, (key, val)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    val.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Convenience: build a string value from anything `Into<String>`.
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl Value {
    /// This object's value for `key`, if this is an [`Object`](Value::Object)
    /// that has it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string, if this is a [`Str`](Value::Str).
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The boolean, if this is a [`Bool`](Value::Bool). Strict: `1` and
    /// `"true"` are not booleans, and a reader that accepts them hides the
    /// encoder bug that produced them.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The elements, if this is an [`Array`](Value::Array).
    #[must_use]
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// This value as `u64` if it is a non-negative integer. Numeric fields
    /// should be read through this or [`as_f64`](Value::as_f64) rather than
    /// matched on a specific variant: which one a number arrives as depends on
    /// how it was written, and JSON itself draws no such distinction.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::UInt(n) => Some(*n),
            Value::Int(n) => u64::try_from(*n).ok(),
            _ => None,
        }
    }

    /// This value as `f64` if it is any number (for display; large integers
    /// may lose precision).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "used for formatting; exact precision is not required"
    )]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::UInt(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// A compact one-line human rendering for table cells: a string bare
    /// (unquoted), `null` as `-`, and anything else as its JSON.
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Null => "-".to_owned(),
            other => other.encode(),
        }
    }
}

/// Deepest nesting [`parse`] accepts, so a hostile `[[[[…` cannot exhaust the
/// stack.
const MAX_DEPTH: usize = 128;

/// Why parsing failed: a byte offset and a fixed reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Byte offset into the input where the problem was found.
    pub at: usize,
    /// What went wrong.
    pub what: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "json: {} at byte {}", self.what, self.at)
    }
}

impl std::error::Error for ParseError {}

/// Parse one JSON value from `input`, rejecting trailing data.
///
/// # Errors
///
/// A [`ParseError`] with a byte offset on malformed input: bad tokens,
/// unterminated or badly escaped strings, numbers that do not parse, nesting
/// past the nesting limit (`MAX_DEPTH`, 128), or trailing bytes after the
/// value.
pub fn parse(input: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
        depth: 0,
    };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.err("trailing data after JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err(&self, what: &'static str) -> ParseError {
        ParseError { at: self.pos, what }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("expected a JSON value")),
        }
    }

    fn literal(&mut self, word: &[u8], value: Value) -> Result<Value, ParseError> {
        if self.bytes[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.pos += 1; // consume '{'
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Object(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected a string key"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after key"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
        self.depth -= 1;
        Ok(Value::Object(fields))
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.pos += 1; // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        self.depth += 1;
        Ok(())
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1; // consume opening quote
        let mut out = String::new();
        loop {
            // Copy a run of ordinary bytes. The run breaks only at '"', '\\',
            // or a control byte — all ASCII — so the slice is whole UTF-8.
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            if self.pos > start {
                match std::str::from_utf8(&self.bytes[start..self.pos]) {
                    Ok(chunk) => out.push_str(chunk),
                    Err(_) => return Err(self.err("invalid UTF-8 in string")),
                }
            }
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.err("control character in string")),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        match self.peek() {
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{08}'),
            Some(b'f') => out.push('\u{0C}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'u') => return self.unicode_escape(out),
            _ => return Err(self.err("invalid string escape")),
        }
        self.pos += 1;
        Ok(())
    }

    fn unicode_escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        self.pos += 1; // consume 'u'
        let hi = self.hex4()?;
        let code = if (0xD800..=0xDBFF).contains(&hi) {
            // High surrogate: must be followed by \uXXXX low surrogate.
            if self.peek() != Some(b'\\') {
                return Err(self.err("lone high surrogate"));
            }
            self.pos += 1;
            if self.peek() != Some(b'u') {
                return Err(self.err("lone high surrogate"));
            }
            self.pos += 1;
            let lo = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&lo) {
                return Err(self.err("invalid low surrogate"));
            }
            0x1_0000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00)
        } else if (0xDC00..=0xDFFF).contains(&hi) {
            return Err(self.err("lone low surrogate"));
        } else {
            u32::from(hi)
        };
        match char::from_u32(code) {
            Some(c) => {
                out.push(c);
                Ok(())
            }
            None => Err(self.err("invalid unicode escape")),
        }
    }

    fn hex4(&mut self) -> Result<u16, ParseError> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(c @ b'0'..=b'9') => u16::from(c - b'0'),
                Some(c @ b'a'..=b'f') => u16::from(c - b'a' + 10),
                Some(c @ b'A'..=b'F') => u16::from(c - b'A' + 10),
                _ => return Err(self.err("expected 4 hex digits")),
            };
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number"))?;
        if !is_float {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Value::Int(n));
            }
            if let Ok(n) = text.parse::<u64>() {
                return Ok(Value::UInt(n));
            }
        }
        // Either a genuine float, or an integer literal too wide for u64.
        let value = text
            .parse::<f64>()
            .map_err(|_| self.err("invalid number"))?;
        if !value.is_finite() {
            // A literal whose magnitude overflows f64 parses to infinity,
            // which JSON has no text for: the encoder can only write it back
            // as `null`. Refuse it here rather than hand the caller a value
            // that changes meaning the moment it is re-serialised.
            return Err(self.err("number out of range"));
        }
        Ok(Value::Float(value))
    }
}

/// Write a JSON string literal, escaping per RFC 8259: `"` and `\` are
/// backslash-escaped, the shortcuts `\b \t \n \f \r` are used where they
/// apply, and everything [`needs_escaping`] names becomes `\uXXXX`. Other
/// non-ASCII UTF-8 passes through unescaped (valid JSON).
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if needs_escaping(c) => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Whether a character is written as a `\uXXXX` escape rather than as itself.
///
/// RFC 8259 requires this of `0x00..0x20` and permits it of anything else, and
/// what the permission is spent on is the point. `clove list --json` and
/// `clove show --json` print this text straight to a terminal, and a terminal
/// is an interpreter: the characters [`crate::text::scrub`] refuses to put in
/// front of a person are exactly the ones that must not survive the encoder
/// either. Escaping them here rather than scrubbing them there is what lets
/// both hold at once — a JSON consumer unescapes and gets the torrent's real
/// name back, and a terminal is shown six inert ASCII characters.
///
/// The set is [`crate::text::scrub`]'s, character for character:
///
/// - the `Cc` category — C0, `DEL` and C1 — where the escape sequences, the
///   carriage returns and the newlines live (`0x00..0x20` is the RFC's
///   requirement and the rest of `Cc` is this function's addition); and
/// - the bidirectional overrides and isolates, which draw nothing themselves
///   and reorder the text around them, so `…rat.exe` can be made to render as
///   `…exe.tar` in the very listing an operator is reading.
fn needs_escaping(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(Value::Null.encode(), "null");
        assert_eq!(Value::Bool(true).encode(), "true");
        assert_eq!(Value::Int(-42).encode(), "-42");
        assert_eq!(Value::UInt(42).encode(), "42");
        assert_eq!(Value::Str("hi".to_owned()).encode(), "\"hi\"");
    }

    #[test]
    fn whole_floats_keep_their_fraction() {
        // Otherwise `1` comes back as Int and the round trip is lossy.
        assert_eq!(Value::Float(1.0).encode(), "1.0");
        assert_eq!(Value::Float(-15e9).encode(), "-15000000000.0");
        assert_eq!(parse("1.0").unwrap(), Value::Float(1.0));
        // Regression, found by the `json` fuzz target: the fraction used to be
        // kept only below 1e15, so a large whole float encoded as a bare digit
        // string and read back as `Int(9111111111111111000)`.
        let big = parse("911111111111111111e1").unwrap();
        assert_eq!(big, Value::Float(9.111_111_111_111_111e18));
        assert_eq!(parse(&big.encode()).unwrap(), big);
        for f in [1e15, -1e15, 1e300, f64::MAX, f64::MIN] {
            let value = Value::Float(f);
            assert_eq!(
                parse(&value.encode()).unwrap(),
                value,
                "{f:e} lost its type"
            );
        }
    }

    #[test]
    fn numbers_beyond_f64_are_refused() {
        // `1e400` parses to infinity, which the encoder can only write back as
        // `null`: a value that changes meaning on re-serialisation is an error
        // here, not something to hand the caller.
        assert!(parse("1e400").is_err());
        assert!(parse("-1e400").is_err());
        // The same literal without an exponent: too wide for u64 and for f64.
        assert!(parse(&format!("1{}", "0".repeat(400))).is_err());
        // What still fits is a float, and survives the round trip.
        assert_eq!(parse("1e308").unwrap(), Value::Float(1e308));
        let wide = parse("123456789012345678901234567890").unwrap();
        assert_eq!(parse(&wide.encode()).unwrap(), wide);
    }

    #[test]
    fn non_finite_floats_are_null() {
        assert_eq!(Value::Float(1.5).encode(), "1.5");
        assert_eq!(Value::Float(f64::NAN).encode(), "null");
        assert_eq!(Value::Float(f64::INFINITY).encode(), "null");
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(
            Value::from("a\"b\\c\n\t\u{01}").encode(),
            "\"a\\\"b\\\\c\\n\\t\\u0001\""
        );
        // Non-ASCII passes through as UTF-8.
        assert_eq!(Value::from("café").encode(), "\"café\"");
    }

    /// The encoder's half of the promise `crate::text` makes: text a stranger
    /// wrote reaches a terminal through `clove list --json` without passing
    /// through `scrub`, so nothing a terminal acts on may survive encoding.
    #[test]
    fn terminal_hazards_do_not_survive_encoding() {
        // DEL and C1 are controls that `0x20` alone does not cover; the C1 run
        // is where a terminal in 8-bit mode finds CSI.
        assert_eq!(Value::from("a\u{7f}b").encode(), "\"a\\u007fb\"");
        assert_eq!(Value::from("a\u{9b}b").encode(), "\"a\\u009bb\"");
        // The classic filename spoof: RLO renders "safe.exe.gnp" as
        // "safe.gnp.exe" — inside a JSON string exactly as outside one.
        assert_eq!(
            Value::from("safe\u{202e}gnp.exe").encode(),
            "\"safe\\u202egnp.exe\""
        );
        for c in [
            '\u{0}', '\u{1b}', '\u{7f}', '\u{80}', '\u{9b}', '\u{200e}', '\u{200f}', '\u{202a}',
            '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
            '\u{2069}',
        ] {
            let encoded = Value::from(c.to_string().as_str()).encode();
            assert!(
                !encoded.chars().any(|e| e == c),
                "{c:?} survived encoding as {encoded:?}"
            );
            // Escaped, not dropped: a consumer that unescapes gets the real
            // name back, which is what the API promises.
            assert_eq!(
                parse(&encoded).unwrap(),
                Value::from(c.to_string().as_str())
            );
        }
        // Text an operator can actually read is untouched, non-ASCII included.
        for ok in ["plain-name_1.0.iso", "café", "日本語のファイル", "Ünïcödé"] {
            assert_eq!(Value::from(ok).encode(), format!("\"{ok}\""));
        }
    }

    #[test]
    fn nested_and_ordered() {
        let v = Value::Object(vec![
            ("name".to_owned(), Value::from("demo")),
            ("done".to_owned(), Value::Bool(false)),
            (
                "peers".to_owned(),
                Value::Array(vec![Value::UInt(1), Value::UInt(2)]),
            ),
        ]);
        // Field order follows insertion, not sorting.
        assert_eq!(v.encode(), r#"{"name":"demo","done":false,"peers":[1,2]}"#);
    }

    #[test]
    fn empty_containers() {
        assert_eq!(Value::Array(vec![]).encode(), "[]");
        assert_eq!(Value::Object(vec![]).encode(), "{}");
    }

    #[test]
    fn parses_scalars_and_numbers() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse(" true ").unwrap(), Value::Bool(true));
        assert_eq!(parse("-42").unwrap(), Value::Int(-42));
        // Positive value beyond i64 becomes UInt.
        assert_eq!(
            parse("18446744073709551615").unwrap(),
            Value::UInt(u64::MAX)
        );
        assert_eq!(parse("1.5e3").unwrap(), Value::Float(1500.0));
        assert_eq!(parse("\"hi\"").unwrap(), Value::from("hi"));
    }

    #[test]
    fn round_trips_a_nested_object() {
        let original = Value::Object(vec![
            ("name".to_owned(), Value::from("dé\"mo")),
            ("done".to_owned(), Value::Bool(false)),
            (
                "peers".to_owned(),
                // Small positive ints parse back as Int (JSON has no unsigned
                // distinction), so use Int here for an exact round-trip.
                Value::Array(vec![Value::Int(1), Value::Null]),
            ),
        ]);
        let reparsed = parse(&original.encode()).unwrap();
        assert_eq!(reparsed, original);
    }

    #[test]
    fn decodes_string_escapes_and_surrogates() {
        assert_eq!(parse(r#""a\nb\t\"c""#).unwrap(), Value::from("a\nb\t\"c"));
        assert_eq!(parse(r#""A""#).unwrap(), Value::from("A"));
        // Surrogate pair for U+1F600.
        assert_eq!(parse(r#""😀""#).unwrap(), Value::from("😀"));
    }

    #[test]
    fn accessors() {
        let v = parse(r#"{"a":"x","n":[1,2]}"#).unwrap();
        assert_eq!(v.get("a").and_then(Value::as_str), Some("x"));
        assert_eq!(
            v.get("n").and_then(Value::as_array).map(<[_]>::len),
            Some(2)
        );
        assert!(v.get("missing").is_none());
    }

    #[test]
    fn rejects_hostile_input() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"k\":}",
            "\"unterminated",
            "\"\\x\"",
            "nul",
            "1 2",
            "\"\\uD83D\"", // lone high surrogate
            "-",
        ] {
            assert!(parse(bad).is_err(), "should reject {bad:?}");
        }
        // Nesting past the depth cap is refused, not a stack overflow.
        let deep = "[".repeat(MAX_DEPTH + 5);
        assert!(parse(&deep).is_err());
    }
}
