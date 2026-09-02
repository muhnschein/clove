//! Hand-rolled bencode codec (`docs/DECISIONS.md`).
//!
//! Decodes .torrent metainfo and clove's own resume files, so it is written
//! for hostile input: hard depth limit, strict integer and length syntax,
//! duplicate dictionary keys rejected, and byte-string lengths checked
//! against the remaining input before anything is allocated.
//!
//! Out-of-order dictionary keys are tolerated on decode (broken torrents
//! exist in the wild); duplicate keys are not. Encoding is always canonical
//! (sorted keys), so decode→encode is not byte-identical for non-canonical
//! foreign input — anything hash-sensitive (the info dictionary) must hash
//! the raw input bytes, obtained via [`raw_entry`].

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

/// Maximum nesting depth the decoder accepts.
pub const MAX_DEPTH: u32 = 32;

/// Fewest input bytes any one node costs: a list or dictionary is its `l`/`d`
/// and its `e`, a byte string its length digit and `:`, an integer one more.
pub const MIN_NODE_BYTES: usize = 2;

/// Most [`Value`]s the decoder will build for `input_len` bytes of input.
///
/// This is the decoder's memory bound written down: at most this many nodes
/// of `size_of::<Value>()` bytes each, a fixed multiple of the input rather
/// than an argument about the grammar. The grammar cannot exceed it — every
/// node costs [`MIN_NODE_BYTES`] — so the check that enforces it is the
/// invariant made explicit, there to fail loudly if a future leniency ever
/// lets a node cost less, not a case anyone expects to see.
#[must_use]
pub fn node_budget(input_len: usize) -> usize {
    input_len / MIN_NODE_BYTES
}

/// A bencode value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Byte string; not necessarily UTF-8.
    Bytes(Vec<u8>),
    /// Integer. The wire format is unbounded; clove bounds it to `i64`.
    Int(i64),
    /// List.
    List(Vec<Value>),
    /// Dictionary. `BTreeMap` keeps keys sorted, making encoding canonical.
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    /// The byte string, if this is one.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// The byte string as UTF-8, if this is one and it is valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    /// The integer, if this is one.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The list items, if this is a list.
    #[must_use]
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }

    /// The map, if this is a dictionary.
    #[must_use]
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(map) => Some(map),
            _ => None,
        }
    }

    /// Dictionary lookup, if this is a dictionary containing `key`.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        self.as_dict().and_then(|d| d.get(key))
    }
}

/// Decode error: what went wrong and the input offset where it did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// Byte offset into the input at the point of failure.
    pub offset: usize,
    /// What was wrong there.
    pub kind: ErrorKind,
}

/// The kinds of malformed input the decoder rejects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Input ended before the value did.
    Truncated,
    /// A complete value followed by leftover bytes.
    TrailingData,
    /// Malformed integer: no digits, leading zeros, `-0`, or outside `i64`.
    BadInt,
    /// Malformed byte-string length prefix.
    BadLength,
    /// Dictionary key that is not a byte string, or repeats an earlier key.
    BadKey,
    /// Nesting deeper than [`MAX_DEPTH`].
    TooDeep,
    /// A byte that cannot start a value.
    BadValueStart,
    /// More nodes than [`node_budget`] allows for the input's length.
    TooManyNodes,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ErrorKind::Truncated => "input truncated mid-value",
            ErrorKind::TrailingData => "trailing bytes after the value",
            ErrorKind::BadInt => "malformed integer",
            ErrorKind::BadLength => "malformed string length",
            ErrorKind::BadKey => "duplicate or non-string dictionary key",
            ErrorKind::TooDeep => "nesting deeper than the decoder allows",
            ErrorKind::BadValueStart => "byte cannot start a value",
            ErrorKind::TooManyNodes => "more values than the input could hold",
        };
        write!(f, "bencode: {what} at offset {}", self.offset)
    }
}

impl std::error::Error for Error {}

