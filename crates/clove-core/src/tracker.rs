//! HTTP tracker announces over I2P (BEP 3 tracker protocol, I2P dialect).
//!
//! An announce is an HTTP GET to the tracker's I2P destination. This module
//! is split so everything but the actual byte transport is pure and tested:
//!
//! - [`build_announce`] turns an announce URL + [`AnnounceParams`] into an
//!   [`http::Request`]. The I2P specifics (SCOPE §3, `docs/PROTOCOL.i2p-bt`):
//!   no real port, and the client's own destination is carried as the `ip`
//!   query parameter (base64) since there is no IP.
//! - [`parse_response`] decodes the bencoded reply. The compact peer list is
//!   the I2P form — concatenated 32-byte destination hashes, no port — not
//!   clearnet's 6-byte ip:port.
//! - [`AnnounceState`] is the timing state machine: honor the tracker's
//!   interval, back off on failure, and know which event to send next.
//!
//! The single I/O function [`announce_over`] wires the above to any
//! `Read + Write` stream (an `i2pnet` stream in production). Live-tracker
//! verification is M3; the conventions here are `[assumed]` in PROTOCOL
//! until then.

use std::fmt::Write as _;
use std::time::Duration;

use i2pnet::DestHash;

use crate::bencode::{self, Value};
use crate::http;

/// Default peers to request per announce.
pub const DEFAULT_NUMWANT: u32 = 200;

/// Minimum interval clove will wait between announces regardless of what a
/// tracker asks for, to avoid hammering (a floor, not the tracker's own
/// `min interval`).
pub const MIN_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);

/// The event an announce carries (BEP 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// First announce to this tracker for this torrent.
    Started,
    /// Graceful departure.
    Stopped,
    /// Download just completed.
    Completed,
    /// A periodic keep-alive announce (no `event` key).
    Periodic,
}

impl Event {
    /// The `event` query value, or `None` for a periodic announce.
    #[must_use]
    pub fn query_value(self) -> Option<&'static str> {
        match self {
            Event::Started => Some("started"),
            Event::Stopped => Some("stopped"),
            Event::Completed => Some("completed"),
            Event::Periodic => None,
        }
    }
}

/// Everything an announce reports about our state.
pub struct AnnounceParams<'a> {
    /// The torrent's 20-byte info-hash.
    pub info_hash: [u8; 20],
    /// Our 20-byte peer id.
    pub peer_id: [u8; 20],
    /// Bytes uploaded so far.
    pub uploaded: u64,
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Bytes still needed (0 when seeding).
    pub left: u64,
    /// The event to report.
    pub event: Event,
    /// Peers requested.
    pub numwant: u32,
    /// Our own I2P destination, base64 — the I2P stand-in for an IP.
    pub our_dest_b64: &'a str,
}

/// A tracker's announce URL, split into the pieces a request needs.
struct AnnounceUrl<'a> {
    host: &'a str,
    path_and_query: &'a str,
}

/// Parse an `http://host.i2p/announce[?...]` URL.
fn split_url(url: &str) -> Option<AnnounceUrl<'_>> {
    let rest = url.strip_prefix("http://")?;
    match rest.find('/') {
        Some(i) => Some(AnnounceUrl {
            host: &rest[..i],
            path_and_query: &rest[i..],
        }),
        None => Some(AnnounceUrl {
            host: rest,
            path_and_query: "/",
        }),
    }
}

/// Build the announce request for `url`. Returns the host (for dialing) and
/// the encoded HTTP request bytes.
///
/// # Errors
///
/// The URL is not a parseable `http://…i2p/…` announce URL.
pub fn build_announce(url: &str, params: &AnnounceParams) -> Result<(String, Vec<u8>), Error> {
    // Second line of defence behind `metainfo`'s filter, and the reason the two
    // agree exactly: a URL that reaches here from anywhere else (a resume file
    // written by an older clove, an operator's edit) is refused rather than
    // handed to a naming lookup.
    if !crate::metainfo::is_i2p_tracker(url) {
        return Err(Error::BadUrl);
    }
    let parsed = split_url(url).ok_or(Error::BadUrl)?;
    // Preserve any query already in the announce path (e.g. postman's
    // /announce.php?...), then append ours with the right separator.
    let sep = if parsed.path_and_query.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut target = String::from(parsed.path_and_query);
    target.push(sep);
    // Writing to a String is infallible; the result is intentionally ignored.
    let _ = write!(
        target,
        "info_hash={}&peer_id={}&port=6881&uploaded={}&downloaded={}&left={}&numwant={}&compact=1&ip={}",
        http::percent_encode(&params.info_hash),
        http::percent_encode(&params.peer_id),
        params.uploaded,
        params.downloaded,
        params.left,
        params.numwant,
        http::percent_encode(params.our_dest_b64.as_bytes()),
    );
    if let Some(event) = params.event.query_value() {
        target.push_str("&event=");
        target.push_str(event);
    }
    let request = http::Request {
        method: "GET",
        target: &target,
        host: parsed.host,
        headers: &[],
        body: &[],
    };
    Ok((parsed.host.to_owned(), request.encode()))
}

