//! Minimal hand-rolled HTTP/1.1 (Q6, `docs/DECISIONS.md`).
//!
//! Just the subset clove needs, hostile-input hardened. Used first by the
//! tracker client (announces are HTTP GETs over an I2P stream); the local
//! API server (Phase F) reuses the same request/response primitives, so
//! this stays transport-agnostic — it encodes to and parses from any
//! `Read`/`Write`, an `i2pnet` stream in production, a cursor in tests.
//!
//! No chunked transfer encoding, no keep-alive reuse: clove opens a stream,
//! sends one request, reads one `Content-Length`-delimited (or
//! close-delimited) response, done. Anything fancier a tracker sends is a
//! parse error, not a silent guess.

use std::io::{self, Read};

/// Largest header section clove will buffer before giving up — guards
/// against a peer/tracker that never sends the blank line.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// An outbound HTTP/1.1 request.
pub struct Request<'a> {
    /// Method, e.g. `GET`.
    pub method: &'a str,
    /// Request target, e.g. `/announce?info_hash=...`.
    pub target: &'a str,
    /// `Host` header value (the tracker's I2P hostname).
    pub host: &'a str,
    /// Extra headers as `(name, value)`.
    pub headers: &'a [(&'a str, &'a str)],
}

impl Request<'_> {
    /// Serialize to on-wire bytes. `Host`, `Connection: close`, and a
    /// zero-length `Content-Length` are always emitted; no body.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(self.method.as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.target.as_bytes());
        out.extend_from_slice(b" HTTP/1.1\r\n");
        out.extend_from_slice(b"Host: ");
        out.extend_from_slice(self.host.as_bytes());
        out.extend_from_slice(b"\r\n");
        for (name, value) in self.headers {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"Connection: close\r\n\r\n");
        out
    }
}

/// A parsed HTTP response.
#[derive(Clone, Debug)]
pub struct Response {
    /// Status code (e.g. 200).
    pub status: u16,
    /// Response headers, lowercased names.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

impl Response {
    /// First header value matching `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a response could not be read.
#[derive(Debug)]
pub enum Error {
    /// Underlying I/O error.
    Io(io::Error),
    /// The response head was malformed or exceeded [`MAX_HEADER_BYTES`].
    BadResponse(&'static str),
    /// A `Content-Length` larger than the caller's `max_body`.
    BodyTooLarge,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "http: {e}"),
            Error::BadResponse(what) => write!(f, "http: malformed response: {what}"),
            Error::BodyTooLarge => write!(f, "http: response body exceeds the allowed size"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Read and parse one HTTP response from `reader`, capping the body at
/// `max_body` bytes.
///
/// # Errors
///
/// I/O errors, a malformed status line or headers, a header section past
/// [`MAX_HEADER_BYTES`], or a `Content-Length` over `max_body`.
pub fn read_response<R: Read>(reader: &mut R, max_body: usize) -> Result<Response, Error> {
    let (head, leftover) = read_head(reader)?;
    let text = std::str::from_utf8(&head).map_err(|_| Error::BadResponse("non-UTF-8 head"))?;
    let mut lines = text.split("\r\n");

    let status_line = lines.next().ok_or(Error::BadResponse("empty response"))?;
    let status = parse_status(status_line)?;

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(Error::BadResponse("header without a colon"))?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            let n: usize = value
                .parse()
                .map_err(|_| Error::BadResponse("bad content-length"))?;
            if n > max_body {
                return Err(Error::BodyTooLarge);
            }
            content_length = Some(n);
        }
        headers.push((name, value));
    }

    let body = read_body(reader, leftover, content_length, max_body)?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// Read up to and including the blank line terminating the head; returns the
/// head bytes (without the trailing CRLFCRLF) and any body bytes already
/// buffered past it.
fn read_head<R: Read>(reader: &mut R) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if buf.len() > MAX_HEADER_BYTES {
            return Err(Error::BadResponse("header section too large"));
        }
        let n = reader.read(&mut byte)?;
        if n == 0 {
            return Err(Error::BadResponse("connection closed before end of head"));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            buf.truncate(buf.len() - 4);
            return Ok((buf, Vec::new()));
        }
    }
}

fn read_body<R: Read>(
    reader: &mut R,
    mut body: Vec<u8>,
    content_length: Option<usize>,
    max_body: usize,
) -> Result<Vec<u8>, Error> {
    if let Some(len) = content_length {
        body.resize(len, 0);
        reader.read_exact(&mut body)?;
        return Ok(body);
    }
    // No Content-Length: read to close, but never past max_body.
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Ok(body);
        }
        if body.len() + n > max_body {
            return Err(Error::BodyTooLarge);
        }
        body.extend_from_slice(&chunk[..n]);
    }
}

