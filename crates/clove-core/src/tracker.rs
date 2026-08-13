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
use std::io::Write as _;
use std::time::{Duration, Instant};

use i2pnet::DestHash;

use crate::bencode::{self, Value};
use crate::http;

/// What clove calls itself to trackers. Kept in step with the wire peer id
/// prefix (`-CV0001-`, Q7) so a tracker operator sees one name, not two.
pub const USER_AGENT: &str = concat!("clove/", env!("CARGO_PKG_VERSION"));

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

/// Build the announce request for `url`. Returns the host (for dialing) and
/// the encoded HTTP request bytes.
///
/// # Errors
///
/// The URL is not a parseable `http://…i2p/…` announce URL.
pub fn build_announce(url: &str, params: &AnnounceParams) -> Result<(String, Vec<u8>), Error> {
    // The same parse the filter uses, not a second one that agrees with it by
    // inspection. A URL can reach here from somewhere `metainfo` never saw — a
    // resume file written by an older clove, an operator's edit — so this is
    // still the second line of defence it always was; it is just no longer a
    // differently-shaped one.
    let parsed = crate::metainfo::TrackerUrl::parse(url).ok_or(Error::BadUrl)?;
    // Preserve any query already in the announce path (e.g. postman's
    // /announce.php?...), then append ours with the right separator.
    let sep = if parsed.path_and_query.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut target = String::from(&parsed.path_and_query);
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
    // Identify ourselves. An announce with no User-Agent at all is another
    // shape almost no real client sends, and trackers have historically been
    // choosy about it. It also means an operator reading a tracker's logs can
    // see which client is misbehaving, which is a courtesy we would want back.
    let request = http::Request {
        method: "GET",
        target: &target,
        // The authority as written, port and all — that is what a `Host` header
        // is. The *dial* uses the bare host: naming resolves a name, and a port
        // in it makes a lookup that cannot succeed.
        host: &parsed.authority,
        headers: &[("User-Agent", USER_AGENT)],
        body: &[],
    };
    // `encode` refuses a field that would break the message into more lines
    // than we wrote. Nothing that got past `TrackerUrl::parse` can, so this is
    // the second lock on the same door — and the one that stays shut if a
    // future caller builds a `Request` from somewhere else.
    let encoded = request.encode().ok_or(Error::BadUrl)?;
    Ok((parsed.host, encoded))
}

/// The URL an encoded announce request actually asks for, reassembled from
/// the bytes on the wire — with our own destination taken back out.
///
/// Taken from the request rather than rebuilt from the parameters, so what
/// gets logged is what was sent — the point is to hand an operator something
/// they can paste into a browser pointed at the same tracker and bisect by
/// deleting parameters. Three rounds of reasoning about which parameter a
/// tracker dislikes were worth less than one round of removing them one at a
/// time, and only the operator can run that test.
///
/// `ip` is the exception, and it is the one parameter an operator never needs
/// to bisect: it is *our full public destination*, and this URL is logged on
/// every failed announce — which any tracker can cause at will by accepting an
/// announce and refusing it. `SECURITY.md` calls the client's destination
/// reaching a log leak-class, the highest severity in the project, and that is
/// the right call: stderr goes to journals, log shippers, backups and bug
/// reports, and unlike the b32 in the line above it this is the complete
/// base64 destination. Redacted rather than dropped, so what is left still
/// says an `ip` was sent and where in the URL it sat.
#[must_use]
pub fn announced_url(host: &str, request: &[u8]) -> String {
    let line = request
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or_default();
    let target = std::str::from_utf8(line)
        .ok()
        .and_then(|l| l.split(' ').nth(1))
        .unwrap_or("/");
    format!("http://{host}{}", redact_ip_param(target))
}

/// Replace the value of the `ip` query parameter with a placeholder.
///
/// Splits on the separators a query is built from rather than searching for
/// `ip=`, so a *different* parameter ending in those two letters keeps its
/// value and an `ip` anywhere in the query loses its own.
fn redact_ip_param(target: &str) -> String {
    let Some((path, query)) = target.split_once('?') else {
        return target.to_owned();
    };
    let mut out = String::with_capacity(target.len());
    out.push_str(path);
    for (i, field) in query.split('&').enumerate() {
        out.push(if i == 0 { '?' } else { '&' });
        match field.split_once('=') {
            Some(("ip", _)) => out.push_str("ip=<redacted>"),
            _ => out.push_str(field),
        }
    }
    out
}