/// Decode one complete bencode value; the entire input must be consumed.
///
/// # Errors
///
/// Any syntax violation, truncation, trailing data, duplicate dictionary
/// key, or nesting past [`MAX_DEPTH`], reported with the offending offset.
pub fn decode(input: &[u8]) -> Result<Value, Error> {
    decode_with_entry(input, &[]).map(|(value, _)| value)
}

/// Decode one bencode value from the start of `input`, returning it and the
/// number of bytes consumed. Unlike [`decode`], trailing bytes are allowed —
/// for framings where a bencoded header is followed by raw data (BEP 9
/// `ut_metadata` data messages, `crate::metadata`).
///
/// # Errors
///
/// Any syntax violation, truncation, duplicate key, or over-deep nesting in
/// the leading value.
pub fn decode_prefix(input: &[u8]) -> Result<(Value, usize), Error> {
    let mut d = Decoder::new(input, &[]);
    let value = d.value(0)?;
    Ok((value, d.pos))
}

/// [`decode`], also reporting where the value stored under `key` in the
/// top-level dictionary sits in `input` — e.g. the exact `info` bytes an
/// info-hash is computed over.
///
/// Found in the same pass that validates the input, so a caller that needs
/// both the tree and the bytes pays for one decode, not for a second over
/// input it already holds as a tree. The range is `None` if the top-level
/// value is not a dictionary or has no such key.
///
/// # Errors
///
/// The same syntax errors as [`decode`].
pub fn decode_with_entry(input: &[u8], key: &[u8]) -> Result<(Value, Option<Range<usize>>), Error> {
    let mut d = Decoder::new(input, key);
    let value = d.value(0)?;
    if d.pos == input.len() {
        Ok((value, d.span.take()))
    } else {
        Err(d.err(ErrorKind::TrailingData))
    }
}

/// Byte range of the raw encoded value stored under `key` in the top-level
/// dictionary — [`decode_with_entry`] for a caller that wants only the range.
///
/// The whole input is validated (as [`decode`] would), so the returned range
/// is trustworthy. Returns `Ok(None)` if the top-level value is not a
/// dictionary or has no such key.
///
/// # Errors
///
/// The same syntax errors as [`decode`].
pub fn raw_entry(input: &[u8], key: &[u8]) -> Result<Option<Range<usize>>, Error> {
    decode_with_entry(input, key).map(|(_, range)| range)
}

/// Canonically encode `value` (dictionary keys sorted, strict syntax).
#[must_use]
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

/// Append the canonical encoding of `value` to `out`.
pub fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Bytes(b) => put_bytes(b, out),
        Value::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(map) => {
            out.push(b'd');
            for (k, v) in map {
                put_bytes(k, out);
                encode_into(v, out);
            }
            out.push(b'e');
        }
    }
}

fn put_bytes(b: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    /// Nodes built so far, against `budget`.
    nodes: usize,
    /// [`node_budget`] of the input, or smaller under test.
    budget: usize,
    /// The top-level dictionary key whose value's byte range is wanted, if
    /// any; an empty key can never match, since keys are never empty here.
    want: &'a [u8],
    /// Where the value under `want` was found.
    span: Option<Range<usize>>,
}

impl<'a> Decoder<'a> {
    fn new(input: &'a [u8], want: &'a [u8]) -> Self {
        Decoder {
            input,
            pos: 0,
            nodes: 0,
            budget: node_budget(input.len()),
            want,
            span: None,
        }
    }

    fn err(&self, kind: ErrorKind) -> Error {
        Error {
            offset: self.pos,
            kind,
        }
    }