fn parse_status(line: &str) -> Result<u16, Error> {
    // "HTTP/1.1 200 OK"
    let mut parts = line.split(' ');
    let version = parts.next().ok_or(Error::BadResponse("no version"))?;
    if !version.starts_with("HTTP/1.") {
        return Err(Error::BadResponse("not HTTP/1.x"));
    }
    let code = parts.next().ok_or(Error::BadResponse("no status code"))?;
    code.parse()
        .map_err(|_| Error::BadResponse("non-numeric status code"))
}

/// Percent-encode raw bytes for a URL query value (RFC 3986): unreserved
/// bytes pass through, everything else becomes `%XX`. Used for the raw
/// 20-byte `info_hash` and `peer_id` in tracker announces.
#[must_use]
pub fn percent_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[(b >> 4) as usize]));
            out.push(char::from(HEX[(b & 0x0f) as usize]));
        }
    }
    out
}

/// Percent-decode a URL-encoded string to raw bytes (`%XX` → byte). Other
/// characters, including `+`, pass through unchanged — magnet URIs use
/// `%20` for spaces, so treating `+` as space would corrupt names.
#[must_use]
pub fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn encodes_a_get() {
        let req = Request {
            method: "GET",
            target: "/announce?x=1",
            host: "tracker.i2p",
            headers: &[("User-Agent", "clove/0.1")],
        };
        let s = String::from_utf8(req.encode()).unwrap();
        assert_eq!(
            s,
            "GET /announce?x=1 HTTP/1.1\r\nHost: tracker.i2p\r\nUser-Agent: clove/0.1\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn reads_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nhello";
        let mut cur = Cursor::new(raw.to_vec());
        let resp = read_response(&mut cur, 1024).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
        assert_eq!(resp.header("content-type"), Some("text/plain"));
        assert_eq!(resp.header("CONTENT-TYPE"), Some("text/plain"));
    }

    #[test]
    fn reads_close_delimited_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: x\r\n\r\nbencoded-body-here";
        let mut cur = Cursor::new(raw.to_vec());
        let resp = read_response(&mut cur, 1024).unwrap();
        assert_eq!(resp.body, b"bencoded-body-here");
    }

    #[test]
    fn rejects_bad_and_oversized() {
        // Non-HTTP.
        let mut cur = Cursor::new(b"garbage\r\n\r\n".to_vec());
        assert!(matches!(
            read_response(&mut cur, 1024),
            Err(Error::BadResponse(_))
        ));

        // Content-Length beyond max_body.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 9999\r\n\r\n";
        let mut cur = Cursor::new(raw.to_vec());
        assert!(matches!(
            read_response(&mut cur, 16),
            Err(Error::BodyTooLarge)
        ));

        // Head with no terminator, hitting EOF.
        let mut cur = Cursor::new(b"HTTP/1.1 200 OK\r\nX: y".to_vec());
        assert!(matches!(
            read_response(&mut cur, 1024),
            Err(Error::BadResponse(_))
        ));
    }

    #[test]
    fn percent_encoding_matches_spec() {
        assert_eq!(percent_encode(b"AZaz09-_.~"), "AZaz09-_.~");
        assert_eq!(percent_encode(&[0x00, 0xFF, b' ', b'/']), "%00%FF%20%2F");
        // A realistic 20-byte info_hash of zeros.
        assert_eq!(percent_encode(&[0u8; 3]), "%00%00%00");
    }

    #[test]
    fn percent_decode_round_trips_and_passes_plus() {
        assert_eq!(percent_decode("hello%20world"), b"hello world");
        assert_eq!(percent_decode("a+b"), b"a+b"); // '+' is literal
        assert_eq!(
            percent_decode(&percent_encode(&[0u8, 0xFF, b'/'])),
            vec![0, 0xFF, b'/']
        );
        assert_eq!(percent_decode("%zz"), b"%zz"); // invalid escape passes through
    }
}
