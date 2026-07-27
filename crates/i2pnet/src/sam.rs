//! `SAMv3` backend over `yosemite` (Phase D, `docs/PLAN.md`).
//!
//! This wraps `yosemite`'s synchronous API behind the crate's traits. It is
//! the *only* code here that talks to a real router. Its runtime behavior
//! against a live router is verified out-of-CI (`docs/LIVE-TESTING.md`); the
//! logic that does not need a router — address derivation and the forwarded
//! destination-line parse — is unit-tested here over loopback TCP.
//!
//! - [`SamSession`] implements [`I2pDialer`] and [`I2pNamingLookup`].
//!   Dialing speaks SAM directly on a socket clove opens per stream (see
//!   [`dial_stream`]) rather than going through yosemite, so a stream is a
//!   [`ForwardedStream`] — a plain TCP socket to the bridge, with real
//!   timeouts, a real `split`, and close-on-drop. yosemite still owns the
//!   session itself: `SESSION CREATE`, `STREAM FORWARD` and `NAMING LOOKUP`.
//! - [`SamListener`] implements [`I2pListener`] for **inbound** streams via
//!   SAM `STREAM FORWARD` to a loopback [`TcpListener`] we own (an allowed
//!   Layer-1 IP socket, bound to `127.0.0.1`). This is the topology chosen
//!   over `STREAM ACCEPT` in `docs/LIVE-TESTING.md` §3: `accept` takes
//!   `&mut self` and serializes every inbound stream on the one session,
//!   whereas `forward` lets the router fan connections into a plain accept
//!   loop. With `SILENT=false` (yosemite's default) the router prepends each
//!   forwarded connection with the peer's base64 destination line, from
//!   which we derive its [`DestHash`] (`docs/PROTOCOL.i2p-bt` §1.3, §2.5).
//!   The exact framing is confirmed against a live router at M1.
//!
//! yosemite hardcodes the SAM host to `127.0.0.1` (only the port is
//! configurable), which happens to match Layer 1's loopback-only rule; the
//! `--i-know-sam-is-remote` escape hatch is therefore not expressible
//! through this backend and is noted as such.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use yosemite::{DestinationKind, Session, SessionOptions, style};

use crate::{DestHash, I2pDialer, I2pListener, I2pNamingLookup, I2pStream};

/// Standard `SAMv3` control port.
pub const DEFAULT_SAM_PORT: u16 = 7656;

/// Default for [`SamConfig::probe_timeout`].
///
/// Generous: a loopback router under load can be slow, and a false negative
/// here costs a full backoff cycle. It only has to be shorter than "forever".
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on the probe's `HELLO REPLY` line. A bridge that streams bytes without
/// a newline is refused rather than buffered — the reason this is a byte cap
/// and not just a timeout.
const MAX_HELLO_LINE: usize = 512;

/// Ask the bridge on `port` to prove it is a SAM bridge, within `timeout`.
///
/// Returns the version it reports, for the operator's log.
///
/// This exists because `yosemite` sets no read or write timeout on its
/// control socket (checked in 0.7.0). Against a bridge that accepts the
/// connection and then goes quiet — a router still starting up, a wedged
/// one, or some other service squatting on 7656 — `Session::new` blocks
/// forever. It never returns an error, so the supervisor above never backs
/// off, never retries and never logs: the daemon simply sits in "connecting"
/// until someone restarts it. That is precisely the failure mode SCOPE §4
/// exists to prevent, so it is worth one extra connection to rule out.
///
/// The probe speaks the SAM handshake itself, with timeouts and a length cap,
/// on a socket it owns and closes. What it cannot cover is a bridge that
/// answers `HELLO` correctly and *then* stalls on `SESSION CREATE`:
/// `Session::new` still hangs there. Closing that gap needs read timeouts
/// inside yosemite (upstream) or proxying its control connection through one
/// of ours; see `docs/PROTOCOL.i2p-bt` §2.7.
fn probe_bridge(port: u16, timeout: Duration) -> io::Result<String> {
    let (_socket, reply) = sam_hello(port, timeout, "HELLO")?;
    Ok(reply
        .split_whitespace()
        .find_map(|field| field.strip_prefix("VERSION="))
        .unwrap_or("unknown")
        .to_owned())
}