/// A tracker's decoded reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnounceResponse {
    /// Seconds the tracker asks us to wait before the next announce.
    pub interval: u32,
    /// The tracker's `min interval`, if given.
    pub min_interval: Option<u32>,
    /// Peers to try, as destination hashes.
    pub peers: Vec<DestHash>,
}

/// Largest announce response body clove will accept (numwant peers * 32
/// bytes, plus generous bencode overhead).
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Parse a bencoded announce response body.
///
/// # Errors
///
/// Malformed bencode, a tracker `failure reason`, a missing/short interval,
/// or a compact peers string whose length is not a multiple of 32.
pub fn parse_response(body: &[u8]) -> Result<AnnounceResponse, Error> {
    // The evidence travels with the complaint. "not bencode" on its own is a
    // true statement about a chunk-framing header, an HTML error page and an
    // empty body alike, and telling them apart cost a live session: every
    // announce failed identically and the reason was one `1a4` away from
    // obvious. `NotBencode` carries a bounded, printable prefix of whatever
    // did arrive.
    let root = bencode::decode(body).map_err(|_| Error::NotBencode {
        content_type: None,
        len: body.len(),
        preview: preview(body),
    })?;
    if let Some(reason) = root.get(b"failure reason").and_then(Value::as_str) {
        return Err(Error::TrackerFailure(reason.to_owned()));
    }
    let interval = root
        .get(b"interval")
        .and_then(Value::as_int)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(Error::BadResponse("missing interval"))?;
    let min_interval = root
        .get(b"min interval")
        .and_then(Value::as_int)
        .and_then(|n| u32::try_from(n).ok());

    let peers = match root.get(b"peers") {
        Some(Value::Bytes(bytes)) => parse_compact_peers(bytes)?,
        Some(Value::List(list)) => parse_dict_peers(list)?,
        Some(_) => return Err(Error::BadResponse("peers has an unexpected type")),
        None => Vec::new(),
    };
    Ok(AnnounceResponse {
        interval,
        min_interval,
        peers,
    })
}

/// I2P compact peers: concatenated 32-byte destination hashes, no port.
fn parse_compact_peers(bytes: &[u8]) -> Result<Vec<DestHash>, Error> {
    if !bytes.len().is_multiple_of(32) {
        return Err(Error::BadResponse("compact peers not a multiple of 32"));
    }
    Ok(bytes
        .chunks_exact(32)
        .map(|c| {
            let mut h = [0u8; 32];
            h.copy_from_slice(c);
            DestHash(h)
        })
        .collect())
}

/// Non-compact peers: a list of dicts, each carrying a full base64
/// destination under `ip` (the I2P convention).
fn parse_dict_peers(list: &[Value]) -> Result<Vec<DestHash>, Error> {
    let mut peers = Vec::with_capacity(list.len());
    for entry in list {
        let dest = entry
            .get(b"ip")
            .and_then(Value::as_str)
            .ok_or(Error::BadResponse("peer dict without ip"))?;
        if let Some(hash) = DestHash::from_b64_destination(dest) {
            peers.push(hash);
        }
    }
    Ok(peers)
}

/// Timing state machine for one (torrent, tracker) pair.
///
/// Tracks when the next announce is due and what event it carries, honoring
/// the tracker's interval on success and backing off on failure. Time is
/// injected (`now` seconds from an arbitrary epoch) so it is deterministic
/// under test and needs no clock here.
pub struct AnnounceState {
    /// Absolute time (secs) the next announce is due.
    next_due: u64,
    /// Whether the initial `started` announce has been sent.
    started: bool,
    /// Whether `completed` has already been reported to this tracker.
    completed: bool,
    /// Consecutive failures, for backoff.
    failures: u32,
    /// Backoff ceiling (secs).
    max_backoff: u64,
}