/// Longest tracker-supplied message clove will repeat back.
const MAX_TRACKER_TEXT: usize = 512;

/// Make tracker-supplied text safe to put in a log line or an API field.
///
/// The text is a stranger's, and it ends up in the daemon's stderr and in
/// `clove list`. See [`crate::text`] for what is replaced and why; the bound is
/// here because a tracker does not get to decide how much of a log it occupies.
fn sanitise(text: &str) -> String {
    crate::text::scrub_bounded(text, MAX_TRACKER_TEXT)
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
        return Err(Error::TrackerFailure(sanitise(reason)));
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

    /// A state for a torrent that was **already complete** when this announcer
    /// started — a resumed seed, or one re-attached after a session rebuild.
    ///
    /// `completed` reports a download, so a client that already held the file
    /// before it announced at all has nothing to report. Without this the
    /// second announce of every restarted seed carries one, and the tracker
    /// counts a snatch that never happened — once per restart, and clove
    /// rebuilds its session tree whenever the router blips.
    #[must_use]
    pub fn already_complete() -> Self {
        AnnounceState {
            completed: true,
            ..Self::default()
        }
    }

    /// Whether an announce is due at `now` (seconds).
    #[must_use]
    pub fn due(&self, now: u64) -> bool {
        now >= self.next_due
    }

    /// Bring forward the one announce that is owed on an event rather than on
    /// a clock: the `completed` that BEP 3 wants when a download finishes.
    ///
    /// The only bypass of the tracker's own interval there is, and deliberately
    /// narrow. The interval a tracker hands back is an instruction, and the
    /// whole point of [`on_success`](Self::on_success) is to obey it.
    /// `completed` is an event, in the same
    /// class as `started` (the first announce, which no interval governs) and
    /// `stopped` (sent on teardown regardless of one) — not a periodic report
    /// whose cadence the tracker gets to set. Waiting for the next interval to
    /// mention it means the swarm does not learn there is a new seed for up to
    /// half an hour, which live cost a whole seeding window: one announce went
    /// out, as a leecher holding nothing, and nothing after it.
    ///
    /// Safe against misuse by construction rather than by discipline. It does
    /// nothing before the first announce (there is no `completed` without a
    /// `started`) and nothing after the completion announce has gone out, so
    /// it can bring forward exactly one announce in a torrent's lifetime and
    /// cannot be turned into a way to hammer a tracker.
    pub fn completion_due(&mut self) {
        if self.started && !self.completed {
            self.next_due = 0;
        }
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

/// How long a whole announce may take, first byte written to last byte read.
///
/// The socket timeout callers set ([`ANNOUNCE_IO_TIMEOUT`]) bounds any *single*
/// read or write; this bounds their sum, which is the part a tracker can
/// otherwise stretch without limit. One byte every few seconds resets a socket
/// timeout forever and never finishes an announce — a stall indistinguishable
/// from a slow tunnel, except that it never ends.
pub const ANNOUNCE_DEADLINE: Duration = Duration::from_secs(120);

/// What callers should set as the per-read/write socket bound before handing a
/// stream to [`announce_over`], so a single blocking call cannot outlast
/// [`ANNOUNCE_DEADLINE`] by more than this much.
pub const ANNOUNCE_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// An I/O stream that refuses to continue past an instant.
///
/// Every read and write checks the clock first, so a deadline set once covers
/// a whole multi-call exchange — including the reads inside
/// [`http::read_response`], which is where a tracker gets to decide how long
/// we wait.
struct Deadline<'a, S> {
    inner: &'a mut S,
    until: Instant,
}

impl<S> Deadline<'_, S> {
    fn check(&self) -> std::io::Result<()> {
        if Instant::now() >= self.until {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the tracker did not finish the exchange in time",
            ));
        }
        Ok(())
    }
}

impl<S: std::io::Read> std::io::Read for Deadline<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.check()?;
        self.inner.read(buf)
    }
}