/// Read one `\n`-terminated line from `stream`, bounded by both `cap` bytes
/// and `deadline`.
///
/// A byte at a time, because the bytes after the line belong to whoever asked
/// for it: a SAM control socket becomes a data stream the moment its status
/// line ends, and a buffered reader that swallowed the first block of a peer's
/// handshake would be a bug with no symptom until much later.
///
/// `what` names the exchange in every error, since "connection closed" is not
/// a diagnosis and "closed during HELLO" is.
fn read_sam_line(
    stream: &mut TcpStream,
    port: u16,
    cap: usize,
    deadline: std::time::Instant,
    what: &str,
) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("SAM bridge on 127.0.0.1:{port} did not finish its {what} reply in time"),
            ));
        }
        // A read timeout surfaces as WouldBlock/TimedOut, whose stock text
        // ("Resource temporarily unavailable") tells an operator nothing.
        let got = stream.read(&mut byte).map_err(|e| {
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "127.0.0.1:{port} accepted the connection but did not answer {what} in \
                         time; is the router still starting, or is something else on that port?"
                    ),
                )
            } else {
                e
            }
        })?;
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("SAM bridge on 127.0.0.1:{port} closed the connection during {what}"),
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "127.0.0.1:{port} answered {what} with {cap}+ bytes and no newline; this does \
                     not look like a SAM bridge"
                ),
            ));
        }
        line.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&line)
        .trim_end_matches('\r')
        .to_owned())
}

/// Open a socket to the SAM bridge and complete the `HELLO VERSION`
/// handshake on it, returning the socket ready for a command.
///
/// Every SAM operation that is not the session's own control connection
/// begins exactly here: a **fresh** socket with its **own** handshake. That
/// is not incidental — it is the whole reason this exists (see
/// [`SamSession::dial`] and `docs/PROTOCOL.i2p-bt` §2.12).
///
/// Both deadlines are left set on the returned socket; the caller adjusts
/// them for whatever it does next.
fn sam_hello(port: u16, timeout: Duration, what: &str) -> io::Result<(TcpStream, String)> {
    let addr = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"HELLO VERSION MIN=3.1 MAX=3.3\n")?;

    // The read timeout is per-call, so a bridge dribbling one byte just
    // inside it could hold this open indefinitely. Bound the exchange too.
    let deadline = std::time::Instant::now() + timeout;
    let reply = read_sam_line(&mut stream, port, MAX_HELLO_LINE, deadline, what)?;
    if !reply.starts_with("HELLO REPLY") || !reply.contains("RESULT=OK") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SAM bridge on 127.0.0.1:{port} refused HELLO: {reply}"),
        ));
    }
    Ok((stream, reply))
}

/// Longest `STREAM STATUS` line accepted. The result and an optional message
/// are short; a router that sends more than this is not answering us.
const MAX_STATUS_LINE: usize = 1024;

/// Open an outbound virtual stream to `peer`, by speaking SAM ourselves.
///
/// **Why clove does this rather than asking its SAM library.** yosemite 0.7's
/// session controller is a single state machine shared by the control
/// connection and every stream operation, and each entry point begins
/// `mem::replace(&mut self.state, Poisoned)`, restoring the state only on
/// paths it expects. One unparseable reply — or a write that fails partway —
/// therefore leaves the controller poisoned, and *every subsequent dial on
/// that session fails forever* (`docs/PROTOCOL.i2p-bt` §2.12). Live, that cost
/// a session rebuild every 60–90 seconds: a new destination each time, all
/// known peers discarded, and a fresh announce needed before anything could
/// resume. There is no way to reset the state from outside the library.
///
/// `SAMv3` does not require any of that. A stream is its own connection: dial
/// the bridge, `HELLO VERSION`, `STREAM CONNECT`, and the socket you are
/// holding *is* the stream. Nothing is shared, so nothing can be poisoned by
/// somebody else's failure — which is how XD does it too, and it cannot have
/// this bug for the same reason.
///
/// Owning the socket buys two more things clove could not have before:
///
/// - **The dial timeout is real.** yosemite's `connect` takes no timeout and
///   the caller's was documented as advisory (§2.3). Here it bounds the wait
///   for `STREAM STATUS`, which is where a leaseSet lookup spends its time.
/// - **The stream is closeable and boundable.** A dialled peer that goes
///   silent parked a thread and leaked a socket for the life of the process
///   (§2.7a); the returned [`ForwardedStream`] takes read and write timeouts
///   like any other socket, and dropping it closes it.
///
/// # Errors
///
/// Connect, handshake or write failure; a `STREAM STATUS` other than
/// `RESULT=OK`, carrying the router's own words; or `timeout` elapsing first.
fn dial_stream(
    port: u16,
    nickname: &str,
    peer: DestHash,
    timeout: Duration,
) -> io::Result<ForwardedStream> {
    let (mut stream, _) = sam_hello(port, timeout, "HELLO (for STREAM CONNECT)")?;

    // SILENT=false: the router answers with a STREAM STATUS line before any
    // peer bytes, which is what makes a failed dial reportable rather than a
    // stream that simply never says anything.
    let command = format!(
        "STREAM CONNECT ID={nickname} DESTINATION={} SILENT=false\n",
        peer.to_b32()
    );
    stream.write_all(command.as_bytes())?;

    // The router may spend most of the budget here resolving a leaseSet, so
    // the status read gets the caller's full timeout rather than the short
    // handshake one.
    stream.set_read_timeout(Some(timeout))?;
    let deadline = std::time::Instant::now() + timeout;
    let status = read_sam_line(
        &mut stream,
        port,
        MAX_STATUS_LINE,
        deadline,
        "STREAM CONNECT",
    )?;
    if !status.starts_with("STREAM STATUS") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SAM bridge answered STREAM CONNECT with {status:?}"),
        ));
    }
    let result = status
        .split_whitespace()
        .find_map(|f| f.strip_prefix("RESULT="))
        .unwrap_or("MISSING");
    if result != "OK" {
        // The router's own result word, and its MESSAGE when it sent one.
        // CANT_REACH_PEER and friends are ordinary on I2P and must read as
        // "this peer, this time" rather than as a fault in the session.
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "router refused the stream to {}: {result}{}",
                peer.to_b32(),
                status
                    .split_once("MESSAGE=")
                    .map(|(_, m)| format!(" ({})", m.trim_matches('"')))
                    .unwrap_or_default()
            ),
        ));
    }

    // Handshake done; the socket is now the peer stream. Clear the deadlines
    // the handshake needed — the engine sets its own per-peer timeouts, and a
    // stray one here would look to it like a peer that went quiet.
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(ForwardedStream::from_socket(stream))
}