    fn peek(&self) -> Result<u8, Error> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.err(ErrorKind::Truncated))
    }

    fn value(&mut self, depth: u32) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return Err(self.err(ErrorKind::TooDeep));
        }
        let value = self.node(depth)?;
        // Counted once built, not once begun: a node that is still open has
        // cost one byte so far, and an input of nothing but `l` would run
        // into this before it ran into the depth limit that is the right
        // complaint about it. A built node has paid its [`MIN_NODE_BYTES`],
        // and there are never more than [`MAX_DEPTH`] open.
        self.nodes += 1;
        if self.nodes > self.budget {
            return Err(self.err(ErrorKind::TooManyNodes));
        }
        Ok(value)
    }

    fn node(&mut self, depth: u32) -> Result<Value, Error> {
        match self.peek()? {
            b'i' => self.int().map(Value::Int),
            b'0'..=b'9' => self.bytes().map(Value::Bytes),
            b'l' => {
                self.pos += 1;
                let mut items = Vec::new();
                while self.peek()? != b'e' {
                    items.push(self.value(depth + 1)?);
                }
                self.pos += 1;
                Ok(Value::List(items))
            }
            b'd' => {
                self.pos += 1;
                let mut map = BTreeMap::new();
                while self.peek()? != b'e' {
                    let key_offset = self.pos;
                    if !self.peek()?.is_ascii_digit() {
                        return Err(Error {
                            offset: key_offset,
                            kind: ErrorKind::BadKey,
                        });
                    }
                    let key = self.bytes()?;
                    let start = self.pos;
                    let val = self.value(depth + 1)?;
                    // Only the top-level dictionary is asked about: an `info`
                    // key nested inside some other value is not the torrent's.
                    if depth == 0 && key == self.want {
                        self.span = Some(start..self.pos);
                    }
                    if map.insert(key, val).is_some() {
                        return Err(Error {
                            offset: key_offset,
                            kind: ErrorKind::BadKey,
                        });
                    }
                }
                self.pos += 1;
                Ok(Value::Dict(map))
            }
            _ => Err(self.err(ErrorKind::BadValueStart)),
        }
    }

    /// Parse `i<digits>e`. Accumulates negatively so `i64::MIN` round-trips
    /// while `i64::MAX + 1` is rejected.
    fn int(&mut self) -> Result<i64, Error> {
        let start = self.pos;
        self.pos += 1; // 'i'
        let negative = self.peek()? == b'-';
        if negative {
            self.pos += 1;
        }
        let digits_start = self.pos;
        let mut n: i64 = 0;
        loop {
            let b = self.peek()?;
            if b == b'e' {
                break;
            }
            if !b.is_ascii_digit() {
                return Err(Error {
                    offset: start,
                    kind: ErrorKind::BadInt,
                });
            }
            n = n
                .checked_mul(10)
                .and_then(|v| v.checked_sub(i64::from(b - b'0')))
                .ok_or(Error {
                    offset: start,
                    kind: ErrorKind::BadInt,
                })?;
            self.pos += 1;
        }
        let ndigits = self.pos - digits_start;
        let bad = ndigits == 0
            || (ndigits > 1 && self.input[digits_start] == b'0')
            || (negative && n == 0);
        if bad {
            return Err(Error {
                offset: start,
                kind: ErrorKind::BadInt,
            });
        }
        self.pos += 1; // 'e'
        if negative {
            Ok(n)
        } else {
            n.checked_neg().ok_or(Error {
                offset: start,
                kind: ErrorKind::BadInt,
            })
        }
    }

    /// Parse `<digits>:<bytes>`. The declared length is validated against
    /// the remaining input before the payload is copied.
    fn bytes(&mut self) -> Result<Vec<u8>, Error> {
        let range = self.bytes_span()?;
        Ok(self.input[range].to_vec())
    }

    fn bytes_span(&mut self) -> Result<Range<usize>, Error> {
        let start = self.pos;
        let mut len: usize = 0;
        let mut ndigits = 0usize;
        loop {
            let b = self.peek()?;
            if b == b':' {
                break;
            }
            if !b.is_ascii_digit() {
                return Err(Error {
                    offset: start,
                    kind: ErrorKind::BadLength,
                });
            }
            len = len
                .checked_mul(10)
                .and_then(|v| v.checked_add(usize::from(b - b'0')))
                .ok_or(Error {
                    offset: start,
                    kind: ErrorKind::BadLength,
                })?;
            ndigits += 1;
            self.pos += 1;
        }
        if ndigits == 0 || (ndigits > 1 && self.input[start] == b'0') {
            return Err(Error {
                offset: start,
                kind: ErrorKind::BadLength,
            });
        }
        self.pos += 1; // ':'
        let end = self
            .pos
            .checked_add(len)
            .filter(|&e| e <= self.input.len())
            .ok_or_else(|| self.err(ErrorKind::Truncated))?;
        let range = self.pos..end;
        self.pos = end;
        Ok(range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn round_trips() {
        let cases: &[&[u8]] = &[
            b"i0e",
            b"i-1e",
            b"i9223372036854775807e",
            b"i-9223372036854775808e",
            b"0:",
            b"4:spam",
            b"le",
            b"de",
            b"l4:spami42ee",
            b"d3:bar4:spam3:fooi42ee",
            b"d1:ad1:bl1:c1:deee",
        ];
        for case in cases {
            let v = decode(case).unwrap();
            assert_eq!(encode(&v).as_slice(), *case, "case {case:?}");
        }
    }

    #[test]
    fn rejects_malformed_integers() {
        for case in [
            &b"ie"[..],
            b"i-e",
            b"i-0e",
            b"i01e",
            b"i-01e",
            b"i1x2e",
            b"i9223372036854775808e",
            b"i-9223372036854775809e",
            b"i42",
        ] {
            assert!(decode(case).is_err(), "accepted {case:?}");
        }
    }

    #[test]
    fn rejects_malformed_strings() {
        for case in [&b"5:spam"[..], b"01:a", b":a", b"4x:spam", b"1:"] {
            assert!(decode(case).is_err(), "accepted {case:?}");
        }
        // A length prefix wildly larger than the input must fail without
        // allocating anything of that size.
        assert_eq!(
            decode(b"99999999999999999999999:x").unwrap_err().kind,
            ErrorKind::BadLength
        );
        assert_eq!(
            decode(b"4294967295:x").unwrap_err().kind,
            ErrorKind::Truncated
        );
    }

    #[test]
    fn rejects_structure_violations() {
        assert_eq!(decode(b"").unwrap_err().kind, ErrorKind::Truncated);
        assert_eq!(decode(b"x").unwrap_err().kind, ErrorKind::BadValueStart);
        assert_eq!(decode(b"i1ei2e").unwrap_err().kind, ErrorKind::TrailingData);
        assert_eq!(decode(b"l4:spam").unwrap_err().kind, ErrorKind::Truncated);
        assert_eq!(decode(b"di1e1:ae").unwrap_err().kind, ErrorKind::BadKey);
        assert_eq!(
            decode(b"d1:ai1e1:ai2ee").unwrap_err().kind,
            ErrorKind::BadKey,
            "duplicate keys"
        );
    }

    #[test]
    fn tolerates_unsorted_keys_but_encodes_canonically() {
        let v = decode(b"d1:bi2e1:ai1ee").unwrap();
        assert_eq!(encode(&v), b"d1:ai1e1:bi2ee");
    }

    #[test]
    fn enforces_depth_limit() {
        let depth = usize::try_from(MAX_DEPTH).unwrap();
        let evil = vec![b'l'; depth + 1];
        assert_eq!(decode(&evil).unwrap_err().kind, ErrorKind::TooDeep);

        // Exactly at the limit is fine.
        let mut ok = vec![b'l'; depth];
        ok.extend(vec![b'e'; depth]);
        assert!(decode(&ok).is_ok());
    }

    #[test]
    fn raw_entry_returns_exact_spans() {
        let input = b"d4:infod4:name1:x6:lengthi5ee5:otheri1ee";
        let range = raw_entry(input, b"info").unwrap().unwrap();
        assert_eq!(&input[range], b"d4:name1:x6:lengthi5ee");
        assert_eq!(raw_entry(input, b"missing").unwrap(), None);
        assert_eq!(raw_entry(b"i42e", b"info").unwrap(), None);
        assert!(raw_entry(b"d4:info", b"info").is_err());
    }

    /// The span comes out of the one validating pass, and it is the
    /// top-level entry's: a key of the same name nested somewhere else is not
    /// the one asked for, and the key's position among its siblings does not
    /// matter.
    #[test]
    fn decode_with_entry_finds_the_top_level_entry_in_one_pass() {
        let input = b"d1:ad4:infoi1ee4:infod1:xi2ee1:zi3ee";
        let (value, range) = decode_with_entry(input, b"info").unwrap();
        assert_eq!(value, decode(input).unwrap());
        assert_eq!(&input[range.unwrap()], b"d1:xi2ee");
        // Absent, or not a dictionary at all: no span, but still a value.
        assert_eq!(decode_with_entry(input, b"nope").unwrap().1, None);
        assert_eq!(decode_with_entry(b"li1ee", b"info").unwrap().1, None);
        // An empty key never matches; `decode` relies on that.
        assert_eq!(decode_with_entry(input, b"").unwrap().1, None);
        // Trailing data is still a refusal, span or no span.
        assert_eq!(
            decode_with_entry(b"d4:infoi1eei2e", b"info")
                .unwrap_err()
                .kind,
            ErrorKind::TrailingData
        );
    }

    fn count_nodes(value: &Value) -> usize {
        match value {
            Value::Bytes(_) | Value::Int(_) => 1,
            Value::List(items) => 1 + items.iter().map(count_nodes).sum::<usize>(),
            Value::Dict(map) => 1 + map.values().map(count_nodes).sum::<usize>(),
        }
    }

    /// The densest input there is — a flood of empty lists at the wire's
    /// message ceiling — decodes to exactly the budget, so the bound is tight
    /// and the decoder's memory is a fixed multiple of its input. Past the
    /// budget is an error, not growth.
    #[test]
    fn node_budget_is_tight_and_enforced() {
        let len = usize::try_from(crate::wire::MAX_MESSAGE_LEN).unwrap();
        let mut flood = vec![b'l'; 1];
        while flood.len() + 3 <= len {
            flood.extend_from_slice(b"le");
        }
        flood.push(b'e');
        let value = decode(&flood).unwrap();
        assert_eq!(count_nodes(&value), node_budget(flood.len()));
        assert_eq!(count_nodes(&value), flood.len() / MIN_NODE_BYTES);

        // Integers cost more than the minimum, so a flood of them sits under
        // the budget; strings and dictionaries sit on it.
        for (dense, nodes) in [(&b"li0ei0ei0ee"[..], 4), (b"l0:0:0:e", 4), (b"ldedee", 3)] {
            assert_eq!(count_nodes(&decode(dense).unwrap()), nodes);
            assert!(nodes <= node_budget(dense.len()), "{dense:?}");
        }

        // The check itself, reached by shrinking the budget under a valid
        // input — the grammar cannot exceed the real one.
        let mut d = Decoder::new(b"llelee", &[]);
        d.budget = 2;
        assert_eq!(d.value(0).unwrap_err().kind, ErrorKind::TooManyNodes);
        let mut d = Decoder::new(b"llelee", &[]);
        d.budget = 3;
        assert!(d.value(0).is_ok());
    }

    #[test]
    fn value_accessors() {
        let v = decode(b"d3:key5:value3:numi7e4:listl1:aee").unwrap();
        assert_eq!(v.get(b"key").and_then(Value::as_str), Some("value"));
        assert_eq!(v.get(b"num").and_then(Value::as_int), Some(7));
        assert_eq!(
            v.get(b"list").and_then(Value::as_list).map(<[Value]>::len),
            Some(1)
        );
        assert_eq!(v.get(b"absent"), None);
        assert_eq!(bytes("x").as_int(), None);
    }
}
