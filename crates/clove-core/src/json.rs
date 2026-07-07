//! A minimal hand-rolled JSON encoder (SCOPE §9: no serde).
//!
//! clove needs to *emit* JSON — API responses and the CLI's `--json` output —
//! but never to parse it (commands reach the daemon as HTTP method + path +
//! small typed bodies, not arbitrary JSON), so this is an encoder only. It is
//! a few hundred lines with exact control over escaping, which is all the
//! surface the local API and CLI require.
//!
//! [`Value::Object`] preserves insertion order, so field order in the output
//! is whatever the caller wrote — stable across runs, diff-friendly, and
//! predictable in tests.

use std::fmt::Write as _;

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
                    let _ = write!(out, "{f}");
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

/// Write a JSON string literal, escaping per RFC 8259: `"` and `\` are
/// backslash-escaped, the shortcuts `\b \t \n \f \r` are used where they
/// apply, and any other control character below `0x20` becomes `\u00XX`.
/// Non-ASCII UTF-8 passes through unescaped (valid JSON).
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
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
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
}