/// A SAM session id unlikely to collide with one already registered.
///
/// SAM session ids are per-router, not per-connection, and a router does not
/// necessarily free one the instant our control socket closes — emissary
/// 0.4.0 holds it long enough that a second run seconds later is refused with
/// `DuplicateId`. Observed as three consecutive `sam-stress` runs failing at
/// session setup after the first one exited normally
/// (`docs/PROTOCOL.i2p-bt` §2.9).
///
/// This matters well beyond the harness: the SCOPE §4 reconnect discipline
/// has the daemon rebuild its session tree after losing the router. If it
/// reuses a fixed id, the rebuild can be refused by the stale session it is
/// replacing — the supervisor would then back off and retry into the same
/// refusal, which is the failure it exists to prevent.
///
/// The id is not the identity: that is the destination key (Q4). This can
/// therefore vary freely per process, and stays readable so an operator can
/// still find the session in a router console.
#[must_use]
pub fn unique_nickname(base: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{base}-{}-{now:05x}", std::process::id())
}

/// How to bring up the SAM session.
#[derive(Clone, Debug)]
pub struct SamConfig {
    /// SAM control port on `127.0.0.1` (yosemite is loopback-only).
    pub samv3_tcp_port: u16,
    /// SAM session nickname.
    pub nickname: String,
    /// Base64 private key for a stable identity (Q4). `None` requests a
    /// transient destination.
    pub persistent_key: Option<String>,
    /// How long the pre-flight handshake probe waits for the bridge to
    /// answer `HELLO` before giving up on this attempt (see
    /// [`probe_bridge`]). Raise it for a router that is slow to come up; the
    /// cost of a low value is a wasted backoff cycle, the cost of no value
    /// at all is a daemon that hangs.
    pub probe_timeout: Duration,
}

impl Default for SamConfig {
    fn default() -> Self {
        SamConfig {
            samv3_tcp_port: DEFAULT_SAM_PORT,
            nickname: "clove".to_owned(),
            persistent_key: None,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }
}

/// A live SAM stream session: our destination plus the control connection,
/// used for outbound streams and naming.
pub struct SamSession {
    session: Mutex<Session<style::Stream>>,
    local: DestHash,
    local_b64: String,
    samv3_tcp_port: u16,
    /// The SAM session id every outbound stream attaches itself to.
    nickname: String,
}

impl SamSession {
    /// Establish the session against the router named by `config`.
    ///
    /// # Errors
    ///
    /// The router is unreachable, refused the session, or returned a
    /// destination we cannot parse.
    pub fn connect(config: &SamConfig) -> io::Result<SamSession> {
        // Prove the bridge is alive and speaks SAM before yosemite is given
        // the port. See [`probe_bridge`]: without this, a router that accepts
        // the connection and then says nothing blocks here forever, and the
        // supervisor above never gets to back off, retry, or log a word.
        let version = probe_bridge(config.samv3_tcp_port, config.probe_timeout)?;
        debug_assert!(!version.is_empty());

        let destination = match &config.persistent_key {
            Some(key) => DestinationKind::Persistent {
                private_key: key.clone(),
            },
            None => DestinationKind::Transient,
        };
        let options = SessionOptions {
            nickname: config.nickname.clone(),
            destination,
            samv3_tcp_port: config.samv3_tcp_port,
            // Inbound forwarding relies on the router prepending each
            // connection with the peer's destination line; keep it explicit
            // rather than inheriting yosemite's default (see [`SamListener`]).
            silent_forward: false,
            ..Default::default()
        };
        let session = Session::<style::Stream>::new(options).map_err(map_err)?;
        // What SAM hands back is the session's *private key blob*, not its
        // destination (SAMv3 SESSION STATUS). Everything clove publishes must
        // be the public destination at the front of it — the hash we call
        // ourselves by, and the base64 an announce carries. Sending the rest
        // to a tracker means sending our private keys to a stranger, which is
        // exactly what clove did until 2026-07-27 (`PROTOCOL.i2p-bt` §5.1c).
        let bytes = crate::addr::destination_bytes(session.destination()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SAM returned a destination clove cannot parse",
            )
        })?;
        let local = DestHash::from_b64_destination(session.destination()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SAM returned an unparseable destination",
            )
        })?;
        let local_b64 = crate::addr::i2p_base64_encode(&bytes);
        Ok(SamSession {
            session: Mutex::new(session),
            local,
            local_b64,
            samv3_tcp_port: config.samv3_tcp_port,
            nickname: config.nickname.clone(),
        })
    }

    /// Our full base64 destination — what tracker announces carry as `ip`
    /// (`docs/PROTOCOL.i2p-bt` §5.1).
    #[must_use]
    pub fn local_dest_b64(&self) -> &str {
        &self.local_b64
    }

    /// Probe the session's control connection with a SAM `PING` (v3.2).
    /// `false` means the router is gone or the session is dead — time to tear
    /// down and rebuild the session tree.
    pub fn healthy(&self) -> bool {
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session.send_command("PING clove\n").is_ok()
    }

    /// This session's own destination hash — the identity peers reach us at
    /// and the one announced to trackers.
    #[must_use]
    pub fn local_dest(&self) -> DestHash {
        self.local
    }
}