impl Default for AnnounceState {
    fn default() -> Self {
        AnnounceState {
            next_due: 0,
            started: false,
            completed: false,
            failures: 0,
            max_backoff: 30 * 60,
        }
    }
}

impl AnnounceState {
    /// A fresh state; the first announce is due immediately.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether an announce is due at `now` (seconds).
    #[must_use]
    pub fn due(&self, now: u64) -> bool {
        now >= self.next_due
    }

    /// Make an announce due immediately, bypassing both the tracker's
    /// interval and any failure backoff.
    ///
    /// This exists for one caller: an operator asking for a re-announce
    /// (`clove announce`). Nothing automatic may use it — the interval a
    /// tracker hands back is an instruction, and the whole point of
    /// [`on_success`](Self::on_success) is to obey it.
    pub fn make_due(&mut self) {
        self.next_due = 0;
    }

    /// The event the next announce should carry, given whether the torrent
    /// is now complete.
    ///
    /// `completed` is reported once. It is an event, not a state: a tracker
    /// counts a snatch every time it sees one, so repeating it on every
    /// periodic announce while seeding inflates the tracker's numbers for as
    /// long as the torrent is up.
    #[must_use]
    pub fn next_event(&self, complete: bool) -> Event {
        if !self.started {
            Event::Started
        } else if complete && !self.completed {
            Event::Completed
        } else {
            Event::Periodic
        }
    }

    /// Record a successful announce of `sent`: schedule the next at
    /// `now + interval` (floored at [`MIN_ANNOUNCE_INTERVAL`]) and clear the
    /// backoff.
    ///
    /// The event is a parameter because the state machine has to know what
    /// actually went out — that is how `completed` stops being sent again.
    pub fn on_success(&mut self, now: u64, interval: u32, sent: Event) {
        self.started = true;
        if sent == Event::Completed {
            self.completed = true;
        }
        self.failures = 0;
        let floor = MIN_ANNOUNCE_INTERVAL.as_secs();
        self.next_due = now.saturating_add(u64::from(interval).max(floor));
    }

    /// Record a failed announce: exponential backoff from 30s, capped.
    pub fn on_failure(&mut self, now: u64) {
        self.failures = self.failures.saturating_add(1);
        let base = 30u64;
        let shift = (self.failures - 1).min(6);
        let delay = base.saturating_mul(1u64 << shift).min(self.max_backoff);
        self.next_due = now.saturating_add(delay);
    }
}

/// Perform one announce over an already-connected stream to the tracker:
/// write the request, read and parse the response.
///
/// # Errors
///
/// I/O failure, an HTTP-level error, a non-200 status, or a tracker/parse
/// error from [`parse_response`].
pub fn announce_over<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    request: &[u8],
) -> Result<AnnounceResponse, Error> {
    stream.write_all(request).map_err(Error::Io)?;
    let response = http::read_response(stream, MAX_RESPONSE_BYTES).map_err(Error::Http)?;
    if response.status != 200 {
        return Err(Error::HttpStatus(response.status));
    }
    // `parse_response` is pure and sees only the body, so the content type is
    // attached here — it is half the diagnosis when a tracker answers 200
    // with something that is not an announce at all.
    parse_response(&response.body).map_err(|e| match e {
        Error::NotBencode { len, preview, .. } => Error::NotBencode {
            content_type: response.header("content-type").map(ToOwned::to_owned),
            len,
            preview,
        },
        other => other,
    })
}