impl<S: std::io::Write> std::io::Write for Deadline<'_, S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.check()?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Perform one announce over an already-connected stream to the tracker:
/// write the request, read and parse the response.
///
/// Bounded by [`ANNOUNCE_DEADLINE`] end to end. Callers should also set
/// [`ANNOUNCE_IO_TIMEOUT`] on the stream: this deadline is only consulted
/// between reads, so without a socket timeout a tracker that accepts the
/// connection and then says nothing at all still parks the thread on the
/// first one.
///
/// # Errors
///
/// I/O failure, the deadline passing, an HTTP-level error, a non-200 status,
/// or a tracker/parse error from [`parse_response`].
pub fn announce_over<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    request: &[u8],
) -> Result<AnnounceResponse, Error> {
    announce_over_until(stream, request, Instant::now() + ANNOUNCE_DEADLINE)
}

/// [`announce_over`] against a caller-chosen deadline, so a test can use one
/// it can afford to wait for.
fn announce_over_until<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    request: &[u8],
    until: Instant,
) -> Result<AnnounceResponse, Error> {
    let mut stream = Deadline {
        inner: stream,
        until,
    };
    stream.write_all(request).map_err(Error::Io)?;
    let response = http::read_response(&mut stream, MAX_RESPONSE_BYTES).map_err(Error::Http)?;
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
        /// Printable prefix, bounded by `PREVIEW_LEN` (512 bytes).
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
/// **HTML is summarised rather than quoted.** A raw prefix of a modern page is
/// its inline stylesheet, and two live runs proved it: 96 bytes of `:root{`
/// said nothing, and 512 bytes of the same said nothing at greater length,
/// while the one useful token — the `<title>` — sat past the cut both times. A
/// page is identified by its title and its words, so those are what come out.
fn preview(body: &[u8]) -> String {
    if body.is_empty() {
        return "<empty body>".to_owned();
    }
    if looks_like_html(body)
        && let Ok(text) = std::str::from_utf8(body)
    {
        return summarise_html(text);
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

/// Whether a body opens like markup, ignoring leading whitespace.
fn looks_like_html(body: &[u8]) -> bool {
    let start: Vec<u8> = body
        .iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace)
        .take(16)
        .collect();
    start.starts_with(b"<!DOCTYPE")
        || start.starts_with(b"<!doctype")
        || start.starts_with(b"<html")
        || start.starts_with(b"<HTML")
}

/// A page's `<title>` and its first visible words — what a person would say if
/// asked "what page is this?".
fn summarise_html(text: &str) -> String {
    let mut out = String::from("HTML page");
    if let Some(title) = between(text, "<title", "</title>")
        && let Some((_, inner)) = title.split_once('>')
    {
        let title = collapse(inner);
        if !title.is_empty() {
            let _ = write!(out, " titled {title:?}");
        }
    }
    let words = collapse(&strip_markup(text));
    if !words.is_empty() {
        let shown: String = words.chars().take(PREVIEW_LEN / 2).collect();
        let _ = write!(out, "; text begins {shown:?}");
    }
    out
}

/// The slice from the first `open` up to the following `close`.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)?;
    let rest = &text[start..];
    let end = rest.find(close)?;
    Some(&rest[..end])
}

/// Drop tags, and the contents of `<head>`/`<style>`/`<script>` with them:
/// those are the bulk of a modern page and none of its meaning.
fn strip_markup(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let skip_to = if opens_tag(rest, "head") {
            "</head>"
        } else if opens_tag(rest, "style") {
            "</style>"
        } else if opens_tag(rest, "script") {
            "</script>"
        } else {
            ">"
        };
        match rest.find(skip_to) {
            Some(i) => rest = &rest[i + skip_to.len()..],
            // An unterminated tag ends the useful text; return what we have
            // rather than emitting the remains of a stylesheet.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Whether `rest` opens the named tag — `<style>` or `<style type=…>`, but not
/// `<styleish>`.
fn opens_tag(rest: &str, name: &str) -> bool {
    let after = &rest[1.min(rest.len())..];
    after.len() > name.len()
        && after.is_char_boundary(name.len())
        && after[..name.len()].eq_ignore_ascii_case(name)
        && after[name.len()..].starts_with([' ', '>', '\t', '\n', '\r'])
}

/// Collapse whitespace runs and trim, so a page's text is one readable line.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
            numwant: 50,
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
            // The shapes the two parsers used to read differently. Each was
            // kept by the filter and then mangled by the builder: a query with
            // no path became part of the hostname, as did a fragment, and a
            // port reached naming lookup glued to the name.
            "http://tracker.i2p#frag/announce",
            "http://tracker.i2p/announce#frag",
            "http://tracker.i2p:/announce",
            "http://tracker.i2p:0/announce",
            "http://tracker.i2p:99999/announce",
            "http://tracker.i2p:80x/announce",
            "http://.i2p/announce",
            "http://a..i2p/announce",
            // Control characters anywhere: the path is written into a request
            // line verbatim and into a log line after that.
            "http://tracker.i2p/announce\r\nX-Evil: 1",
            "http://tracker.i2p/announce\nX-Evil: 1",
            "http://track\rer.i2p/announce",
            "http://tracker.i2p/ann ounce",
            // A percent escape that is not one.
            "http://tracker.i2p/announce?x=%zz",
            "http://tracker.i2p/announce?x=%4",
        ] {
            assert!(!crate::metainfo::is_i2p_tracker(url), "{url}");
            assert!(
                matches!(build_announce(url, &params()), Err(Error::BadUrl)),
                "{url} was built into an announce"
            );
        }
    }

    /// A query with no path is a query, not a hostname.
    ///
    /// `http://tracker.i2p?x=1` passed the filter (which cut the authority at
    /// `?`) and was then read by the builder (which cut only at `/`) as a host
    /// literally named `tracker.i2p?x=1` — a naming lookup that could never
    /// resolve, failing on every announce for the life of the torrent.
    #[test]
    fn a_query_with_no_path_is_not_part_of_the_hostname() {
        let (host, request) = build_announce("http://tracker.i2p?x=1", &params()).expect("built");
        assert_eq!(host, "tracker.i2p");
        let text = String::from_utf8(request).expect("ascii");
        assert!(text.starts_with("GET /?x=1&info_hash="), "{text}");
        assert!(text.contains("Host: tracker.i2p\r\n"), "{text}");
    }

    /// The dial gets the host; the `Host` header gets the authority.
    ///
    /// A port is meaningful to HTTP and meaningless to SAM naming, which
    /// resolves a name to a destination. Handing it the port too — which the
    /// old builder did, because it never separated one — is a lookup that
    /// cannot succeed.
    #[test]
    fn a_port_reaches_the_host_header_but_never_the_naming_lookup() {
        let (host, request) =
            build_announce("http://opentracker.dg2.i2p:80/announce", &params()).expect("built");
        assert_eq!(host, "opentracker.dg2.i2p", "the port must not be dialed");
        let text = String::from_utf8(request).expect("ascii");
        assert!(text.contains("Host: opentracker.dg2.i2p:80\r\n"), "{text}");
    }

    /// Nothing a `.torrent` can say puts an extra line in the request we send.
    ///
    /// Belt and braces, because the two halves fail independently: the URL is
    /// refused at parse time, and the encoder refuses the request even if
    /// something else built one.
    #[test]
    fn a_torrent_cannot_inject_a_header_into_our_announce() {
        assert!(matches!(
            build_announce("http://t.i2p/a\r\nX-Evil: 1", &params()),
            Err(Error::BadUrl)
        ));
        assert!(
            http::Request {
                method: "GET",
                target: "/a\r\nX-Evil: 1",
                host: "t.i2p",
                headers: &[],
                body: &[],
            }
            .encode()
            .is_none(),
            "the encoder must refuse a target that breaks the message"
        );
        assert!(
            http::Request {
                method: "GET",
                target: "/a",
                host: "t.i2p\r\nX-Evil: 1",
                headers: &[],
                body: &[],
            }
            .encode()
            .is_none(),
            "…and a host that does"
        );
        assert!(
            http::Request {
                method: "GET",
                target: "/a",
                host: "t.i2p",
                headers: &[("X", "1\r\nX-Evil: 1")],
                body: &[],
            }
            .encode()
            .is_none(),
            "…and a header value that does"
        );
    }

    /// A tracker does not get to write our log for us.
    #[test]
    fn a_tracker_failure_message_cannot_forge_a_log_line() {
        let body = encode(&dict(vec![(
            "failure reason",
            Value::Bytes(b"nope\r\ncloved: everything is fine\x1b[2J".to_vec()),
        )]));
        let Err(Error::TrackerFailure(text)) = parse_response(&body) else {
            panic!("a failure reason must surface as one");
        };
        assert!(
            !text.contains('\n') && !text.contains('\r') && !text.contains('\x1b'),
            "control characters survived: {text:?}"
        );
        assert!(
            text.starts_with("nope"),
            "the message is still readable: {text}"
        );

        // And a tracker cannot decide how much of the log it occupies.
        let long = encode(&dict(vec![(
            "failure reason",
            Value::Bytes(vec![b'x'; 10_000]),
        )]));
        let Err(Error::TrackerFailure(text)) = parse_response(&long) else {
            panic!("expected a failure reason");
        };
        assert!(
            text.chars().count() <= MAX_TRACKER_TEXT + 1,
            "{}",
            text.len()
        );
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
    /// What gets logged when a tracker refuses an announce must be the URL
    /// that was actually sent — every parameter an operator might bisect,
    /// except the one that is our own identity.
    #[test]
    fn the_announced_url_is_reassembled_from_the_wire() {
        let (host, request) =
            build_announce("http://tracker2.postman.i2p/announce.php", &params()).unwrap();
        let url = announced_url(&host, &request);
        assert!(url.starts_with("http://tracker2.postman.i2p/announce.php?info_hash="));
        assert!(
            !url.contains("MYDESTb64"),
            "our destination must not reach a log: {url}"
        );
        assert!(url.contains("&ip=<redacted>"), "{url}");
        // Everything else survives, or the line stops being worth logging.
        assert!(url.contains("&compact=1"), "{url}");
        assert!(url.contains("&event=started"), "{url}");
        assert!(
            url.contains(&format!("info_hash={}", "%AB".repeat(20))),
            "{url}"
        );
        assert!(
            !url.contains("HTTP/1.1"),
            "the version is not part of it: {url}"
        );
        assert!(
            !url.contains('\r') && !url.contains('\n'),
            "one line: {url}"
        );

        // Garbage in, something harmless out — this runs on an error path and
        // must never be the thing that panics.
        assert_eq!(announced_url("h", b""), "http://h/");
        assert_eq!(announced_url("h", b"GET"), "http://h/");
        assert_eq!(announced_url("h", &[0xff, 0xfe]), "http://h/");
    }

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
    /// `completed` is owed the moment a download finishes, not whenever the
    /// tracker's interval next comes round — and `completion_due` is the only
    /// automatic thing allowed to say so.
    #[test]
    fn finishing_brings_the_completion_announce_forward_exactly_once() {
        let mut st = AnnounceState::new();
        // Nothing to bring forward before the first announce: there is no
        // `completed` without a `started`.
        st.completion_due();
        assert_eq!(st.next_event(false), Event::Started);
        st.on_success(1_000, 1_800, Event::Started);
        assert!(!st.due(1_100), "the tracker asked for half an hour");

        // Finishing overrides that, once.
        st.completion_due();
        assert!(st.due(1_100), "a finished download waited out the interval");
        assert_eq!(st.next_event(true), Event::Completed);
        st.on_success(1_100, 1_800, Event::Completed);

        // And never again: the snatch has been reported, so this cannot become
        // a way to bypass the interval a second time.
        assert!(!st.due(1_200));
        st.completion_due();
        assert!(!st.due(1_200), "completion_due fired twice");
        assert_eq!(st.next_event(true), Event::Periodic);
    }

    /// A client that already held the file has no download to report. Sending
    /// one has the tracker count a snatch that never happened — once per
    /// restart, and clove rebuilds its announcers on every session blip.
    #[test]
    fn a_torrent_that_was_already_complete_reports_no_snatch() {
        let mut already = AnnounceState::already_complete();
        assert_eq!(already.next_event(true), Event::Started, "still says hello");
        already.on_success(0, 60, Event::Started);
        assert_eq!(
            already.next_event(true),
            Event::Periodic,
            "a resumed seed reported a download it never did"
        );
        already.completion_due();
        assert!(
            !already.due(1),
            "and cannot bring an announce forward either"
        );

        // The contrast: a torrent that finished while we watched does report.
        let mut earned = AnnounceState::new();
        earned.on_success(0, 60, Event::Started);
        assert_eq!(earned.next_event(true), Event::Completed);
    }

    #[test]
    fn a_non_bencode_body_reports_what_it_actually_was() {
        let html = b"<html><head><title>500 Internal Server Error</title></head>";
        let Err(e) = parse_response(html) else {
            panic!("HTML is not a valid announce response");
        };
        let text = e.to_string();
        assert!(
            text.contains("HTML page") && text.contains("500 Internal Server Error"),
            "the error must identify the page, got: {text}"
        );

        let page = "<!DOCTYPE html>\n<html>\n<head>\n<style type=\"text/css\">\n:root{\n\
             --border_table:inset 0 0 0 1px rgba(255,255,255,.3);\n\
             --postman:url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg'\
             %3E%3Cpath fill='%23ffe1b2' d='M19.2 27.4c-1.4-.4-2.4.4-2.4 2 0 2 2 7 4 4z'\
             /%3E%3C/svg%3E\");\n}\n</style>\n<title>Postman's Tracker</title>\n</head>\n\
             <body>\n<h1>Welcome</h1>\n<p>Torrent index and tracker.</p>\n</body>\n</html>";
        let Err(styled) = parse_response(page.as_bytes()) else {
            panic!("a web page is not a valid announce response");
        };
        let text = styled.to_string();
        assert!(
            text.contains("Postman's Tracker"),
            "the title identifies the page, and must survive the stylesheet: {text}"
        );
        assert!(
            text.contains("Welcome") && text.contains("Torrent index"),
            "the page's visible words must come through: {text}"
        );
        assert!(
            !text.contains("--border_table") && !text.contains("image/svg"),
            "the stylesheet is the noise this exists to remove: {text}"
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

    /// A tracker that answers correctly and unendingly slowly: a valid head,
    /// a length it means to honour, and then the body one byte at a time with
    /// a pause between each.
    ///
    /// Nothing here is malformed, so no parser rejects it, and every read
    /// succeeds, so a per-read socket timeout never fires — each byte resets
    /// it. The byte caps do not help either: the body stays under
    /// `MAX_RESPONSE_BYTES`, it just never arrives. Only a clock over the whole
    /// exchange ends this, which is what the deadline is.
    #[test]
    fn a_tracker_that_drips_slowly_gives_up_on_a_deadline() {
        /// A valid response head, then the body a byte per read, each after a
        /// pause — the shape of a tracker on a very slow tunnel, and of one
        /// stalling on purpose.
        struct Drip {
            head: std::io::Cursor<Vec<u8>>,
        }
        impl std::io::Read for Drip {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.head.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                if buf.is_empty() {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(5));
                buf[0] = b'd';
                Ok(1)
            }
        }
        impl std::io::Write for Drip {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (_host, request) = build_announce("http://t.i2p/announce", &params()).unwrap();
        // Well under MAX_RESPONSE_BYTES: the body is legal, it just never ends.
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 40000\r\n\r\n".to_vec();
        let mut drip = Drip {
            head: std::io::Cursor::new(head),
        };

        let started = Instant::now();
        let deadline = started + Duration::from_millis(200);
        let outcome = announce_over_until(&mut drip, &request, deadline);

        assert!(
            outcome.is_err(),
            "the body never arrived; this is not a win"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "a drip held the announce thread for {:?}",
            started.elapsed()
        );
        // 40 000 bytes at 5 ms each is over three minutes, so finishing this
        // fast can only be the deadline: the test would still pass on elapsed
        // time alone if the body had simply been short.
        let Err(Error::Http(http::Error::Io(e))) = outcome else {
            panic!("expected the deadline to surface as an I/O error");
        };
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut, "{e}");
    }

    /// The `ip` parameter carries our destination and must not survive into a
    /// log line; everything an operator would bisect must.
    #[test]
    fn only_the_ip_parameter_is_redacted() {
        assert_eq!(
            redact_ip_param("/announce?info_hash=%AB&ip=SECRET&event=started"),
            "/announce?info_hash=%AB&ip=<redacted>&event=started"
        );
        // First and last positions, not just the middle.
        assert_eq!(redact_ip_param("/a?ip=SECRET"), "/a?ip=<redacted>");
        assert_eq!(redact_ip_param("/a?ip=SECRET&x=1"), "/a?ip=<redacted>&x=1");
        // A parameter that merely ends in those letters keeps its value, and a
        // target with no query is untouched.
        assert_eq!(redact_ip_param("/a?skip=1&zip=2"), "/a?skip=1&zip=2");
        assert_eq!(redact_ip_param("/announce"), "/announce");
        // An `ip` with `=` in its value (percent-encoding leaves padding) still
        // loses all of it.
        assert_eq!(
            redact_ip_param("/a?ip=AA%3D%3D&x=1"),
            "/a?ip=<redacted>&x=1"
        );
    }
}