/// Upper bound on the forwarded destination line (bytes). A full I2P
/// destination is ~516 base64 chars plus a few `FROM_PORT`/`TO_PORT` params;
/// anything past this from a misbehaving router is refused rather than read
/// unboundedly.
const MAX_DEST_LINE: usize = 4096;

/// How long `accept` waits for a forwarded connection's destination line
/// before giving up on it. Guards the acceptor thread against a router that
/// forwards a connection but never sends the (`SILENT=false`) header.
const DEST_LINE_TIMEOUT: Duration = Duration::from_secs(30);

/// An inbound listener: the router forwards peer streams (SAM `STREAM
/// FORWARD`) to a loopback [`TcpListener`] this owns. Holds an [`Arc`] to the
/// [`SamSession`] so the forwarding stays live for the listener's lifetime.
///
/// One session backs both dialing ([`SamSession`] as [`I2pDialer`]) and this
/// listener; `forward` and `connect` are independent SAM operations on the
/// same nickname, so a client both seeds and leeches on one destination.
pub struct SamListener {
    listener: TcpListener,
    local: DestHash,
    port: u16,
    // Keeps the SAM session (and thus the router-side forwarding) alive.
    _session: Arc<SamSession>,
}

impl SamListener {
    /// Ask the router to forward inbound streams for `session`'s destination
    /// to a fresh loopback listener, and return it.
    ///
    /// # Errors
    ///
    /// The loopback listener cannot be bound, or the router refuses the
    /// `STREAM FORWARD` request.
    pub fn forward(session: Arc<SamSession>) -> io::Result<SamListener> {
        // The one inbound IP-socket construction site: loopback by
        // construction (Layer 1, SCOPE §5), ephemeral port.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        {
            let mut sam = session
                .session
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            sam.forward(port).map_err(map_err)?;
        }
        let local = session.local;
        Ok(SamListener {
            listener,
            local,
            port,
            _session: session,
        })
    }

    /// The loopback port the router forwards to. Keep it around to
    /// [`poke_listener`] a blocked accept during session teardown.
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.port
    }
}

/// Wake a [`SamListener`]'s blocked accept by making one throwaway loopback
/// connection to it — used after raising the demux stop flag during session
/// teardown, since a dead router never unblocks the accept itself.
///
/// # Errors
///
/// The connect fails (listener already gone — which also unblocks nothing
/// left to unblock).
pub fn poke_listener(port: u16) -> io::Result<()> {
    drop(TcpStream::connect((Ipv4Addr::LOCALHOST, port))?);
    Ok(())
}

impl I2pListener for SamListener {
    type Stream = ForwardedStream;

    fn local_dest(&self) -> DestHash {
        self.local
    }

    fn accept(&self) -> io::Result<Option<(ForwardedStream, DestHash)>> {
        let (mut stream, _addr) = self.listener.accept()?;
        // Bound the header read so a silent/misbehaving router cannot wedge
        // the acceptor; then hand a blocking socket to the reader thread.
        stream.set_read_timeout(Some(DEST_LINE_TIMEOUT))?;
        // A header that never arrives, arrives as garbage, or belongs to
        // something that is not the router at all says nothing about the
        // listener: drop that connection and let the caller accept the next.
        // Anything on the loopback forward port can produce one, including our
        // own `poke_listener`.
        let Ok(dest) = read_dest_line(&mut stream, MAX_DEST_LINE) else {
            return Ok(None);
        };
        stream.set_read_timeout(None)?;
        Ok(Some((ForwardedStream { inner: stream }, dest)))
    }
}