/// Why an announce failed.
#[derive(Debug)]
pub enum Error {
    /// The announce URL was not a parseable I2P HTTP URL.
    BadUrl,
    /// Underlying I/O error.
    Io(std::io::Error),
    /// HTTP transport/parse error.
    Http(http::Error),
    /// Tracker returned a non-200 status.
    HttpStatus(u16),
    /// The tracker replied with a `failure reason`.
    TrackerFailure(String),
    /// The bencoded response was malformed.
    BadResponse(&'static str),
    /// The body was not bencode at all. Carries enough of the response to
    /// identify it without a second live run: what the tracker said it was,
    /// how much of it there was, and a printable prefix.
    ///
    /// The prefix alone was not enough. A run reported 96 characters of
    /// `<!DOCTYPE html>…<style type="text/css">…` — plainly a web page, and
    /// plainly *not* an announce, but with the `<title>` still beyond the cut
    /// there was no telling whose page it was: the tracker's own error page,
    /// a different site behind a stale address-book entry, or a router
    /// console. Those have three different fixes. The content type and a
    /// longer prefix settle it in the same log line.
    NotBencode {
        /// The `Content-Type` the tracker sent, if any.
        content_type: Option<String>,
        /// Full body length, which the preview may not show all of.
        len: usize,
        /// Printable prefix, bounded by [`PREVIEW_LEN`].
        preview: String,
    },
}

/// Longest response prefix carried in a [`Error::NotBencode`].
///
/// Long enough to reach the `<title>` of a page whose `<head>` opens with
/// inline CSS, which is what it took to identify the last one.
const PREVIEW_LEN: usize = 512;

/// A bounded, single-line, printable rendering of a response body, for an
/// error message an operator reads in a log.
///
/// Non-printable bytes become `.` rather than being escaped: this is a hint
/// about what kind of thing arrived, not a hex dump, and a log line full of
/// `\x1b` would be both longer and less readable. Control characters never
/// reach the terminal, which matters when the bytes came off the network.
fn preview(body: &[u8]) -> String {
    if body.is_empty() {
        return "<empty body>".to_owned();
    }
    let mut out: String = body
        .iter()
        .take(PREVIEW_LEN)
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    if body.len() > PREVIEW_LEN {
        out.push('…');
    }
    out
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::BadUrl => write!(f, "tracker: unparseable announce URL"),
            Error::Io(e) => write!(f, "tracker: {e}"),
            Error::Http(e) => write!(f, "tracker: {e}"),
            Error::HttpStatus(s) => write!(f, "tracker: HTTP status {s}"),
            Error::TrackerFailure(r) => write!(f, "tracker refused: {r}"),
            Error::BadResponse(w) => write!(f, "tracker: malformed response: {w}"),
            Error::NotBencode {
                content_type,
                len,
                preview,
            } => write!(
                f,
                "tracker: response is not bencode ({} bytes, content-type {}); it begins {preview:?}",
                len,
                content_type.as_deref().unwrap_or("unset"),
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::encode;
    use std::collections::BTreeMap;

    fn params() -> AnnounceParams<'static> {
        AnnounceParams {
            info_hash: [0xAB; 20],
            peer_id: *b"-CV0001-abcdefghijkl",
            uploaded: 100,
            downloaded: 200,
            left: 300,
            event: Event::Started,
            numwant: DEFAULT_NUMWANT,
            our_dest_b64: "MYDESTb64",
        }
    }

    #[test]
    fn builds_announce_preserving_existing_query() {
        let (host, req) =
            build_announce("http://tracker.postman.i2p/announce.php?x=1", &params()).unwrap();
        assert_eq!(host, "tracker.postman.i2p");
        let s = String::from_utf8(req).unwrap();
        assert!(s.starts_with("GET /announce.php?x=1&info_hash="));
        assert!(s.contains("&compact=1"));
        assert!(s.contains("&ip=MYDESTb64"));
        assert!(s.contains("&event=started"));
        assert!(s.contains("Host: tracker.postman.i2p\r\n"));
        // info_hash of 0xAB bytes percent-encodes to %AB repeated.
        assert!(s.contains(&format!("info_hash={}", "%AB".repeat(20))));
    }

    #[test]
    fn periodic_event_has_no_event_param() {
        let mut p = params();
        p.event = Event::Periodic;
        let (_, req) = build_announce("http://t.i2p/a", &p).unwrap();
        let s = String::from_utf8(req).unwrap();
        assert!(!s.contains("event="));
        assert!(s.starts_with("GET /a?info_hash="));
    }

    #[test]
    fn rejects_non_i2p_url() {
        assert!(matches!(
            build_announce("udp://t.i2p/a", &params()),
            Err(Error::BadUrl)
        ));
    }

    fn dict(entries: Vec<(&str, Value)>) -> Value {
        Value::Dict(
            entries
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn parses_compact_response() {
        let mut peers = Vec::new();
        peers.extend_from_slice(&[0x11; 32]);
        peers.extend_from_slice(&[0x22; 32]);
        let body = encode(&dict(vec![
            ("interval", Value::Int(1800)),
            ("min interval", Value::Int(900)),
            ("peers", Value::Bytes(peers)),
        ]));
        let resp = parse_response(&body).unwrap();
        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.min_interval, Some(900));
        assert_eq!(resp.peers, vec![DestHash([0x11; 32]), DestHash([0x22; 32])]);
    }

    #[test]
    fn rejects_misaligned_compact_peers() {
        let body = encode(&dict(vec![
            ("interval", Value::Int(60)),
            ("peers", Value::Bytes(vec![0u8; 33])),
        ]));
        assert!(matches!(parse_response(&body), Err(Error::BadResponse(_))));
    }

    #[test]
    fn surfaces_tracker_failure() {
        let body = encode(&dict(vec![(
            "failure reason",
            Value::Bytes(b"torrent not registered".to_vec()),
        )]));
        match parse_response(&body) {
            Err(Error::TrackerFailure(r)) => assert_eq!(r, "torrent not registered"),
            other => panic!("expected TrackerFailure, got {other:?}"),
        }
    }

    #[test]
    fn empty_peers_is_valid() {
        let body = encode(&dict(vec![("interval", Value::Int(1800))]));
        let resp = parse_response(&body).unwrap();
        assert!(resp.peers.is_empty());
    }

    #[test]
    fn state_machine_timing_and_events() {
        let mut st = AnnounceState::new();
        assert!(st.due(0));
        assert_eq!(st.next_event(false), Event::Started);

        // Success at t=0 with a 1800s interval -> next due at 1800.
        st.on_success(0, 1800, Event::Started);
        assert!(!st.due(1799));
        assert!(st.due(1800));
        assert_eq!(st.next_event(false), Event::Periodic);
        assert_eq!(st.next_event(true), Event::Completed);

        // A tiny interval is floored to MIN_ANNOUNCE_INTERVAL.
        st.on_success(2000, 5, Event::Periodic);
        assert!(!st.due(2000 + 59));
        assert!(st.due(2000 + 60));
    }

    #[test]
    fn completed_is_reported_once_and_only_once() {
        // A tracker counts a snatch per `completed` event, so a seeding torrent
        // that reports it on every periodic announce inflates the count for as
        // long as it stays up.
        let mut st = AnnounceState::new();
        assert_eq!(st.next_event(false), Event::Started);
        st.on_success(0, 60, Event::Started);

        assert_eq!(st.next_event(true), Event::Completed);
        st.on_success(60, 60, Event::Completed);
        assert_eq!(
            st.next_event(true),
            Event::Periodic,
            "completed was reported twice"
        );
        st.on_success(120, 60, Event::Periodic);
        assert_eq!(st.next_event(true), Event::Periodic);

        // A torrent that completes later still gets its one report.
        let mut later = AnnounceState::new();
        later.on_success(0, 60, Event::Started);
        assert_eq!(later.next_event(false), Event::Periodic);
        later.on_success(60, 60, Event::Periodic);
        assert_eq!(later.next_event(true), Event::Completed);
    }

    #[test]
    fn build_announce_refuses_what_the_filter_drops() {
        // The property that keeps the two in step: anything metainfo keeps,
        // the announcer can build; anything it drops, the announcer refuses.
        for url in [
            "http://tracker.postman.i2p/announce.php",
            "http://opentracker.dg2.i2p:80/announce",
            "http://x.b32.i2p/a?x=1",
        ] {
            assert!(crate::metainfo::is_i2p_tracker(url), "{url}");
            build_announce(url, &params()).expect(url);
        }
        for url in [
            "https://tracker.example.i2p/announce",
            "http://tracker.example.org/announce",
            "udp://tracker.example.i2p/announce",
            "http://1.2.3.4:6969/announce",
            "http://evil@host.i2p/announce",
            "not a url",
        ] {
            assert!(!crate::metainfo::is_i2p_tracker(url), "{url}");
            assert!(
                matches!(build_announce(url, &params()), Err(Error::BadUrl)),
                "{url} was built into an announce"
            );
        }
    }

    #[test]
    fn failure_backoff_grows_and_caps() {
        let mut st = AnnounceState::new();
        st.on_failure(0);
        assert!(st.due(30) && !st.due(29)); // 30s
        st.on_failure(30);
        assert!(st.due(30 + 60) && !st.due(30 + 59)); // 60s
        // Many failures cap at max_backoff (30 min).
        for t in 0..20 {
            st.on_failure(t);
        }
        assert!(st.due(20 + 30 * 60));
    }

    /// Duplex stub tracker: reads return a canned response, writes are sunk.
    struct Stub {
        to_read: std::io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }
    impl std::io::Read for Stub {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.to_read.read(buf)
        }
    }
    impl std::io::Write for Stub {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The live failure, end to end: postman answers chunked, and every
    /// announce came back "not bencode" with zero peers. The announce path —
    /// not just the HTTP reader — has to survive it.
    #[test]
    fn announce_over_a_chunked_stream() {
        let mut peers = Vec::new();
        peers.extend_from_slice(&[0x77; 32]);
        peers.extend_from_slice(&[0x88; 32]);
        let body = encode(&dict(vec![
            ("interval", Value::Int(1800)),
            ("peers", Value::Bytes(peers)),
        ]));

        // Two chunks, split at an arbitrary byte, as a webserver would.
        let split = body.len() / 2;
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for part in [&body[..split], &body[split..]] {
            raw.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            raw.extend_from_slice(part);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");

        let (_host, request) = build_announce("http://t.i2p/announce", &params()).unwrap();
        let mut stub = Stub {
            to_read: std::io::Cursor::new(raw),
            written: Vec::new(),
        };
        let response = announce_over(&mut stub, &request).expect("chunked announce");
        assert_eq!(response.interval, 1800);
        assert_eq!(response.peers.len(), 2);
    }

    /// A body that is not bencode must arrive with the evidence attached. The
    /// live run reported "not bencode" for a fortnight of runs and the answer
    /// was in the first three bytes nobody could see.
    #[test]
    fn a_non_bencode_body_reports_what_it_actually_was() {
        let html = b"<html><head><title>500 Internal Server Error</title></head>";
        let Err(e) = parse_response(html) else {
            panic!("HTML is not a valid announce response");
        };
        let text = e.to_string();
        assert!(
            text.contains("<html>") && text.contains("500"),
            "the error must quote the body, got: {text}"
        );

        // The half a 96-character preview could not give: a tracker answering
        // 200 with a web page says so in its content type, and that is what
        // separates "the tracker is broken" from "this is not the tracker".
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 6\r\n\r\n<html>";
        let (_host, request) = build_announce("http://t.i2p/announce", &params()).unwrap();
        let mut stub = Stub {
            to_read: std::io::Cursor::new(raw.to_vec()),
            written: Vec::new(),
        };
        let Err(served_a_page) = announce_over(&mut stub, &request) else {
            panic!("a web page is not an announce response");
        };
        assert!(
            served_a_page.to_string().contains("text/html"),
            "the content type must reach the operator: {served_a_page}"
        );

        // Bounded, and control bytes never reach the terminal.
        let Err(long) = parse_response(&[0x1b; 4096]) else {
            panic!("escape bytes are not a valid announce response");
        };
        let text = long.to_string();
        assert!(!text.contains('\u{1b}'), "control bytes must be scrubbed");
        // Bounded by PREVIEW_LEN, not by a number written twice: the preview
        // grew from 96 to 512 on purpose when 96 proved too short to identify
        // a page, and a hardcoded ceiling here would have made that a test
        // failure rather than a decision.
        assert!(
            text.len() < PREVIEW_LEN + 128,
            "the preview must stay bounded: {} chars",
            text.len()
        );

        // An empty body is its own diagnosis and says so.
        let Err(empty) = parse_response(b"") else {
            panic!("an empty body is not a valid announce response");
        };
        assert!(empty.to_string().contains("empty"));
    }

    #[test]
    fn announce_over_a_stream() {
        // A canned tracker: responds to any request with a compact reply.
        let mut peers = Vec::new();
        peers.extend_from_slice(&[0x77; 32]);
        let resp_body = encode(&dict(vec![
            ("interval", Value::Int(1800)),
            ("peers", Value::Bytes(peers)),
        ]));
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            resp_body.len()
        )
        .into_bytes();
        raw.extend_from_slice(&resp_body);

        let (_host, request) = build_announce("http://t.i2p/announce", &params()).unwrap();
        let mut stub = Stub {
            to_read: std::io::Cursor::new(raw),
            written: Vec::new(),
        };
        let resp = announce_over(&mut stub, &request).unwrap();
        assert_eq!(resp.peers, vec![DestHash([0x77; 32])]);
        assert!(stub.written.starts_with(b"GET /announce?"));
    }
}