/// Read the `SILENT=false` destination header the router prepends to a
/// forwarded connection — the peer's base64 destination, optionally followed
/// by space-separated `FROM_PORT`/`TO_PORT` params — up to the `\n`, and
/// derive the peer's [`DestHash`]. Reads one byte at a time so the stream
/// payload after the newline is left untouched for the peer's reader.
fn read_dest_line<R: Read>(reader: &mut R, max_len: usize) -> io::Result<DestHash> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "forwarded stream closed before its destination line",
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "forwarded destination line exceeds the maximum length",
            ));
        }
        line.push(byte[0]);
    }
    let text = std::str::from_utf8(&line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "destination line is not UTF-8"))?;
    DestHash::from_b64_destination(text).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "forwarded destination line is not a parseable I2P destination",
        )
    })
}

/// An inbound SAM stream: the loopback TCP connection the router forwarded to
/// us, carrying the tunneled peer stream after its destination header was
/// consumed. Split via `TcpStream::try_clone` (both halves are the same
/// socket), matching the reader-thread/writer-thread model (Q5).
#[derive(Debug)]
pub struct ForwardedStream {
    inner: TcpStream,
}

impl ForwardedStream {
    /// Adopt an already-connected SAM stream socket.
    ///
    /// Inbound (the router forwarded it) and outbound (we dialled it with
    /// [`dial_stream`]) end up as the same thing — a TCP socket to the bridge
    /// carrying peer bytes — so they get the same type, and with it the same
    /// timeouts, the same `split`, and the same close-on-drop.
    #[must_use]
    pub(crate) fn from_socket(inner: TcpStream) -> ForwardedStream {
        ForwardedStream { inner }
    }

    /// Bound how long reads and writes on this stream may block.
    ///
    /// The socket is a loopback TCP connection from the router, so this is a
    /// real timeout and not a polite request. Worth setting on anything that
    /// serves peers: a stream that connects and then goes quiet otherwise
    /// parks a thread for the life of the process.
    ///
    /// # Errors
    ///
    /// The underlying `setsockopt` failed.
    pub fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)?;
        self.inner.set_write_timeout(timeout)
    }
}

impl Read for ForwardedStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for ForwardedStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl I2pStream for ForwardedStream {
    type Reader = TcpStream;
    type Writer = TcpStream;

    fn split(self) -> io::Result<(TcpStream, TcpStream)> {
        let reader = self.inner.try_clone()?;
        Ok((reader, self.inner))
    }

    /// Real timeouts: this is a loopback TCP socket from the router.
    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        ForwardedStream::set_timeouts(self, timeout)
    }
}

impl I2pDialer for SamSession {
    type Stream = ForwardedStream;

    /// Dial `peer` on a socket of our own (see [`dial_stream`]).
    ///
    /// No session mutex is taken and no library state is touched, so dials
    /// are genuinely concurrent — `PROTOCOL.i2p-bt` §2.6a's serialization
    /// point was yosemite's `&mut self`, and it is gone with it. The session
    /// is consulted for exactly one thing, its nickname, which SAM needs to
    /// attach the new stream to the right session.
    fn dial(&self, peer: DestHash, timeout: Duration) -> io::Result<ForwardedStream> {
        dial_stream(self.samv3_tcp_port, &self.nickname, peer, timeout)
    }
}

impl I2pNamingLookup for SamSession {
    fn lookup(&self, name: &str) -> io::Result<DestHash> {
        let dest = yosemite::RouterApi::new(self.samv3_tcp_port)
            .lookup_name(name)
            .map_err(map_err)?;
        DestHash::from_b64_destination(&dest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "naming lookup returned garbage")
        })
    }
}

/// Map a yosemite error into an `io::Error` with operator-readable text.
fn map_err(e: yosemite::Error) -> io::Error {
    match e {
        yosemite::Error::IoError(io) => io,
        other => io::Error::other(other),
    }
}

#[cfg(test)]
mod tests {
    //! Router-free coverage. A *working* SAM session needs a live router
    //! (`docs/LIVE-TESTING.md`), but everything about a router that is
    //! **not** working can be tested here, and that is the half that decides
    //! whether the daemon degrades or wedges.
    //!
    //! Three groups:
    //!
    //! - The inbound path's pure logic: splitting the forwarded socket and
    //!   parsing the `SILENT=false` destination header.
    //! - [`probe_bridge`] against a fake bridge that lies, stalls, floods or
    //!   dies — SCOPE §9's "SAM bridge lying or dying mid-operation".
    //! - [`SamSession::connect`] as a whole against the same fakes, which is
    //!   the claim that actually matters: **every one of them must fail, and
    //!   fail in bounded time.** Before the probe existed, four of six hung
    //!   forever, and a hang here is worse than an error — the supervisor
    //!   above never backs off, never retries and never logs.

    use super::*;
    use crate::addr::i2p_base64_encode;
    use std::io::Cursor;

    #[test]
    fn nicknames_are_unique_and_still_readable() {
        let a = unique_nickname("clove");
        let b = unique_nickname("clove");
        assert_ne!(a, b, "two ids in the same process collided");
        // Readable enough to find in a router console, and a valid SAM id:
        // no spaces, no quotes, nothing needing escaping.
        assert!(a.starts_with("clove-"), "{a}");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{a} would need quoting in a SAM command"
        );
    }

    #[test]
    fn forwarded_stream_splits_and_carries_both_directions() {
        // A real loopback TCP pair stands in for the router-forwarded socket.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();

        let mut client = ForwardedStream { inner: client };
        let server = ForwardedStream { inner: server };
        let (mut srv_read, mut srv_write) = server.split().unwrap();

        client.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        srv_read.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");

        srv_write.write_all(b"pong").unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[test]
    fn read_dest_line_parses_dest_and_leaves_payload() {
        // A destination-shaped blob: an inbound peer's header carries a real
        // destination, and anything shorter is not one.
        let mut dest_bytes = vec![0x42u8; 384];
        dest_bytes.extend_from_slice(&[0x05, 0x00, 0x04, 0x00, 0x07, 0x00, 0x00]);
        let b64 = i2p_base64_encode(&dest_bytes);
        let expected = DestHash::from_b64_destination(&b64).unwrap();

        // The router's SILENT=false header, then the peer's BT handshake.
        let mut input = Vec::new();
        input.extend_from_slice(b64.as_bytes());
        input.extend_from_slice(b" FROM_PORT=6881 TO_PORT=0\n");
        input.extend_from_slice(b"the-bittorrent-handshake");
        let mut cursor = Cursor::new(input);

        let got = read_dest_line(&mut cursor, MAX_DEST_LINE).unwrap();
        assert_eq!(got, expected);

        // The payload after the newline must be intact for the peer reader.
        let mut rest = Vec::new();
        cursor.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"the-bittorrent-handshake");
    }

    #[test]
    fn read_dest_line_rejects_overlong_line() {
        let mut cursor = Cursor::new(vec![b'A'; 128]); // no newline, no dest
        let err = read_dest_line(&mut cursor, 32).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_dest_line_eof_before_newline() {
        let mut cursor = Cursor::new(b"partial-no-newline".to_vec());
        let err = read_dest_line(&mut cursor, MAX_DEST_LINE).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[cfg(test)]
mod hostile_bridge_tests {
    //! A fake SAM bridge, misbehaving in every way a real one can.
    //!
    //! Each case asserts two things: the call **fails** (never succeeds
    //! against a bridge that is not one) and it **returns**. The second is
    //! the one with teeth — see the module docs above.

    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    /// Anything a bridge can do wrong before a session exists.
    #[derive(Clone, Copy, Debug)]
    enum Misbehaviour {
        /// Accept, then close without a word.
        CloseImmediately,
        /// Accept and never say anything, holding the connection open.
        Silence,
        /// Bytes that are not SAM, with no line terminator.
        Garbage,
        /// An endless stream with no newline — the case a byte cap catches
        /// and a timeout alone does not.
        Flood,
        /// A well-formed refusal.
        RefuseHello,
        /// A single byte every so often: inside any per-read timeout, but
        /// never finishing. Only a whole-exchange deadline stops this.
        Dribble,
        /// Valid SAM, then a stall. The residual case the probe cannot
        /// cover; asserted as a known limitation rather than a passing test.
        HelloThenStall,
    }

    /// Start a fake bridge on an ephemeral loopback port.
    ///
    /// The listener thread is detached and the process ends the test binary;
    /// nothing here outlives the run.
    fn fake_bridge(how: Misbehaviour) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                std::thread::spawn(move || {
                    // Let the client get its HELLO out first.
                    let mut buf = [0u8; 256];
                    let _ = sock.read(&mut buf);
                    match how {
                        Misbehaviour::CloseImmediately => return,
                        Misbehaviour::Garbage => {
                            let _ = sock.write_all(&[0xFF; 64]);
                        }
                        Misbehaviour::Flood => {
                            while sock.write_all(&[b'A'; 4096]).is_ok() {}
                            return;
                        }
                        Misbehaviour::RefuseHello => {
                            let _ = sock.write_all(b"HELLO REPLY RESULT=NOVERSION\n");
                        }
                        Misbehaviour::Dribble => loop {
                            if sock.write_all(b"X").is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(200));
                        },
                        Misbehaviour::HelloThenStall => {
                            let _ = sock.write_all(b"HELLO REPLY RESULT=OK VERSION=3.3\n");
                        }
                        Misbehaviour::Silence => {}
                    }
                    // Hold the connection open; the test's own deadline ends
                    // the wait either way.
                    std::thread::sleep(Duration::from_secs(120));
                });
            }
        });
        port
    }

    /// Run `f` on a thread and give it `limit`. `None` means it never
    /// returned — a hang, which is the failure this suite exists to catch.
    fn within<T: Send + 'static>(
        limit: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<(T, Duration)> {
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(limit).ok().map(|v| (v, start.elapsed()))
    }

    /// Short enough that a test which regressed into a hang fails quickly,
    /// long enough to clear the probe's own 10s budget.
    const LIMIT: Duration = Duration::from_secs(25);

    /// The probe budget under test. Short on purpose: these cases stall by
    /// design, and `make test` should not spend half a minute proving it.
    const PROBE: Duration = Duration::from_millis(600);

    #[test]
    fn the_probe_refuses_every_bridge_that_is_not_one() {
        for how in [
            Misbehaviour::CloseImmediately,
            Misbehaviour::Silence,
            Misbehaviour::Garbage,
            Misbehaviour::Flood,
            Misbehaviour::RefuseHello,
            Misbehaviour::Dribble,
        ] {
            let port = fake_bridge(how);
            let Some((result, elapsed)) = within(LIMIT, move || probe_bridge(port, PROBE)) else {
                panic!("{how:?}: probe_bridge never returned");
            };
            let err = result.expect_err(&format!("{how:?} was accepted as a SAM bridge"));
            assert!(elapsed < LIMIT, "{how:?}: probe took {elapsed:?}");
            // The operator has to be able to act on this at 2 a.m., so the
            // message names the address it could not talk to.
            assert!(
                err.to_string().contains("127.0.0.1:"),
                "{how:?}: unhelpful error {err}"
            );
        }
    }

    #[test]
    fn connect_fails_in_bounded_time_against_a_broken_bridge() {
        // The regression this locks down: before the pre-flight probe,
        // Silence, Garbage, Flood and Dribble all blocked in
        // yosemite's Session::new forever, and cloved sat in "connecting"
        // with nothing in its log until someone restarted it.
        for how in [
            Misbehaviour::CloseImmediately,
            Misbehaviour::Silence,
            Misbehaviour::Garbage,
            Misbehaviour::Flood,
            Misbehaviour::RefuseHello,
            Misbehaviour::Dribble,
        ] {
            let port = fake_bridge(how);
            let config = SamConfig {
                samv3_tcp_port: port,
                probe_timeout: PROBE,
                ..Default::default()
            };
            let Some((result, elapsed)) = within(LIMIT, move || SamSession::connect(&config))
            else {
                panic!("{how:?}: SamSession::connect never returned — the hang is back");
            };
            assert!(
                result.is_err(),
                "{how:?}: connect claimed a session against a bridge that is not one"
            );
            assert!(elapsed < LIMIT, "{how:?}: connect took {elapsed:?}");
        }
    }

    #[test]
    fn nothing_is_listening_is_a_fast_clean_error() {
        // Bind and drop, so the port is almost certainly free: the ordinary
        // "router is not running" case, which must be cheap because the
        // supervisor retries it on every backoff tick.
        let port = {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let config = SamConfig {
            samv3_tcp_port: port,
            probe_timeout: PROBE,
            ..Default::default()
        };
        let (result, elapsed) =
            within(LIMIT, move || SamSession::connect(&config)).expect("connect returned");
        assert!(result.is_err());
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    /// The gap the probe does not close, recorded as a test so it cannot be
    /// forgotten: a bridge that answers `HELLO` and then stalls still hangs
    /// inside yosemite, which sets no read timeout on its control socket.
    ///
    /// Written as an assertion about the *probe* — which correctly passes
    /// such a bridge — rather than about `connect`, because asserting the
    /// hang would mean a test that waits for one.
    #[test]
    fn a_bridge_that_passes_hello_and_then_stalls_is_a_known_gap() {
        let port = fake_bridge(Misbehaviour::HelloThenStall);
        let (result, _) = within(LIMIT, move || probe_bridge(port, PROBE)).expect("probe returned");
        assert_eq!(
            result.expect("a valid HELLO REPLY must pass the probe"),
            "3.3"
        );
        // If yosemite ever grows read timeouts, connect() against this bridge
        // starts failing cleanly and PROTOCOL.i2p-bt §2.7 can be closed.
    }

    #[test]
    fn a_healthy_hello_is_accepted_and_its_version_reported() {
        let port = fake_bridge(Misbehaviour::HelloThenStall);
        assert_eq!(probe_bridge(port, PROBE).expect("hello accepted"), "3.3");
    }

    /// A bridge that speaks the outbound-dial half of SAM: `HELLO REPLY`,
    /// then the `STREAM STATUS` line it was told to give, then it echoes.
    ///
    /// Enough to test the whole dial path without a router, which is the
    /// point: this code is the reason clove no longer needs a live router to
    /// know whether a failed dial is reportable.
    fn dial_bridge(status: &'static str, echo: bool) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut line = Vec::new();
                    let mut byte = [0u8; 1];
                    // HELLO VERSION
                    while sock.read(&mut byte).unwrap_or(0) == 1 {
                        if byte[0] == b'\n' {
                            break;
                        }
                        line.push(byte[0]);
                    }
                    let _ = sock.write_all(b"HELLO REPLY RESULT=OK VERSION=3.3\n");
                    // STREAM CONNECT
                    let mut command = Vec::new();
                    while sock.read(&mut byte).unwrap_or(0) == 1 {
                        if byte[0] == b'\n' {
                            break;
                        }
                        command.push(byte[0]);
                    }
                    let _ = sock.write_all(status.as_bytes());
                    if echo {
                        let mut buf = [0u8; 64];
                        while let Ok(n) = sock.read(&mut buf) {
                            if n == 0 || sock.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });
        port
    }

    const PEER: DestHash = DestHash([0x11; 32]);

    /// The happy path: a dialled stream is a socket clove owns, carrying peer
    /// bytes, with the status line consumed and not one byte more.
    #[test]
    fn a_dialled_stream_hands_back_the_socket_with_the_status_line_eaten() {
        let port = dial_bridge("STREAM STATUS RESULT=OK\n", true);
        let mut stream = dial_stream(port, "clove-test", PEER, PROBE).expect("dial");
        stream.write_all(b"the-bittorrent-handshake").unwrap();
        let mut back = [0u8; 24];
        stream.read_exact(&mut back).unwrap();
        assert_eq!(
            &back, b"the-bittorrent-handshake",
            "the status line must not be mistaken for peer data, nor peer data \
             swallowed while reading it"
        );
    }

    /// A router refusing one peer is ordinary on I2P. It must read as "this
    /// peer, this time", carry the router's own words, and leave nothing
    /// behind — the failure that used to poison the whole session.
    #[test]
    fn a_refused_dial_reports_the_routers_own_words_and_costs_nothing_else() {
        let port = dial_bridge("STREAM STATUS RESULT=CANT_REACH_PEER\n", false);
        let e = dial_stream(port, "clove-test", PEER, PROBE).expect_err("refused");
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
        assert!(e.to_string().contains("CANT_REACH_PEER"), "{e}");
        assert!(e.to_string().contains(&PEER.to_b32()), "{e}");

        // And the next dial on the same session id works, because the two
        // share no state at all.
        let ok = dial_bridge("STREAM STATUS RESULT=OK\n", true);
        assert!(dial_stream(ok, "clove-test", PEER, PROBE).is_ok());
    }

    /// A `MESSAGE=` is the router explaining itself; it belongs in the error.
    #[test]
    fn a_refusal_message_reaches_the_operator() {
        let port = dial_bridge(
            "STREAM STATUS RESULT=I2P_ERROR MESSAGE=\"session not found\"\n",
            false,
        );
        let e = dial_stream(port, "clove-test", PEER, PROBE).expect_err("refused");
        assert!(e.to_string().contains("session not found"), "{e}");
    }

    /// Every way a bridge can misbehave during a dial must fail, and fail
    /// bounded. Before clove owned this socket the caller's timeout was
    /// advisory (`PROTOCOL.i2p-bt` §2.3) and a silent bridge parked the
    /// thread for the life of the process.
    #[test]
    fn no_misbehaving_bridge_can_park_a_dial() {
        for how in [
            Misbehaviour::CloseImmediately,
            Misbehaviour::Silence,
            Misbehaviour::Garbage,
            Misbehaviour::Flood,
            Misbehaviour::RefuseHello,
            Misbehaviour::Dribble,
            Misbehaviour::HelloThenStall,
        ] {
            let port = fake_bridge(how);
            let (result, took) =
                within(LIMIT, move || dial_stream(port, "clove-test", PEER, PROBE))
                    .expect("dial returned");
            assert!(result.is_err(), "{how:?} was accepted as a stream");
            assert!(took < LIMIT, "{how:?} took {took:?}");
        }
    }

    /// A `RESULT=OK` with no newline is not a status line, however much it
    /// looks like one. Refusing it is what stops a half-sent reply being
    /// handed to the engine as a working peer.
    ///
    /// The unbounded and dribbled variants — where the bridge keeps the
    /// socket open — are covered by `Flood` and `Dribble` in the sweep above.
    /// Here the bridge closes, so the honest answer is end-of-file, and what
    /// matters is that the error names the exchange it died in.
    #[test]
    fn an_unterminated_status_line_is_never_a_stream() {
        let port = dial_bridge("STREAM STATUS RESULT=OK", false); // no newline
        let e = dial_stream(port, "clove-test", PEER, PROBE).expect_err("no newline");
        assert!(
            e.to_string().contains("STREAM CONNECT"),
            "the error must name the exchange it failed in: {e}"
        );
    }
}
