//! `SAMv3` backend — the M1 half of `SCOPE.md` §8.
//!
//! This is the *only* code here that talks to a real router, and its runtime
//! behavior against one is therefore not covered by any test in this repo.
//! Everything that does not need a router — address derivation, the forwarded
//! destination-line parse, and every way a bridge can misbehave — is
//! unit-tested here over loopback TCP.
//!
//! - [`SamSession`] implements [`I2pDialer`] and [`I2pNamingLookup`]. It owns
//!   its control connection: `HELLO VERSION` and `SESSION CREATE` are spoken
//!   on a socket clove opens, with real deadlines on both (`PROTOCOL.i2p-bt`
//!   §2.7, §2.13). Dialing likewise speaks SAM on a socket opened per stream
//!   (see the private `dial_stream`), so a stream is a [`ForwardedStream`] — a plain TCP
//!   socket to the bridge, with real timeouts, a real `split`, and
//!   close-on-drop. `NAMING LOOKUP` is likewise a one-shot on a socket of its
//!   own (the private `naming_lookup`), with no session behind it.
//! - The control connection is **read continuously** by a watchdog thread for
//!   as long as the session lives (the private `watch_control` thread). That
//!   is not a nicety:
//!   `SAMv3.2` lets the router ping the client and Java I2P drops the session
//!   when nobody answers, so a control connection that is only written to is a
//!   session with a timer on it. It is also the only place the router's
//!   account of *why* a session ended is ever spoken (§2.13).
//! - [`SamListener`] implements [`I2pListener`] for **inbound** streams via
//!   SAM `STREAM FORWARD` to a loopback [`TcpListener`] we own (an allowed
//!   Layer-1 IP socket, bound to `127.0.0.1`). This is the topology chosen
//!   over `STREAM ACCEPT`, and the reason is concurrency: `accept` takes
//!   `&mut self` and serializes every inbound stream on the one session,
//!   whereas `forward` lets the router fan connections into a plain accept
//!   loop. With `SILENT=false` the router prepends each
//!   forwarded connection with the peer's base64 destination line, from
//!   which we derive its [`DestHash`] (`docs/PROTOCOL.i2p-bt` §1.3, §2.5).
//!
//! Every socket here is opened to `127.0.0.1` by construction, which is
//! Layer 1's loopback-only rule. A remote SAM bridge is not reachable through
//! this backend and is refused at configuration time.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::{DestHash, I2pDialer, I2pListener, I2pNamingLookup, I2pStream};

/// Standard `SAMv3` control port.
pub const DEFAULT_SAM_PORT: u16 = 7656;

/// Default for [`SamConfig::probe_timeout`].
///
/// Generous: a loopback router under load can be slow, and a false negative
/// here costs a full backoff cycle. It only has to be shorter than "forever".
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default for [`SamConfig::session_timeout`].
///
/// `SESSION CREATE` is where a router builds the destination's tunnels, so it
/// is legitimately slow — Java I2P has taken tens of seconds from a cold start
/// (`PROTOCOL.i2p-bt` §2.10) — but it is not legitimately unbounded, which is
/// what it used to be.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(180);

/// Cap on the `HELLO REPLY` line. A bridge that streams bytes without a
/// newline is refused rather than buffered — the reason this is a byte cap and
/// not just a timeout.
const MAX_HELLO_LINE: usize = 512;

/// Cap on a `SESSION STATUS` line. It carries the session's whole private key
/// blob (§5.1c: 908 base64 characters from i2pd, and longer key types exist),
/// so it needs far more room than a status line — but not an unbounded amount.
/// i2pd's own SAM read buffer is 8 KiB, so nothing conforming exceeds this.
const MAX_SESSION_LINE: usize = 8192;

/// Cap on a line the router volunteers on the control connection after setup
/// (a `PING`, or the `SESSION STATUS` that explains a session ending).
const MAX_CONTROL_LINE: usize = 8192;

/// How long a `PONG` may take to reach a bridge that has stopped reading
/// before the watchdog gives up on the session.
const PONG_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Read one `\n`-terminated line from `stream`, bounded by `cap` bytes and —
/// when one is given — by `deadline`.
///
/// A byte at a time, because the bytes after the line belong to whoever asked
/// for it: a SAM control socket becomes a data stream the moment its status
/// line ends, and a buffered reader that swallowed the first block of a peer's
/// handshake would be a bug with no symptom until much later.
///
/// `deadline` is `None` for the one reader that is *supposed* to wait
/// indefinitely: the session watchdog, which sits on the control connection
/// for the life of the session and whose whole job is to be there whenever the
/// router finally says something.
///
/// `what` names the exchange in every error, since "connection closed" is not
/// a diagnosis and "closed during HELLO" is.
fn read_sam_line(
    stream: &mut TcpStream,
    port: u16,
    cap: usize,
    deadline: Option<Instant>,
    what: &str,
) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if deadline.is_some_and(|at| Instant::now() >= at) {
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
    let deadline = Instant::now() + timeout;
    let reply = read_sam_line(&mut stream, port, MAX_HELLO_LINE, Some(deadline), what)?;
    // The result is a field, not a substring: a refusal whose MESSAGE quotes
    // `RESULT=OK` is still a refusal. And the line reaches a terminal.
    let result = reply
        .split_whitespace()
        .find_map(|f| f.strip_prefix("RESULT="))
        .unwrap_or("MISSING");
    if !reply.starts_with("HELLO REPLY") || result != "OK" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SAM bridge on 127.0.0.1:{port} refused HELLO: {}",
                scrub_control_line(&reply)
            ),
        ));
    }
    Ok((stream, reply))
}

/// Longest `STREAM STATUS` line accepted. The result and an optional message
/// are short; a router that sends more than this is not answering us.
const MAX_STATUS_LINE: usize = 1024;

/// Open an outbound virtual stream to `peer`, by speaking SAM ourselves.
///
/// A stream is its own connection: dial the bridge, `HELLO VERSION`, `STREAM
/// CONNECT`, and the socket being held *is* the stream. Nothing is shared
/// between dials, so one failure cannot affect another.
///
/// Owning the socket is what makes two things possible:
///
/// - **The dial timeout is real.** It bounds the wait for `STREAM STATUS`,
///   which is where a leaseSet lookup spends its time.
/// - **The stream is closeable and boundable.** The returned
///   [`ForwardedStream`] takes read and write timeouts like any other socket,
///   and dropping it closes it.
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
) -> Result<ForwardedStream, DialFailed> {
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
    let deadline = Instant::now() + timeout;
    let status = read_sam_line(
        &mut stream,
        port,
        MAX_STATUS_LINE,
        Some(deadline),
        "STREAM CONNECT",
    )?;
    expect_stream_ok(&status, &format!("the stream to {}", peer.to_b32()))?;

    // Handshake done; the socket is now the peer stream. Clear the deadlines
    // the handshake needed — the engine sets its own per-peer timeouts, and a
    // stray one here would look to it like a peer that went quiet.
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(ForwardedStream::from_socket(stream))
}

/// The `STREAM STATUS` result word for "no session has this id".
const INVALID_ID: &str = "INVALID_ID";

/// Why a dial produced no stream, and — the part that matters — whether the
/// blame lies with the peer or with the session itself.
///
/// The distinction is not cosmetic. A dial attaches to a session by id, so
/// exactly one refusal means every future dial is doomed too, and treating it
/// as "this peer, this time" is how a daemon spends hours announcing into a
/// session the router destroyed (see [`DialFailed::SessionGone`]).
enum DialFailed {
    /// `RESULT=INVALID_ID`: the router has no session under this id.
    ///
    /// Not a peer failure and not retryable — the session is gone, and no
    /// dial, announce or lookup attached to it can succeed again. The router
    /// is under no obligation to have told us first: it may have destroyed
    /// the session without a word and without closing the control
    /// connection, in which case this refusal is the only evidence there is.
    SessionGone(io::Error),
    /// Anything else: an unreachable peer, a timeout, a bridge that misbehaved
    /// on the way. Ordinary on I2P, and says nothing about the session.
    Peer(io::Error),
}

impl From<io::Error> for DialFailed {
    /// Transport failures on the way to a `STREAM STATUS` are the dial's, not
    /// the session's: a bridge answering badly is not the router telling us
    /// our session is gone.
    fn from(e: io::Error) -> DialFailed {
        DialFailed::Peer(e)
    }
}

impl From<DialFailed> for io::Error {
    fn from(failed: DialFailed) -> io::Error {
        match failed {
            DialFailed::SessionGone(e) | DialFailed::Peer(e) => e,
        }
    }
}

/// Require a `STREAM STATUS RESULT=OK`, naming `what` was refused otherwise.
///
/// The router's own result word, and its `MESSAGE` when it sent one.
/// `CANT_REACH_PEER` and friends are ordinary on I2P and must read as "this
/// peer, this time" rather than as a fault in the session — with the single
/// exception of `INVALID_ID`, which is the opposite, and is separated out
/// here so that no caller can accidentally treat it as ordinary.
fn expect_stream_ok(status: &str, what: &str) -> Result<(), DialFailed> {
    if !status.starts_with("STREAM STATUS") {
        return Err(DialFailed::Peer(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SAM bridge answered with {:?} where a STREAM STATUS belongs",
                scrub_control_line(status)
            ),
        )));
    }
    let result = status
        .split_whitespace()
        .find_map(|f| f.strip_prefix("RESULT="))
        .unwrap_or("MISSING");
    if result != "OK" {
        // The router's words reach a terminal, so they are scrubbed like every
        // other control line. `MESSAGE=` runs to the end of the line and is
        // scrubbed as one, rather than trusted to be only a message.
        let error = io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "router refused {what}: {}{}",
                result.chars().map(scrub_char).collect::<String>(),
                status
                    .split_once("MESSAGE=")
                    .map(|(_, m)| format!(" ({})", scrub_control_line(m.trim_matches('"'))))
                    .unwrap_or_default()
            ),
        );
        return Err(if result == INVALID_ID {
            DialFailed::SessionGone(error)
        } else {
            DialFailed::Peer(error)
        });
    }
    Ok(())
}

/// A SAM session id unlikely to collide with one already registered.
///
/// SAM session ids are per-router, not per-connection, and a router does not
/// necessarily free one the instant our control socket closes: at least one
/// holds it long enough that a process starting seconds after a clean exit is
/// refused with `DuplicateId` (`docs/PROTOCOL.i2p-bt` §2.9).
///
/// This matters beyond a one-off run: the SCOPE §4 reconnect discipline
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
///
/// `Debug` is written by hand so that [`persistent_key`](SamConfig::persistent_key)
/// prints as `Some(<redacted>)`: a derived one would put the whole private
/// key blob into any `{config:?}` on an error path, which is the class of
/// leak `SECURITY.md` ranks highest.
#[derive(Clone)]
pub struct SamConfig {
    /// SAM control port on `127.0.0.1`.
    pub samv3_tcp_port: u16,
    /// SAM session nickname.
    pub nickname: String,
    /// Base64 private key for a stable identity (Q4). `None` requests a
    /// transient destination.
    pub persistent_key: Option<String>,
    /// How long the `HELLO VERSION` handshake waits for the bridge to answer
    /// before giving up on this attempt. Raise it for a router that is slow to
    /// come up; the cost of a low value is a wasted backoff cycle, the cost of
    /// no value at all is a daemon that hangs.
    pub probe_timeout: Duration,
    /// How long `SESSION CREATE` may take. Much longer than
    /// [`probe_timeout`](SamConfig::probe_timeout): answering it means the
    /// router has built the destination's tunnels.
    pub session_timeout: Duration,
}

impl Default for SamConfig {
    fn default() -> Self {
        SamConfig {
            samv3_tcp_port: DEFAULT_SAM_PORT,
            nickname: "clove".to_owned(),
            persistent_key: None,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            session_timeout: DEFAULT_SESSION_TIMEOUT,
        }
    }
}

impl std::fmt::Debug for SamConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// Stands in for the key: `Some(<redacted>)` says a key is set,
        /// which is all a diagnostic needs.
        struct Redacted;
        impl std::fmt::Debug for Redacted {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("<redacted>")
            }
        }
        f.debug_struct("SamConfig")
            .field("samv3_tcp_port", &self.samv3_tcp_port)
            .field("nickname", &self.nickname)
            .field(
                "persistent_key",
                &self.persistent_key.as_ref().map(|_| Redacted),
            )
            .field("probe_timeout", &self.probe_timeout)
            .field("session_timeout", &self.session_timeout)
            .finish()
    }
}

/// Whether a session's control connection is still up, why it ended, and the
/// last thing the router said on it.
///
/// The last line matters as much as the flag. When a session ends, the reason
/// is almost always something the router *said* first — a `SESSION STATUS`
/// carrying an `I2P_ERROR`, or Java I2P's `PONG timeout` — and clove used to
/// throw every one of those away unread. Four live runs died on a ~90 second
/// cycle and not one of them could say why, because the only code that ever
/// touched the control connection was a health probe that wrote a `PING`, read
/// exactly one line, and looked at nothing in it.
#[derive(Default)]
struct Life {
    /// `None` while the session is alive; why it ended once it is not.
    ended: Option<String>,
    /// The most recent line from the router that was not a `PING`.
    last_line: Option<String>,
}

/// The shared half of a session's liveness, between the watchdog thread and
/// everybody asking whether the session is worth handing work to.
#[derive(Default)]
struct SessionLife {
    state: Mutex<Life>,
    /// Signalled once, when the session ends.
    ended: Condvar,
}

impl SessionLife {
    fn lock(&self) -> MutexGuard<'_, Life> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn alive(&self) -> bool {
        self.lock().ended.is_none()
    }

    /// Remember a line the router volunteered, in case it turns out to have
    /// been the session's last words.
    ///
    /// Scrubbed on the way in, not on the way out: this ends up in the
    /// "router lost" line the daemon prints, and a control line can carry the
    /// `DESTINATION=` blob. Storing it raw would mean the only thing between
    /// the key and a log was remembering to scrub at every use.
    fn note(&self, line: &str) {
        self.lock().last_line = Some(scrub_control_line(line));
    }

    /// Record the session as ended. The first reason wins: what killed it is
    /// more informative than whatever noticed second.
    fn end(&self, why: &str) {
        let mut state = self.lock();
        if state.ended.is_none() {
            state.ended = Some(match &state.last_line {
                Some(line) => format!("{why}; the router's last words were {line:?}"),
                None => why.to_owned(),
            });
        }
        drop(state);
        self.ended.notify_all();
    }

    /// Block until the session ends, and say why.
    fn wait_end(&self) -> String {
        let state = self.lock();
        let state = self
            .ended
            .wait_while(state, |s| s.ended.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state
            .ended
            .clone()
            .unwrap_or_else(|| "the session ended".to_owned())
    }
}

/// Sits on the session's control connection for as long as it lives.
///
/// Two jobs, and clove had neither:
///
/// - **Answer the router's `PING`.** `SAMv3.2` lets the *router* ping the
///   client; Java I2P's `SAMv3Handler` does it on every read timeout and
///   answers an unanswered ping with `SESSION_ERROR "PONG timeout"`, killing
///   the session. Nothing in clove ever read the control connection, so
///   nothing could ever reply. i2pd does not ping, so this costs nothing
///   there — and is the whole session on Java I2P.
/// - **Notice, immediately and with a reason, when the session ends.** The
///   alternative is a torrent dialling into a session the router has already
///   destroyed, and a rebuild that can say only "router lost".
fn watch_control(mut socket: TcpStream, port: u16, alive: &SessionLife) {
    // A PONG must not park this thread forever if the bridge stops reading;
    // a bridge that will not take ten bytes in ten seconds is gone.
    let _ = socket.set_write_timeout(Some(PONG_WRITE_TIMEOUT));
    loop {
        let said = match read_sam_line(
            &mut socket,
            port,
            MAX_CONTROL_LINE,
            None,
            "the session control connection",
        ) {
            Ok(said) => said,
            Err(e) => {
                alive.end(&format!("the SAM control connection ended: {e}"));
                return;
            }
        };
        // `PING` alone or `PING <token>`; the token is echoed back verbatim,
        // which is what Java I2P compares against.
        if let Some(token) = ping_token(&said) {
            if socket
                .write_all(format!("PONG{token}\n").as_bytes())
                .is_err()
            {
                alive.end("the SAM control connection would not take a PONG");
                return;
            }
            continue;
        }
        // A `SESSION STATUS` *after* setup is the router reporting on the
        // session itself, and a non-OK one is it announcing the session is
        // over. Ending here rather than merely remembering the line is the
        // difference between a rebuild and a wedge: the router is not
        // required to close the control connection when it does this, and
        // when it does not, nothing else will ever notice.
        //
        // This is one step past the fix that installed this thread. That one
        // stopped clove throwing the router's account away — but it kept the
        // account as *text for a future log line*, so a session could be
        // pronounced dead by the router, filed away as a souvenir, and still
        // be handed every dial the daemon made.
        if let Some(result) = session_status_result(&said)
            && result != "OK"
            && result != "MISSING"
        {
            alive.note(&said);
            alive.end("the router ended the session");
            return;
        }
        alive.note(&said);
    }
}

/// The `RESULT=` word of a `SESSION STATUS` line, or `None` when the line is
/// not one.
///
/// Deliberately strict about the prefix, in both directions: `SESSION STATUS`
/// and `SESSION STATUS RESULT=…` are the router talking about our session,
/// while a hypothetical `SESSION STATUSES` is not, and acting on it would be
/// acting on something we did not understand.
///
/// A `SESSION STATUS` carrying no `RESULT=` at all reports `MISSING`, which
/// the caller treats as *not* a death: it is malformed rather than fatal, and
/// a session that is genuinely gone will prove it on the next dial
/// ([`DialFailed::SessionGone`]) rather than having to be guessed at here.
fn session_status_result(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("SESSION STATUS")?;
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    Some(
        rest.split_whitespace()
            .find_map(|f| f.strip_prefix("RESULT="))
            .unwrap_or("MISSING"),
    )
}

/// The echo-back part of a router `PING`, or `None` when the line is not one.
///
/// `PING` alone and `PING <token>` are both pings; `PINGER` is not, and
/// answering it would be answering something we did not understand.
fn ping_token(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("PING")?;
    (rest.is_empty() || rest.starts_with(' ')).then_some(rest)
}

/// A live SAM stream session: our destination plus the control connection,
/// used for outbound streams and naming.
pub struct SamSession {
    /// The session's control connection. The router destroys the session when
    /// this closes, so it is held for the session's whole life; it is read by
    /// the watchdog thread and written by nothing else.
    control: TcpStream,
    life: Arc<SessionLife>,
    local: DestHash,
    local_b64: String,
    /// The session's **private key blob** — the whole `DESTINATION=` field,
    /// of which `local_b64` is only the public prefix. Kept so a persistent
    /// identity (Q4) can be written to disk once and replayed on every later
    /// `SESSION CREATE`. Never logged, never published, never serialized: see
    /// [`SamSession::private_key_b64`].
    private_key_b64: String,
    samv3_tcp_port: u16,
    probe_timeout: Duration,
    /// The SAM session id every outbound stream attaches itself to.
    nickname: String,
}

impl SamSession {
    /// Establish the session against the router named by `config`.
    ///
    /// Both halves of the exchange are bounded. `HELLO VERSION` gets
    /// [`SamConfig::probe_timeout`] and `SESSION CREATE` gets
    /// [`SamConfig::session_timeout`], on a socket clove owns — which is what
    /// closes the residual hang in `PROTOCOL.i2p-bt` §2.7, where a bridge that
    /// answered `HELLO` and then stalled blocked the daemon forever with
    /// nothing above it able to back off, retry, or log.
    ///
    /// # Errors
    ///
    /// The router is unreachable, did not answer in time, refused the session,
    /// or returned a destination we cannot parse.
    pub fn connect(config: &SamConfig) -> io::Result<SamSession> {
        let port = config.samv3_tcp_port;
        let (mut control, hello) = sam_hello(port, config.probe_timeout, "HELLO")?;
        debug_assert!(hello.contains("RESULT=OK"));

        let destination = config.persistent_key.as_deref().unwrap_or("TRANSIENT");
        // The parameter set a live i2pd accepts today. A destination's tunnel
        // shape and lease-set encryption are not changed casually.
        let command = format!(
            "SESSION CREATE STYLE=STREAM ID={} DESTINATION={destination} \
             i2cp.leaseSetEncType=6,4 inbound.length=3 inbound.quantity=2 \
             outbound.length=3 outbound.quantity=2 SIGNATURE_TYPE=7\n",
            config.nickname
        );
        control.write_all(command.as_bytes())?;

        control.set_read_timeout(Some(config.session_timeout))?;
        let deadline = Instant::now() + config.session_timeout;
        let status = read_sam_line(
            &mut control,
            port,
            MAX_SESSION_LINE,
            Some(deadline),
            "SESSION CREATE",
        )?;
        let blob = parse_session_status(&status)?;

        // What SAM hands back is the session's *private key blob*, not its
        // destination (SAMv3 SESSION STATUS). Everything clove publishes must
        // be the public destination at the front of it — the hash we call
        // ourselves by, and the base64 an announce carries. Sending the rest
        // to a tracker means sending our private keys to a stranger, which is
        // exactly what clove did until 2026-07-27 (`PROTOCOL.i2p-bt` §5.1c).
        let bytes = crate::addr::destination_bytes(blob).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SAM returned a destination clove cannot parse",
            )
        })?;
        let local = DestHash::from_b64_destination(blob).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SAM returned an unparseable destination",
            )
        })?;
        let local_b64 = crate::addr::i2p_base64_encode(&bytes);
        let private_key_b64 = blob.to_owned();

        // Setup is over; the watchdog owns the reading end from here.
        control.set_read_timeout(None)?;
        let life = Arc::new(SessionLife::default());
        let watched = control.try_clone()?;
        let watch_life = Arc::clone(&life);
        std::thread::spawn(move || {
            watch_control(watched, port, &watch_life);
            // Belt and braces: however that returned, the session is over, and
            // a supervisor waiting on this must not wait forever.
            watch_life.end("the session watchdog stopped");
        });

        Ok(SamSession {
            control,
            life,
            local,
            local_b64,
            private_key_b64,
            samv3_tcp_port: port,
            probe_timeout: config.probe_timeout,
            nickname: config.nickname.clone(),
        })
    }

    /// Our full base64 destination — what tracker announces carry as `ip`
    /// (`docs/PROTOCOL.i2p-bt` §5.1).
    #[must_use]
    pub fn local_dest_b64(&self) -> &str {
        &self.local_b64
    }

    /// The session's private key blob, for persisting the identity (Q4).
    ///
    /// **This is secret material.** SAM's `DESTINATION=` field is the private
    /// crypto and signing keys with the public destination on the front
    /// (`docs/PROTOCOL.i2p-bt` §5.1c); handing the whole thing to a tracker is
    /// what §5.1c is *about*. Anything published — an announce's `ip=`, a PEX
    /// message, a log line, the control API — wants
    /// [`local_dest_b64`](SamSession::local_dest_b64) instead, which is the
    /// public prefix and nothing else.
    ///
    /// The only legitimate caller is the daemon writing
    /// `<data_dir>/destination.key` at `0600`.
    #[must_use]
    pub fn private_key_b64(&self) -> &str {
        &self.private_key_b64
    }

    /// Whether the session's control connection is still up.
    ///
    /// Free, and true up to the instant the router hangs up: the watchdog
    /// thread is already sitting on the connection, so there is nothing to
    /// probe and no probe interval to be wrong about.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.life.alive()
    }

    /// Block until this session's control connection ends, and return the
    /// router's account of why — the line an operator needs and the one no
    /// live run has ever produced.
    #[must_use]
    pub fn wait_until_lost(&self) -> String {
        self.life.wait_end()
    }

    /// This session's own destination hash — the identity peers reach us at
    /// and the one announced to trackers.
    #[must_use]
    pub fn local_dest(&self) -> DestHash {
        self.local
    }
}

impl Drop for SamSession {
    fn drop(&mut self) {
        // Closing the control connection is how a session is ended in SAMv3,
        // and it is also what unblocks the watchdog's read. Without it the
        // router holds the session — and its nickname — after clove has moved
        // on, which is the DuplicateId hazard of §2.9 from the other side.
        let _ = self.control.shutdown(Shutdown::Both);
    }
}

/// Make a line the router sent safe to repeat in an error or a log.
///
/// A SAM control line is the one place clove handles text that may contain the
/// *private* half of its identity: `SESSION STATUS RESULT=OK DESTINATION=…`
/// carries the full key blob, and that line — or a malformed one meant to be
/// it — used to be embedded verbatim in the error we then printed.
/// `SECURITY.md` puts "the destination key, or any part of the SAM
/// `DESTINATION=` blob behind it, reaching … a log" in scope, and rightly: a
/// bridge that answers oddly is a bug to report, and a bug report is exactly
/// where that line ends up.
///
/// A hostile bridge already holds the key — it generated it — so this is not
/// defence against the bridge. It is defence against the key travelling
/// further than the machine it was made on, which is what logs do.
///
/// Two rules, because the field name is not enough on its own:
///
/// 1. a field whose *name* says it carries key material loses its value; and
/// 2. any long run of I2P base64 loses itself, whatever field it turned up in
///    — a bridge that puts a key somewhere unexpected is exactly the bridge
///    worth defending against, and by definition it is not going to label it.
///
/// `RESULT` and `MESSAGE` survive: they are the router explaining why a session
/// died, which is the entire reason this text is kept, and neither is a place a
/// key belongs. Control characters go regardless — this reaches a terminal.
fn scrub_control_line(line: &str) -> String {
    /// Field names whose values are, or may contain, private key material.
    const SECRET_FIELDS: &[&str] = &["DESTINATION", "PRIVKEY", "PRIVATEKEY", "KEY"];
    /// Shortest run of I2P base64 treated as a key rather than a word. A
    /// destination is ~516 characters; no diagnostic English gets near this.
    const BLOB: usize = 64;

    let mut out = String::with_capacity(line.len());
    for (i, field) in line.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match field.split_once('=') {
            Some((key, _)) if SECRET_FIELDS.contains(&key.to_ascii_uppercase().as_str()) => {
                out.extend(key.chars().map(scrub_char));
                out.push_str("=<redacted>");
            }
            _ if looks_like_key_material(field, BLOB) => out.push_str("<redacted>"),
            _ => out.extend(field.chars().map(scrub_char)),
        }
    }
    out
}

/// Whether `field` contains an unbroken run of at least `min` I2P base64
/// characters — the shape of a destination or a key blob, in any field.
fn looks_like_key_material(field: &str, min: usize) -> bool {
    let mut run = 0usize;
    for c in field.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '~' | '=') {
            run += 1;
            if run >= min {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// One character's worth of the above.
///
/// The same two families `clove_core::text::scrub` replaces, and deliberately
/// not a call to it: `clove-core` depends on this crate, not the other way
/// round, so the shared helper is not reachable from here. The duplication is
/// four lines and the alternative is a text utility living in the crate whose
/// entire job is sockets.
pub(crate) fn scrub_char(c: char) -> char {
    match c {
        c if c.is_control() => '?',
        // The bidirectional overrides and isolates draw nothing and reorder the
        // text around them, so a bridge could make the rest of a log line read
        // as something else. Not caught by `is_control`.
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '?',
        c => c,
    }
}

/// Pull the destination blob out of a `SESSION STATUS` reply, or say what the
/// router said instead.
fn parse_session_status(status: &str) -> io::Result<&str> {
    if !status.starts_with("SESSION STATUS") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SAM bridge answered SESSION CREATE with {:?}",
                scrub_control_line(status)
            ),
        ));
    }
    let result = status
        .split_whitespace()
        .find_map(|field| field.strip_prefix("RESULT="))
        .unwrap_or("MISSING");
    if result != "OK" {
        // The router's own result word: DUPLICATED_ID, I2P_ERROR and friends
        // call for entirely different actions, and "session refused" calls for
        // none of them.
        //
        // `MESSAGE=` runs to the end of the line, so a router that puts
        // anything after it — including a `DESTINATION=` — puts it in here too.
        // Scrubbed as a whole line rather than trusted to be only a message.
        return Err(io::Error::other(format!(
            "router refused the session: {}{}",
            result.chars().map(scrub_char).collect::<String>(),
            status
                .split_once("MESSAGE=")
                .map(|(_, m)| format!(" ({})", scrub_control_line(m.trim_matches('"'))))
                .unwrap_or_default()
        )));
    }
    status
        .split_whitespace()
        .find_map(|field| field.strip_prefix("DESTINATION="))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SESSION STATUS said OK but carried no DESTINATION",
            )
        })
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
    /// The socket `STREAM FORWARD` was issued on. `SAMv3` keeps forwarding
    /// only while it is open, so this is held, never used — and dropping the
    /// listener is therefore how forwarding stops.
    _forward: TcpStream,
    /// Keeps the SAM session (and thus the destination) alive — and is told
    /// when the listener itself fails, see [`I2pListener::accept`] below.
    session: Arc<SamSession>,
}

impl SamListener {
    /// Ask the router to forward inbound streams for `session`'s destination
    /// to a fresh loopback listener, and return it.
    ///
    /// `SILENT=false`: the router then prepends each forwarded connection with
    /// the peer's base64 destination line, which is the only way we learn who
    /// dialled us (`docs/PROTOCOL.i2p-bt` §1.3, §2.5).
    ///
    /// # Errors
    ///
    /// The loopback listener cannot be bound, the bridge does not answer, or
    /// the router refuses the `STREAM FORWARD` request.
    pub fn forward(session: Arc<SamSession>) -> io::Result<SamListener> {
        // The one inbound IP-socket construction site: loopback by
        // construction (Layer 1, SCOPE §5), ephemeral port.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let sam_port = session.samv3_tcp_port;
        let timeout = session.probe_timeout;

        // Its own connection, per SAMv3: the socket that carries STREAM
        // FORWARD is dedicated to it for as long as forwarding lasts.
        let (mut forward, _) = sam_hello(sam_port, timeout, "HELLO (for STREAM FORWARD)")?;
        let command = format!(
            "STREAM FORWARD ID={} PORT={port} SILENT=false\n",
            session.nickname
        );
        forward.write_all(command.as_bytes())?;
        let deadline = Instant::now() + timeout;
        let status = read_sam_line(
            &mut forward,
            sam_port,
            MAX_STATUS_LINE,
            Some(deadline),
            "STREAM FORWARD",
        )?;
        expect_stream_ok(&status, "STREAM FORWARD")?;
        forward.set_read_timeout(None)?;

        let local = session.local;
        Ok(SamListener {
            listener,
            local,
            port,
            _forward: forward,
            session,
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

/// A connected loopback TCP pair: the shape every socket this crate opens has.
///
/// Test support for `cloved`'s confinement test, which has to make a real
/// `AF_INET` connection under the live seccomp filter to prove the allowlist
/// and its argument restriction let the SAM transport through. Behind the
/// `mock` feature, so it is not in the daemon, and here rather than in the
/// caller because Layer 1 keeps socket construction in this crate.
///
/// # Errors
///
/// The bind, connect or accept fails.
#[cfg(feature = "mock")]
pub fn loopback_pair() -> io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let client = TcpStream::connect(listener.local_addr()?)?;
    let (server, _) = listener.accept()?;
    Ok((client, server))
}

impl I2pListener for SamListener {
    type Stream = ForwardedStream;

    fn local_dest(&self) -> DestHash {
        self.local
    }

    fn accept(&self) -> io::Result<Option<(ForwardedStream, DestHash)>> {
        accept_forwarded(&self.listener).map_err(|e| {
            // An error here is the listener's, and nobody above rebuilds a
            // listener on its own: the demux thread returns, and the session
            // goes on reporting itself healthy with no inbound service behind
            // it. Ending the session is what turns that into a rebuild.
            self.session
                .life
                .end(&format!("the forward listener failed: {e}"));
            e
        })
    }
}

/// Linux's numbers for the `accept(2)` errors std gives no [`io::ErrorKind`]
/// of their own. The daemon is Linux-only (Landlock, seccomp); anywhere else
/// a mismatch only means the error is treated as the listener's.
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;
const ENOBUFS: i32 = 105;

/// How long to stand back when the process or the system is out of file
/// descriptors before accepting again. The connection is not lost — it waits
/// in the backlog — and spinning on `EMFILE` burns the CPU that the peer
/// threads need in order to close something.
const FD_EXHAUSTED_PAUSE: Duration = Duration::from_millis(50);

/// Sort one `accept(2)` result by who the error, if any, is about.
///
/// `Ok(None)` is an error that cost this one connection and says nothing
/// about the listener: the process is out of descriptors (`EMFILE`, `ENFILE`),
/// the kernel is out of memory or buffers (`ENOMEM`, `ENOBUFS`), the peer hung
/// up between arriving and being accepted (`ECONNABORTED`), or a signal landed
/// (`EINTR`). Every one of these used to end the accept loop — and with it
/// inbound peer service for the rest of the session, while `clove status` went
/// on saying `connected` — and descriptor exhaustion is reachable from a swarm:
/// a thread and a socket per peer, a file per torrent. `Err` is kept for the
/// errors that are the listener's own (`EBADF`, `EINVAL`, and anything
/// unfamiliar), after which accepting again would only fail the same way.
fn usable_accept(accepted: io::Result<TcpStream>) -> io::Result<Option<TcpStream>> {
    let e = match accepted {
        Ok(socket) => return Ok(Some(socket)),
        Err(e) => e,
    };
    let out_of_descriptors = matches!(e.raw_os_error(), Some(EMFILE | ENFILE));
    let transient = out_of_descriptors
        || e.raw_os_error() == Some(ENOBUFS)
        || matches!(
            e.kind(),
            io::ErrorKind::Interrupted
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::OutOfMemory
        );
    if !transient {
        return Err(e);
    }
    if out_of_descriptors {
        std::thread::sleep(FD_EXHAUSTED_PAUSE);
    }
    Ok(None)
}

/// One forwarded connection off `listener`, with its destination header read.
///
/// `Ok(None)` for anything that cost only this connection: a transient accept
/// error ([`usable_accept`]), a socket that will not take a read timeout, or a
/// header that never arrives, arrives as garbage, or belongs to something that
/// is not the router at all — anything on the loopback forward port can
/// produce one, including our own [`poke_listener`]. `Err` is the listener's.
fn accept_forwarded(listener: &TcpListener) -> io::Result<Option<(ForwardedStream, DestHash)>> {
    let Some(mut stream) = usable_accept(listener.accept().map(|(socket, _addr)| socket))? else {
        return Ok(None);
    };
    // Bound the header read so a silent/misbehaving router cannot wedge the
    // acceptor; then hand a blocking socket to the reader thread. A socket
    // that refuses the option is this connection's problem, not the listener's.
    if stream.set_read_timeout(Some(DEST_LINE_TIMEOUT)).is_err() {
        return Ok(None);
    }
    // The same budget again as a whole-line deadline: the read timeout is per
    // byte, and a header dribbled a byte at a time is otherwise unbounded.
    let deadline = Instant::now() + DEST_LINE_TIMEOUT;
    let Ok(dest) = read_dest_line(&mut stream, MAX_DEST_LINE, Some(deadline)) else {
        return Ok(None);
    };
    if stream.set_read_timeout(None).is_err() {
        return Ok(None);
    }
    Ok(Some((ForwardedStream { inner: stream }, dest)))
}

/// Read the `SILENT=false` destination header the router prepends to a
/// forwarded connection — the peer's base64 destination, optionally followed
/// by space-separated `FROM_PORT`/`TO_PORT` params — up to the `\n`, and
/// derive the peer's [`DestHash`]. Reads one byte at a time so the stream
/// payload after the newline is left untouched for the peer's reader.
///
/// `deadline` bounds the whole line, as [`read_sam_line`]'s does, and for
/// the same reason: the socket's read timeout is per byte, so without it one
/// byte every 29 seconds held this open for `max_len` reads — on the one
/// acceptor thread, before any per-connection thread exists, from anything
/// that can reach the loopback forward port. A read already in progress when
/// the deadline passes still gets its own timeout, so the true bound is the
/// deadline plus one read.
fn read_dest_line<R: Read>(
    reader: &mut R,
    max_len: usize,
    deadline: Option<Instant>,
) -> io::Result<DestHash> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "forwarded stream did not finish its destination line in time",
            ));
        }
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
    type Closer = TcpStream;

    fn split(self) -> io::Result<(TcpStream, TcpStream)> {
        let reader = self.inner.try_clone()?;
        Ok((reader, self.inner))
    }

    /// A duplicated descriptor onto the same loopback socket: `shutdown` on it
    /// tears down the connection whichever half a thread is parked on.
    fn closer(&self) -> io::Result<TcpStream> {
        self.inner.try_clone()
    }

    /// Real timeouts: this is a loopback TCP socket from the router.
    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        ForwardedStream::set_timeouts(self, timeout)
    }
}

impl I2pDialer for SamSession {
    type Stream = ForwardedStream;

    /// Dial `peer` on a socket of our own (see the private `dial_stream`).
    ///
    /// No session mutex is taken and no shared state is touched, so dials are
    /// genuinely concurrent. The session is consulted for exactly one thing,
    /// its nickname, which SAM needs to attach the new stream to the right
    /// session.
    fn dial(&self, peer: DestHash, timeout: Duration) -> io::Result<ForwardedStream> {
        dial_stream(self.samv3_tcp_port, &self.nickname, peer, timeout).map_err(|failed| {
            // `INVALID_ID` is the router saying this session no longer
            // exists, and it is frequently the only way it says so: the
            // control connection can stay open and silent over a session the
            // router has already destroyed, leaving the watchdog nothing to
            // read and nothing to report.
            //
            // Ending the session here is what turns that from permanent into
            // a reconnect. Without it every announce and every peer dial
            // fails identically and forever, `clove status` still reports
            // `connected`, and the only cure is restarting the daemon —
            // which is exactly the flakiness SCOPE §4 exists to rule out.
            if let DialFailed::SessionGone(e) = &failed {
                self.life
                    .end(&format!("the router no longer has our session ({e})"));
            }
            failed.into()
        })
    }

    /// A dial attaches to the session by nickname, so a session the router has
    /// destroyed cannot carry one however healthy the bridge is. Callers about
    /// to spend a thread and a socket on best-effort work — a goodbye announce
    /// during teardown — ask this first.
    fn usable(&self) -> bool {
        self.healthy()
    }
}

/// Cap on a `NAMING REPLY` line. It carries a whole destination — around 516
/// base64 characters for the common key types, more for larger certificates —
/// so it needs far more room than a status line, but not an unbounded amount.
const MAX_NAMING_LINE: usize = 8192;

/// Longest name accepted for a lookup. A b32 address is 60 characters and no
/// registered hostname comes near this.
const MAX_LOOKUP_NAME: usize = 255;

/// Refuse a name that cannot go into a `NAMING LOOKUP` as a single field.
///
/// SAM is a line protocol of space-separated fields, so a name carrying a space
/// or a newline is not one argument — it is a second command of the sender's
/// choosing. Announce URLs reach here out of `.torrent` files written by
/// strangers, and this crate does not assume the parser two crates away already
/// refused them (SCOPE §5: no layer assumes another is present).
fn check_lookup_name(name: &str) -> io::Result<()> {
    let bad = |why: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "not a name clove will look up ({why}): {:?}",
                name.chars().map(scrub_char).collect::<String>()
            ),
        )
    };
    if name.is_empty() {
        return Err(bad("empty"));
    }
    if name.len() > MAX_LOOKUP_NAME {
        return Err(bad("longer than a hostname"));
    }
    if name.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(bad("contains a space or a control character"));
    }
    Ok(())
}

/// Pull the destination out of a `NAMING REPLY`, or say what the router said
/// instead.
///
/// The success shape is `NAMING REPLY RESULT=OK NAME=<name> VALUE=<destination>`.
/// A destination contains no spaces, so the field split is exact.
fn parse_naming_reply<'a>(reply: &'a str, name: &str) -> io::Result<&'a str> {
    let asked = name.chars().map(scrub_char).collect::<String>();
    if !reply.starts_with("NAMING REPLY") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "SAM bridge answered {:?} where a NAMING REPLY belongs",
                scrub_control_line(reply)
            ),
        ));
    }
    let result = reply
        .split_whitespace()
        .find_map(|f| f.strip_prefix("RESULT="))
        .unwrap_or("MISSING");
    if result != "OK" {
        // The result word is the router's text too, and reaches a terminal.
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "router could not resolve {asked}: {}",
                result.chars().map(scrub_char).collect::<String>()
            ),
        ));
    }
    reply
        .split_whitespace()
        .find_map(|f| f.strip_prefix("VALUE="))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("router resolved {asked} but sent no destination"),
            )
        })
}

/// Resolve `name` through the bridge, on a socket of its own.
///
/// One `HELLO VERSION`, one `NAMING LOOKUP`, one reply: no session is involved,
/// so nothing here can disturb one.
fn naming_lookup(port: u16, name: &str, timeout: Duration) -> io::Result<DestHash> {
    check_lookup_name(name)?;
    let (mut stream, _) = sam_hello(port, timeout, "HELLO (for NAMING LOOKUP)")?;
    stream.write_all(format!("NAMING LOOKUP NAME={name}\n").as_bytes())?;

    let deadline = Instant::now() + timeout;
    let reply = read_sam_line(
        &mut stream,
        port,
        MAX_NAMING_LINE,
        Some(deadline),
        "NAMING LOOKUP",
    )?;
    let value = parse_naming_reply(&reply, name)?;
    DestHash::from_b64_destination(value).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "router resolved {} to something that is not a destination",
                name.chars().map(scrub_char).collect::<String>()
            ),
        )
    })
}

impl I2pNamingLookup for SamSession {
    fn lookup(&self, name: &str) -> io::Result<DestHash> {
        naming_lookup(self.samv3_tcp_port, name, self.probe_timeout)
    }
}

#[cfg(test)]
mod tests {
    //! Router-free coverage. A *working* SAM session needs a live router and
    //! so cannot be exercised here, but everything about a router that is
    //! **not** working can be, and that is the half that decides whether the
    //! daemon degrades or wedges.
    //!
    //! Three groups:
    //!
    //! - The inbound path's pure logic: splitting the forwarded socket and
    //!   parsing the `SILENT=false` destination header.
    //! - The `HELLO VERSION` exchange against a fake bridge that lies,
    //!   stalls, floods or dies — SCOPE §9's "SAM bridge lying or dying
    //!   mid-operation".
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

        let got = read_dest_line(&mut cursor, MAX_DEST_LINE, Some(generous())).unwrap();
        assert_eq!(got, expected);

        // The payload after the newline must be intact for the peer reader.
        let mut rest = Vec::new();
        cursor.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"the-bittorrent-handshake");
    }

    #[test]
    fn read_dest_line_rejects_overlong_line() {
        let mut cursor = Cursor::new(vec![b'A'; 128]); // no newline, no dest
        let err = read_dest_line(&mut cursor, 32, Some(generous())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_dest_line_eof_before_newline() {
        let mut cursor = Cursor::new(b"partial-no-newline".to_vec());
        let err = read_dest_line(&mut cursor, MAX_DEST_LINE, Some(generous())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A deadline no test here will reach, for the cases about something else.
    fn generous() -> Instant {
        Instant::now() + DEST_LINE_TIMEOUT
    }

    /// The header must arrive whole within the deadline. The per-byte socket
    /// timeout alone let a byte every 29 s hold `accept` for `MAX_DEST_LINE`
    /// reads — some 34 hours — on the one acceptor thread, and any local
    /// process can reach the forward port.
    #[test]
    fn a_dribbled_destination_line_is_cut_off_at_the_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut dribbler = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        std::thread::spawn(move || {
            // One byte every so often, never a newline; ends when the test
            // drops its end of the socket.
            while dribbler.write_all(b"A").is_ok() {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        // A per-read timeout far past the deadline, so only the deadline can
        // end this.
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let budget = Duration::from_millis(200);
        let started = Instant::now();
        let err = read_dest_line(&mut server, MAX_DEST_LINE, Some(started + budget)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        let took = started.elapsed();
        assert!(took >= budget, "gave up before the deadline: {took:?}");
        assert!(
            took < Duration::from_secs(2),
            "the deadline did not end the read: {took:?}"
        );
    }

    /// The `SILENT=false` header of a forwarded connection, and the hash the
    /// peer behind it must be known by.
    fn forwarded_header() -> (String, DestHash) {
        let mut dest_bytes = vec![0x42u8; 384];
        dest_bytes.extend_from_slice(&[0x05, 0x00, 0x04, 0x00, 0x07, 0x00, 0x00]);
        let b64 = i2p_base64_encode(&dest_bytes);
        let hash = DestHash::from_b64_destination(&b64).unwrap();
        (format!("{b64} FROM_PORT=6881 TO_PORT=0\n"), hash)
    }

    /// Every accept error that cost one connection is `Ok(None)`; only the
    /// listener's own errors are `Err`. Before this, `EMFILE` — reachable
    /// from a swarm — ended inbound service for the rest of the session.
    #[test]
    fn a_transient_accept_error_costs_one_connection_not_the_listener() {
        const EBADF: i32 = 9;
        const EINVAL: i32 = 22;

        for errno in [EMFILE, ENFILE, ENOBUFS] {
            let got = usable_accept(Err(io::Error::from_raw_os_error(errno)));
            assert!(matches!(got, Ok(None)), "errno {errno} ended the listener");
        }
        for kind in [
            io::ErrorKind::Interrupted,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::OutOfMemory,
        ] {
            let got = usable_accept(Err(io::Error::from(kind)));
            assert!(matches!(got, Ok(None)), "{kind:?} ended the listener");
        }
        // Out of descriptors: stand back rather than spin.
        let started = Instant::now();
        assert!(matches!(
            usable_accept(Err(io::Error::from_raw_os_error(EMFILE))),
            Ok(None)
        ));
        assert!(
            started.elapsed() >= FD_EXHAUSTED_PAUSE,
            "no pause on EMFILE"
        );

        for errno in [EBADF, EINVAL] {
            let e = usable_accept(Err(io::Error::from_raw_os_error(errno)))
                .expect_err("a listener error was read as one bad connection");
            assert_eq!(e.raw_os_error(), Some(errno), "the error must survive");
        }
        assert!(usable_accept(Err(io::Error::other("something new"))).is_err());
    }

    /// The loop as the demux runs it: `EMFILE` once, then a real forwarded
    /// connection, which must still be accepted.
    #[test]
    fn an_accept_loop_survives_emfile_and_takes_the_next_connection() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (header, expected) = forwarded_header();
        let mut peer = TcpStream::connect(addr).unwrap();
        peer.write_all(header.as_bytes()).unwrap();

        let script = [
            Err(io::Error::from_raw_os_error(EMFILE)),
            listener.accept().map(|(socket, _)| socket),
        ];
        let mut accepted = 0;
        for result in script {
            match usable_accept(result) {
                Ok(Some(_socket)) => accepted += 1,
                Ok(None) => {}
                Err(e) => panic!("the accept loop ended: {e}"),
            }
        }
        assert_eq!(accepted, 1);

        // And the whole inbound path, header included, on a fresh connection.
        let mut peer = TcpStream::connect(addr).unwrap();
        peer.write_all(header.as_bytes()).unwrap();
        let (_stream, from) = accept_forwarded(&listener)
            .expect("the listener is fine")
            .expect("a forwarded connection with a header is usable");
        assert_eq!(from, expected);
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
    pub(super) enum Misbehaviour {
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
        /// A refusal whose `MESSAGE` quotes `RESULT=OK`, with a terminal
        /// escape for company: a substring match reads it as acceptance, and
        /// an unscrubbed error repeats the escape.
        ForgeOk,
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
    pub(super) fn fake_bridge(how: Misbehaviour) -> u16 {
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
                        Misbehaviour::ForgeOk => {
                            let _ = sock.write_all(
                                b"HELLO REPLY RESULT=NOVERSION MESSAGE=\"RESULT=OK\x1b[2J\"\n",
                            );
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

    /// The `HELLO VERSION` exchange on its own, reporting the version the
    /// bridge claimed. `SamSession::connect` opens with exactly this and then
    /// keeps the socket; testing it apart from `SESSION CREATE` is how a
    /// bridge that fails at the handshake is told from one that fails later.
    fn hello_version(port: u16, timeout: Duration) -> io::Result<String> {
        let (_socket, reply) = sam_hello(port, timeout, "HELLO")?;
        Ok(reply
            .split_whitespace()
            .find_map(|field| field.strip_prefix("VERSION="))
            .unwrap_or("unknown")
            .to_owned())
    }

    #[test]
    fn the_probe_refuses_every_bridge_that_is_not_one() {
        for how in [
            Misbehaviour::CloseImmediately,
            Misbehaviour::Silence,
            Misbehaviour::Garbage,
            Misbehaviour::Flood,
            Misbehaviour::RefuseHello,
            Misbehaviour::Dribble,
            Misbehaviour::ForgeOk,
        ] {
            let port = fake_bridge(how);
            let Some((result, elapsed)) = within(LIMIT, move || hello_version(port, PROBE)) else {
                panic!("{how:?}: the HELLO exchange never returned");
            };
            let err = result.expect_err(&format!("{how:?} was accepted as a SAM bridge"));
            assert!(elapsed < LIMIT, "{how:?}: probe took {elapsed:?}");
            // The message names the address it could not talk to.
            assert!(
                err.to_string().contains("127.0.0.1:"),
                "{how:?}: unhelpful error {err}"
            );
        }
    }

    #[test]
    fn connect_fails_in_bounded_time_against_a_broken_bridge() {
        // Every one of these must fail rather than block: a daemon stuck in
        // "connecting" with nothing in its log is worse than an error.
        for how in [
            Misbehaviour::CloseImmediately,
            Misbehaviour::Silence,
            Misbehaviour::Garbage,
            Misbehaviour::Flood,
            Misbehaviour::RefuseHello,
            Misbehaviour::Dribble,
            Misbehaviour::ForgeOk,
        ] {
            let port = fake_bridge(how);
            let config = SamConfig {
                samv3_tcp_port: port,
                probe_timeout: PROBE,
                session_timeout: PROBE,
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
            session_timeout: PROBE,
            ..Default::default()
        };
        let (result, elapsed) =
            within(LIMIT, move || SamSession::connect(&config)).expect("connect returned");
        assert!(result.is_err());
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    /// A bridge that answers `HELLO` correctly and *then* stalls on `SESSION
    /// CREATE` passes the handshake half, which is what makes it easy to miss.
    /// Both halves are bounded (`PROTOCOL.i2p-bt` §2.7).
    #[test]
    fn a_bridge_that_passes_hello_and_then_stalls_no_longer_hangs() {
        let port = fake_bridge(Misbehaviour::HelloThenStall);
        // The handshake half still passes, which is what made this invisible.
        assert_eq!(
            hello_version(port, PROBE).expect("a valid HELLO REPLY is accepted"),
            "3.3"
        );
        // And the session half now fails, in bounded time, saying so.
        let config = SamConfig {
            samv3_tcp_port: port,
            probe_timeout: PROBE,
            session_timeout: PROBE,
            ..Default::default()
        };
        // The session itself is dropped on the spot: what is under test is
        // that `connect` *returned*, and only the error is worth keeping.
        let (result, elapsed) = within(LIMIT, move || SamSession::connect(&config).map(drop))
            .expect("connect returned rather than hanging");
        let err = result.expect_err("a bridge that never answers SESSION CREATE is not a session");
        assert!(elapsed < LIMIT, "took {elapsed:?}");
        assert!(
            err.to_string().contains("SESSION CREATE"),
            "the error must name the exchange it died in: {err}"
        );
    }

    #[test]
    fn a_healthy_hello_is_accepted_and_its_version_reported() {
        let port = fake_bridge(Misbehaviour::HelloThenStall);
        assert_eq!(hello_version(port, PROBE).expect("hello accepted"), "3.3");
    }

    /// `RESULT=` is a field. A refusal whose `MESSAGE` quoted `RESULT=OK`
    /// passed the old substring check, and the raw line then reached the
    /// error — escape sequence and all.
    #[test]
    fn a_forged_ok_inside_a_refusal_is_still_a_refusal_and_is_scrubbed() {
        let port = fake_bridge(Misbehaviour::ForgeOk);
        let e = hello_version(port, PROBE)
            .expect_err("a refusal quoting RESULT=OK in its MESSAGE was accepted");
        assert!(e.to_string().contains("NOVERSION"), "{e}");
        assert!(!e.to_string().contains('\u{1b}'), "escape survived: {e}");
    }

    /// A `STREAM STATUS` is the router's text and reaches a terminal, in the
    /// result word, the message, and the line that is not a status at all.
    #[test]
    fn a_stream_status_cannot_forge_a_log_line() {
        for status in [
            "STREAM STATUS RESULT=I2P_ERROR\u{1b}[2J MESSAGE=\"x\r\ncloved: all is well\"",
            "\u{1b}[2Jnot a status\r\n",
        ] {
            let e: io::Error = expect_stream_ok(status, "the stream to a peer")
                .expect_err("refused")
                .into();
            let text = e.to_string();
            assert!(
                !text.contains('\u{1b}') && !text.contains('\r') && !text.contains('\n'),
                "{text:?}"
            );
        }
        // What the words are for still gets through.
        let e: io::Error = expect_stream_ok(
            "STREAM STATUS RESULT=I2P_ERROR MESSAGE=\"no tunnels\"",
            "the stream to a peer",
        )
        .expect_err("refused")
        .into();
        assert!(e.to_string().contains("I2P_ERROR"), "{e}");
        assert!(e.to_string().contains("no tunnels"), "{e}");
    }

    /// A bridge that speaks the outbound-dial half of SAM: `HELLO REPLY`,
    /// then the `STREAM STATUS` line it was told to give, then it echoes.
    ///
    /// Enough to test the whole dial path without a router, which is the
    /// point: this code is the reason clove no longer needs a live router to
    /// know whether a failed dial is reportable.
    /// [`dial_stream`] with its failure flattened back to an `io::Error`.
    ///
    /// The tests below care what an operator is told, which is the error; the
    /// one test that cares *which kind* of failure it was classifies it
    /// through `expect_stream_ok` directly.
    fn dialed(port: u16, peer: DestHash, timeout: Duration) -> io::Result<ForwardedStream> {
        dial_stream(port, "clove-test", peer, timeout).map_err(io::Error::from)
    }

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
        let mut stream = dialed(port, PEER, PROBE).expect("dial");
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
        let e = dialed(port, PEER, PROBE).expect_err("refused");
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
        assert!(e.to_string().contains("CANT_REACH_PEER"), "{e}");
        assert!(e.to_string().contains(&PEER.to_b32()), "{e}");

        // And the next dial on the same session id works, because the two
        // share no state at all.
        let ok = dial_bridge("STREAM STATUS RESULT=OK\n", true);
        assert!(dialed(ok, PEER, PROBE).is_ok());
    }

    /// A `MESSAGE=` is the router explaining itself; it belongs in the error.
    #[test]
    fn a_refusal_message_reaches_the_operator() {
        let port = dial_bridge(
            "STREAM STATUS RESULT=I2P_ERROR MESSAGE=\"session not found\"\n",
            false,
        );
        let e = dialed(port, PEER, PROBE).expect_err("refused");
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
            Misbehaviour::ForgeOk,
        ] {
            let port = fake_bridge(how);
            let (result, took) =
                within(LIMIT, move || dialed(port, PEER, PROBE)).expect("dial returned");
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
        let e = dialed(port, PEER, PROBE).expect_err("no newline");
        assert!(
            e.to_string().contains("STREAM CONNECT"),
            "the error must name the exchange it failed in: {e}"
        );
    }
}

#[cfg(test)]
mod naming_tests {
    //! `NAMING LOOKUP`: the name check that keeps somebody else's text out of
    //! the command, the reply parser on its own, and the whole exchange
    //! against a bridge that answers well and bridges that do not.
    //!
    //! The name reaching [`naming_lookup`] is a tracker host out of a
    //! `.torrent` a stranger wrote, so every case here is an input clove does
    //! not control.

    use super::hostile_bridge_tests::{Misbehaviour, fake_bridge};
    use super::*;
    use crate::addr::i2p_base64_encode;
    use std::sync::mpsc;

    /// Short: the misbehaving bridges below stall by design.
    const PROBE: Duration = Duration::from_millis(600);

    /// Long enough to clear `PROBE`, short enough that a hang fails the run.
    const LIMIT: Duration = Duration::from_secs(25);

    /// A destination-shaped blob and the hash it must resolve to.
    fn destination() -> (String, DestHash) {
        let mut bytes = vec![0x42u8; 384];
        bytes.extend_from_slice(&[0x05, 0x00, 0x04, 0x00, 0x07, 0x00, 0x00]);
        let b64 = i2p_base64_encode(&bytes);
        let hash = DestHash::from_b64_destination(&b64).expect("a destination");
        (b64, hash)
    }

    /// Read one `\n`-terminated line, as a bridge does.
    fn take_line(sock: &mut TcpStream) -> String {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while sock.read(&mut byte).unwrap_or(0) == 1 {
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        String::from_utf8_lossy(&line).into_owned()
    }

    /// A bridge that completes `HELLO`, reports the command it was sent, and
    /// answers with `reply`.
    fn naming_bridge(reply: &str) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let reply = reply.to_owned();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                let (reply, tx) = (reply.clone(), tx.clone());
                std::thread::spawn(move || {
                    let _ = take_line(&mut sock); // HELLO VERSION
                    let _ = sock.write_all(b"HELLO REPLY RESULT=OK VERSION=3.3\n");
                    let _ = tx.send(take_line(&mut sock)); // NAMING LOOKUP
                    let _ = sock.write_all(reply.as_bytes());
                    std::thread::sleep(Duration::from_secs(30));
                });
            }
        });
        (port, rx)
    }

    /// Run `f` on a thread and give it `limit`; `None` means it never returned.
    fn within<T: Send + 'static>(
        limit: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(limit).ok()
    }

    // ---- the name check -------------------------------------------------

    #[test]
    fn ordinary_names_are_accepted() {
        for name in [
            "tracker.postman.i2p",
            "opentracker.dg2.i2p",
            "ukeu3k5oycgaauneqgtnvselmt4yemvoilkln7jpvamvfx7dnkdq.b32.i2p",
            &"a".repeat(MAX_LOOKUP_NAME),
        ] {
            check_lookup_name(name).unwrap_or_else(|e| panic!("{name:?} refused: {e}"));
        }
    }

    #[test]
    fn a_name_that_would_forge_a_second_command_is_refused() {
        // The attack this exists for: SAM is a line protocol, so a newline in
        // a tracker host appends a command of the sender's choosing to ours.
        for name in [
            "tracker.i2p\nSESSION CREATE STYLE=STREAM ID=evil DESTINATION=TRANSIENT",
            "tracker.i2p\r\nDEST GENERATE",
            "tracker.i2p SILENT=true",
            "tracker.i2p\tX",
            "tracker.i2p\0",
            "tracker.i2p\x7f",
        ] {
            let e = check_lookup_name(name)
                .expect_err(&format!("{name:?} was accepted into a SAM command"));
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            // The rejection is readable and cannot itself forge a log line.
            assert!(
                !e.to_string().contains('\n'),
                "the error repeated the newline: {e}"
            );
        }
    }

    #[test]
    fn an_empty_or_overlong_name_is_refused() {
        assert_eq!(
            check_lookup_name("").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let long = "a".repeat(MAX_LOOKUP_NAME + 1);
        assert_eq!(
            check_lookup_name(&long).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_refused_name_never_reaches_a_socket() {
        // Nothing is listening on this port, so a connection error would prove
        // the name check ran too late. It must fail before the dial.
        let port = {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let e = naming_lookup(port, "tracker.i2p\nDEST GENERATE", PROBE)
            .expect_err("an injecting name resolved");
        assert_eq!(
            e.kind(),
            io::ErrorKind::InvalidInput,
            "the name was checked only after dialling: {e}"
        );
    }

    // ---- the reply parser ------------------------------------------------

    #[test]
    fn a_good_reply_yields_its_value() {
        let (b64, _) = destination();
        let reply = format!("NAMING REPLY RESULT=OK NAME=tracker.i2p VALUE={b64}");
        assert_eq!(parse_naming_reply(&reply, "tracker.i2p").unwrap(), b64);
    }

    #[test]
    fn field_order_does_not_matter() {
        let (b64, _) = destination();
        let reply = format!("NAMING REPLY VALUE={b64} RESULT=OK NAME=tracker.i2p");
        assert_eq!(parse_naming_reply(&reply, "tracker.i2p").unwrap(), b64);
    }

    #[test]
    fn a_refusal_carries_the_routers_own_word() {
        for result in ["KEY_NOT_FOUND", "INVALID_KEY", "I2P_ERROR"] {
            let reply = format!("NAMING REPLY RESULT={result} NAME=tracker.i2p");
            let e = parse_naming_reply(&reply, "tracker.i2p")
                .expect_err("a refusal resolved to something");
            assert_eq!(e.kind(), io::ErrorKind::NotFound);
            assert!(e.to_string().contains(result), "{result} not reported: {e}");
        }
    }

    #[test]
    fn replies_that_are_not_replies_are_refused() {
        for reply in [
            "",
            "HELLO REPLY RESULT=OK VERSION=3.3",
            "STREAM STATUS RESULT=OK",
            "NAMING",
            "naming reply RESULT=OK VALUE=x", // SAM is upper-case
            "\u{1b}[2JNAMING REPLY RESULT=OK VALUE=x",
        ] {
            let e = parse_naming_reply(reply, "tracker.i2p")
                .expect_err(&format!("{reply:?} was read as a NAMING REPLY"));
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn a_reply_with_no_usable_value_is_refused() {
        for reply in [
            "NAMING REPLY RESULT=OK NAME=tracker.i2p",
            "NAMING REPLY RESULT=OK NAME=tracker.i2p VALUE=",
        ] {
            let e = parse_naming_reply(reply, "tracker.i2p")
                .expect_err(&format!("{reply:?} yielded a destination"));
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        }
        // A missing RESULT is a refusal, not an accident to be read past.
        let e = parse_naming_reply("NAMING REPLY NAME=t.i2p VALUE=x", "t.i2p")
            .expect_err("a reply with no RESULT was accepted");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    /// The result word is the router's text as much as the rest of the line.
    #[test]
    fn a_refusal_word_cannot_forge_a_log_line() {
        let e = parse_naming_reply(
            "NAMING REPLY RESULT=KEY_NOT_FOUND\u{1b}[2J\r\ncloved: ok NAME=t.i2p",
            "t.i2p",
        )
        .expect_err("refused");
        let text = e.to_string();
        assert!(text.contains("KEY_NOT_FOUND"), "{text}");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\r') && !text.contains('\n'),
            "{text:?}"
        );
    }

    #[test]
    fn a_reply_cannot_forge_a_log_line_or_leak_a_blob() {
        // The reply is the bridge's text and reaches an operator's terminal.
        let e = parse_naming_reply("NAMING\u{1b}[2J REPLY RESULT=OK", "t.i2p").unwrap_err();
        assert!(!e.to_string().contains('\u{1b}'), "escape survived: {e}");
        // A key-shaped run in an unexpected place is redacted, not printed.
        let blob = "A".repeat(200);
        let e =
            parse_naming_reply(&format!("SESSION STATUS DESTINATION={blob}"), "t.i2p").unwrap_err();
        assert!(!e.to_string().contains(&blob), "a blob was printed: {e}");
    }

    // ---- the whole exchange ---------------------------------------------

    #[test]
    fn a_good_bridge_resolves_a_name() {
        let (b64, expected) = destination();
        let (port, sent) = naming_bridge(&format!(
            "NAMING REPLY RESULT=OK NAME=tracker.postman.i2p VALUE={b64}\n"
        ));
        let got = naming_lookup(port, "tracker.postman.i2p", PROBE).expect("resolved");
        assert_eq!(got, expected);
        // And the command was the one SAM specifies, on one line.
        assert_eq!(
            sent.recv_timeout(LIMIT).expect("the bridge saw a command"),
            "NAMING LOOKUP NAME=tracker.postman.i2p"
        );
    }

    #[test]
    fn a_value_that_is_not_a_destination_is_refused() {
        // A certificate claiming more payload than the blob carries: the
        // length field is the router's word for how long this is, and taking
        // it on trust is how a short read becomes a long one.
        let mut truncated = vec![0x11u8; 384];
        truncated.extend_from_slice(&[0x05, 0xFF, 0xFF]); // 65535 bytes promised, none sent
        let truncated = i2p_base64_encode(&truncated);

        for value in [
            "not-a-destination",
            "AAAA",
            // 384 bytes: the key fields with no room for a certificate header.
            &"A".repeat(512),
            &truncated,
        ] {
            let (port, _sent) = naming_bridge(&format!("NAMING REPLY RESULT=OK VALUE={value}\n"));
            let e = naming_lookup(port, "tracker.i2p", PROBE)
                .expect_err(&format!("{value:?} was accepted as a destination"));
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn a_reply_with_no_newline_is_capped_rather_than_buffered() {
        // No terminator, more than the cap: the byte bound is what ends this,
        // since a timeout alone would let a bridge stream for its whole budget.
        let (port, _sent) = naming_bridge(&"A".repeat(MAX_NAMING_LINE + 1024));
        let e = within(LIMIT, move || naming_lookup(port, "tracker.i2p", PROBE))
            .expect("the lookup returned")
            .expect_err("an unterminated reply resolved");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn every_broken_bridge_fails_in_bounded_time() {
        for how in [
            Misbehaviour::CloseImmediately,
            Misbehaviour::Silence,
            Misbehaviour::Garbage,
            Misbehaviour::Flood,
            Misbehaviour::RefuseHello,
            Misbehaviour::Dribble,
            Misbehaviour::HelloThenStall,
            Misbehaviour::ForgeOk,
        ] {
            let port = fake_bridge(how);
            let Some(result) = within(LIMIT, move || naming_lookup(port, "tracker.i2p", PROBE))
            else {
                panic!("{how:?}: the lookup never returned");
            };
            let e = result.expect_err(&format!("{how:?} resolved a name"));
            assert_ne!(
                e.kind(),
                io::ErrorKind::InvalidInput,
                "{how:?}: blamed the name for a bridge fault: {e}"
            );
        }
    }

    #[test]
    fn nothing_listening_is_a_fast_clean_error() {
        let port = {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let start = Instant::now();
        naming_lookup(port, "tracker.i2p", PROBE).expect_err("resolved with no router");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(test)]
mod session_tests {
    //! The control connection: `SESSION CREATE`, the watchdog, and what the
    //! session says when it ends.
    //!
    //! A session that ends without a reported reason is one nobody can
    //! diagnose, so what the router says on the way out is asserted here
    //! (`PROTOCOL.i2p-bt` §2.13).

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    /// A session's private key blob, shaped like the one i2pd returns: 391
    /// bytes of destination (384 of key material, a KEY certificate with a
    /// 4-byte payload) followed by 288 bytes of private key material that must
    /// never leave the process (§5.1c).
    fn key_blob() -> Vec<u8> {
        let mut blob = vec![0x42u8; 384];
        blob.push(0x05);
        blob.extend_from_slice(&4u16.to_be_bytes());
        blob.extend_from_slice(&[0x00, 0x07, 0x00, 0x00]);
        blob.extend(std::iter::repeat_n(0xAAu8, 288));
        blob
    }

    /// The public destination inside [`key_blob`] — the only part of it that
    /// may ever be published.
    fn destination() -> Vec<u8> {
        key_blob()[..391].to_vec()
    }

    fn read_line_from(socket: &mut TcpStream) -> String {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while socket.read(&mut byte).unwrap_or(0) == 1 {
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        String::from_utf8_lossy(&line).trim_end().to_owned()
    }

    /// What the fake router does once the session is established.
    #[derive(Clone, Copy, Debug)]
    enum AfterSession {
        /// Nothing at all — an ordinary, healthy session.
        Hold,
        /// Ping the client, the way Java I2P's `SAMv3Handler` does, and report
        /// whatever came back.
        Ping,
        /// Explain itself and hang up, the way a router ending a session does.
        ExplainAndClose,
        /// Explain itself and **hold the connection open** — a router that
        /// destroys the session without hanging up, which is the case that
        /// wedged clove: nothing closes, so nothing detected it.
        ExplainAndHold,
    }

    /// A fake SAM bridge that can carry a whole session.
    ///
    /// The first connection is the control connection; every later one is
    /// answered as a `STREAM FORWARD`. Everything the client says is echoed
    /// down the returned channel, so a test can assert on the wire.
    fn session_bridge(status: &'static str, after: AfterSession) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let seen = AtomicUsize::new(0);
            while let Ok((mut socket, _)) = listener.accept() {
                let first = seen.fetch_add(1, Ordering::Relaxed) == 0;
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let _ = read_line_from(&mut socket); // HELLO VERSION
                    let _ = socket.write_all(b"HELLO REPLY RESULT=OK VERSION=3.3\n");
                    let command = read_line_from(&mut socket);
                    let _ = tx.send(command);
                    if !first {
                        // A STREAM FORWARD, which this bridge always allows.
                        let _ = socket.write_all(b"STREAM STATUS RESULT=OK\n");
                        std::thread::sleep(Duration::from_secs(120));
                        return;
                    }
                    let _ = socket.write_all(status.as_bytes());
                    match after {
                        AfterSession::Hold => std::thread::sleep(Duration::from_secs(120)),
                        AfterSession::Ping => {
                            let _ = socket.write_all(b"PING 12345\n");
                            let _ = tx.send(read_line_from(&mut socket));
                            std::thread::sleep(Duration::from_secs(120));
                        }
                        AfterSession::ExplainAndClose => {
                            let _ = socket.write_all(
                                b"SESSION STATUS RESULT=I2P_ERROR MESSAGE=\"tunnel build failed\"\n",
                            );
                            // and drop, which closes it
                        }
                        AfterSession::ExplainAndHold => {
                            let _ = socket.write_all(
                                b"SESSION STATUS RESULT=I2P_ERROR MESSAGE=\"tunnel build failed\"\n",
                            );
                            std::thread::sleep(Duration::from_secs(120));
                        }
                    }
                });
            }
        });
        (port, rx)
    }

    /// A `SESSION STATUS` carrying [`key_blob`], as the router sends it.
    fn ok_status() -> &'static str {
        // Leaked once per process, and only in tests: `session_bridge` wants a
        // 'static line and the blob is computed.
        Box::leak(
            format!(
                "SESSION STATUS RESULT=OK DESTINATION={}\n",
                crate::addr::i2p_base64_encode(&key_blob())
            )
            .into_boxed_str(),
        )
    }

    fn config(port: u16) -> SamConfig {
        SamConfig {
            samv3_tcp_port: port,
            nickname: "clove-test".to_owned(),
            probe_timeout: Duration::from_secs(5),
            session_timeout: Duration::from_secs(5),
            persistent_key: None,
        }
    }

    /// The config holds the whole `DESTINATION=` blob, and a config is exactly
    /// what an error path prints with `{:?}`.
    #[test]
    fn a_config_debug_print_redacts_the_private_key() {
        let key = crate::addr::i2p_base64_encode(&key_blob());
        let config = SamConfig {
            persistent_key: Some(key.clone()),
            ..config(7656)
        };
        for printed in [format!("{config:?}"), format!("{config:#?}")] {
            assert!(!printed.contains(&key), "the key was printed: {printed}");
            // Not even a recognisable piece of it.
            assert!(!printed.contains(&key[..32]), "{printed}");
            assert!(printed.contains("<redacted>"), "{printed}");
            // The rest of the config is still worth printing.
            assert!(printed.contains("clove-test"), "{printed}");
            assert!(printed.contains("7656"), "{printed}");
        }
        // Says a key is set, without saying what it is.
        assert!(
            format!("{config:?}").contains("persistent_key: Some(<redacted>)"),
            "{config:?}"
        );
        let transient = format!(
            "{:?}",
            SamConfig {
                nickname: "clove-test".to_owned(),
                ..SamConfig::default()
            }
        );
        assert!(transient.contains("persistent_key: None"), "{transient}");
    }

    #[test]
    fn a_session_publishes_its_destination_and_never_its_private_keys() {
        let (port, sent) = session_bridge(ok_status(), AfterSession::Hold);
        let session = SamSession::connect(&config(port)).expect("session");

        // The command actually put on the wire.
        let create = sent.recv_timeout(Duration::from_secs(5)).expect("create");
        assert!(
            create.starts_with("SESSION CREATE STYLE=STREAM"),
            "{create}"
        );
        assert!(create.contains("ID=clove-test"), "{create}");
        assert!(create.contains("DESTINATION=TRANSIENT"), "{create}");

        // §5.1c, the defect that sent our private keys to a tracker: what we
        // publish is the destination at the front of the blob, and the
        // identity is its hash — not the blob's.
        let published =
            crate::addr::i2p_base64_decode(session.local_dest_b64()).expect("published base64");
        assert_eq!(published, destination(), "the private half must be cut off");
        assert!(
            published.len() < key_blob().len(),
            "the whole blob was published"
        );
        assert_eq!(
            session.local_dest(),
            DestHash::from_b64_destination(&crate::addr::i2p_base64_encode(&destination()))
                .expect("hash")
        );
        assert!(session.healthy());
        assert!(I2pDialer::usable(&session), "a live session is usable");
    }

    /// The keep-alive clove never answered. Java I2P pings the client on every
    /// read timeout and kills the session with `SESSION_ERROR "PONG timeout"`
    /// when no `PONG` comes back — and nothing in clove read the control
    /// connection at all, so nothing ever could.
    #[test]
    fn a_router_initiated_ping_is_answered_with_a_pong() {
        let (port, sent) = session_bridge(ok_status(), AfterSession::Ping);
        let session = SamSession::connect(&config(port)).expect("session");
        let _create = sent.recv_timeout(Duration::from_secs(5)).expect("create");

        let reply = sent
            .recv_timeout(Duration::from_secs(5))
            .expect("a reply to the PING");
        assert_eq!(reply, "PONG 12345", "the token must be echoed verbatim");
        assert!(
            session.healthy(),
            "answering a ping is not losing a session"
        );
    }

    /// The whole point of the watchdog: when a session ends, say so at once
    /// and say what the router said. The old probe took up to 90 seconds to
    /// notice and reported the word "lost".
    #[test]
    fn a_lost_session_is_noticed_at_once_and_carries_the_routers_last_words() {
        let (port, _sent) = session_bridge(ok_status(), AfterSession::ExplainAndClose);
        let session = SamSession::connect(&config(port)).expect("session");

        let started = Instant::now();
        let reason = session.wait_until_lost();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?} to notice a closed control connection",
            started.elapsed()
        );
        assert!(!session.healthy());
        assert!(
            !I2pDialer::usable(&session),
            "a dead session must not be handed best-effort work"
        );
        assert!(
            reason.contains("tunnel build failed"),
            "the router explained itself and we dropped it: {reason}"
        );
    }

    /// End-of-file is not health: a reader that maps it to an empty line reads
    /// a destroyed session as a healthy one.
    #[test]
    fn a_control_connection_at_end_of_file_is_dead_not_healthy() {
        let (port, _sent) = session_bridge(ok_status(), AfterSession::ExplainAndClose);
        let session = SamSession::connect(&config(port)).expect("session");
        let _ = session.wait_until_lost();
        assert!(
            !session.healthy(),
            "a closed control connection reported itself healthy"
        );
    }

    #[test]
    fn a_refused_session_reports_the_routers_own_word() {
        // DuplicateId is the §2.9 hazard, and it calls for a different action
        // than any other refusal — so it has to survive into the error.
        let (port, _sent) = session_bridge(
            "SESSION STATUS RESULT=DUPLICATED_ID MESSAGE=\"session clove-test exists\"\n",
            AfterSession::Hold,
        );
        let err = SamSession::connect(&config(port))
            .map(drop)
            .expect_err("a refused session is not a session");
        assert!(err.to_string().contains("DUPLICATED_ID"), "{err}");
        assert!(
            err.to_string().contains("session clove-test exists"),
            "{err}"
        );
    }

    /// A router's control line may carry the private half of our identity, and
    /// what we do with a line we did not expect is print it.
    ///
    /// `SESSION STATUS RESULT=OK DESTINATION=…` is the ordinary shape of that:
    /// the blob is the key. A bridge that answers oddly — malformed, or a
    /// refusal with the key after `MESSAGE=` — used to have that line embedded
    /// verbatim in an error, which the supervisor then wrote to stderr.
    /// `SECURITY.md` puts any part of the `DESTINATION=` blob reaching a log in
    /// scope, and a bug report is precisely where such a line goes.
    #[test]
    fn a_control_line_cannot_carry_key_material_into_a_log() {
        let key = "A".repeat(516);

        // Named field, whatever else is on the line.
        let scrubbed = scrub_control_line(&format!(
            "SESSION STATUS RESULT=OK DESTINATION={key} STYLE=STREAM"
        ));
        assert!(!scrubbed.contains(&key), "{scrubbed}");
        assert!(scrubbed.contains("RESULT=OK"), "{scrubbed}");
        assert!(scrubbed.contains("DESTINATION=<redacted>"), "{scrubbed}");
        assert!(scrubbed.contains("STYLE=STREAM"), "the shape survives");

        // Unnamed, or hidden behind a field we have never heard of: the run of
        // base64 gives it away on its own.
        for line in [
            format!("SESSION STATUS RESULT=OK {key}"),
            format!("SESSION STATUS RESULT=I2P_ERROR MESSAGE=\"oops {key}\""),
            format!("SESSION STATUS RESULT=OK SOMETHINGNEW={key}"),
        ] {
            let scrubbed = scrub_control_line(&line);
            assert!(!scrubbed.contains(&key), "key survived in: {scrubbed}");
        }

        // What the field is *for* survives, or keeping the line is pointless.
        let diagnostic =
            scrub_control_line("SESSION STATUS RESULT=I2P_ERROR MESSAGE=\"tunnel build failed\"");
        assert!(diagnostic.contains("I2P_ERROR"), "{diagnostic}");
        assert!(diagnostic.contains("tunnel build failed"), "{diagnostic}");

        // And a router cannot forge a log line or repaint the terminal.
        let nasty = scrub_control_line("SESSION STATUS RESULT=OK\r\ncloved: all is well\x1b[2J");
        assert!(
            !nasty.contains('\n') && !nasty.contains('\r') && !nasty.contains('\x1b'),
            "{nasty:?}"
        );
    }

    #[test]
    fn a_session_status_without_a_destination_is_refused() {
        let (port, _sent) = session_bridge("SESSION STATUS RESULT=OK\n", AfterSession::Hold);
        let err = SamSession::connect(&config(port))
            .map(drop)
            .expect_err("no DESTINATION means no identity");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn forwarding_asks_for_the_destination_header_and_keeps_its_socket() {
        let (port, sent) = session_bridge(ok_status(), AfterSession::Hold);
        let session = Arc::new(SamSession::connect(&config(port)).expect("session"));
        let _create = sent.recv_timeout(Duration::from_secs(5)).expect("create");

        let listener = SamListener::forward(Arc::clone(&session)).expect("forward");
        let forward = sent
            .recv_timeout(Duration::from_secs(5))
            .expect("forward command");
        assert!(forward.starts_with("STREAM FORWARD "), "{forward}");
        assert!(forward.contains("ID=clove-test"), "{forward}");
        assert!(
            forward.contains(&format!("PORT={}", listener.local_port())),
            "the router must be pointed at the listener we bound: {forward}"
        );
        // SILENT=false is what makes the peer's destination line arrive, and
        // without it an inbound peer has no identity at all (§2.5).
        assert!(forward.contains("SILENT=false"), "{forward}");
        assert_eq!(listener.local_dest(), session.local_dest());
    }

    #[test]
    fn ping_lines_are_recognised_exactly() {
        assert_eq!(ping_token("PING"), Some(""));
        assert_eq!(ping_token("PING 12345"), Some(" 12345"));
        assert_eq!(ping_token("PING  two  spaces"), Some("  two  spaces"));
        // Not pings, and answering them would be answering something we did
        // not understand.
        assert_eq!(ping_token("PINGER 1"), None);
        assert_eq!(ping_token("PONG 12345"), None);
        assert_eq!(ping_token("SESSION STATUS RESULT=OK"), None);
        assert_eq!(ping_token(""), None);
    }

    #[test]
    fn session_status_lines_are_recognised_exactly() {
        assert_eq!(
            session_status_result("SESSION STATUS RESULT=I2P_ERROR"),
            Some("I2P_ERROR")
        );
        assert_eq!(
            session_status_result("SESSION STATUS RESULT=OK"),
            Some("OK")
        );
        // Malformed rather than fatal: reported, but not acted on. A session
        // that is really gone proves it on the next dial.
        assert_eq!(session_status_result("SESSION STATUS"), Some("MISSING"));
        // Not a SESSION STATUS at all, and acting on one would be acting on
        // something we did not understand.
        assert_eq!(session_status_result("SESSION STATUSES RESULT=OK"), None);
        assert_eq!(session_status_result("STREAM STATUS RESULT=OK"), None);
        assert_eq!(session_status_result("PING"), None);
        assert_eq!(session_status_result(""), None);
    }

    /// The wedge this fix exists for.
    ///
    /// A router may destroy a session and say so on the control connection
    /// **without closing it**. The previous version of this code read that
    /// line, filed it away as a souvenir for a future log message, and went
    /// on believing the session was alive — so `wait_until_lost` never
    /// returned, the daemon reported `connected` indefinitely, and every dial
    /// and announce failed with `INVALID_ID` until somebody restarted it.
    #[test]
    fn a_session_the_router_ended_is_lost_even_if_nothing_hangs_up() {
        let (port, _sent) = session_bridge(ok_status(), AfterSession::ExplainAndHold);
        let session = Arc::new(SamSession::connect(&config(port)).expect("session"));

        // Bounded, because the regression is a *hang*: before this fix
        // `wait_until_lost` simply never returned.
        let (tx, rx) = mpsc::channel();
        let waiting = Arc::clone(&session);
        std::thread::spawn(move || {
            let _ = tx.send(waiting.wait_until_lost());
        });
        let lost = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a session the router ended must be reported lost, not held open");

        assert!(lost.contains("the router ended the session"), "{lost}");
        // And it must carry what the router actually said, which is the whole
        // reason the watchdog reads this connection at all.
        assert!(lost.contains("I2P_ERROR"), "{lost}");
        assert!(lost.contains("tunnel build failed"), "{lost}");
        assert!(!session.healthy(), "and nothing may be handed to it after");
    }

    /// The backstop, for a router that destroys a session and says nothing.
    ///
    /// Then a refused dial is the only evidence there is, and treating
    /// `INVALID_ID` as "this peer, this time" means retrying into it forever.
    #[test]
    fn a_dial_refused_with_invalid_id_condemns_the_session() {
        let refused = expect_stream_ok("STREAM STATUS RESULT=INVALID_ID", "the stream to a peer")
            .expect_err("INVALID_ID is a refusal");
        assert!(
            matches!(refused, DialFailed::SessionGone(_)),
            "INVALID_ID is the session's death, not a peer's"
        );
        // Every other refusal stays the peer's problem, or one bad peer would
        // tear down a healthy session — the opposite failure, and worse.
        for ordinary in ["CANT_REACH_PEER", "I2P_ERROR", "TIMEOUT", "MISSING"] {
            let status = format!("STREAM STATUS RESULT={ordinary}");
            let e = expect_stream_ok(&status, "the stream to a peer").expect_err("refusal");
            assert!(
                matches!(e, DialFailed::Peer(_)),
                "{ordinary} must not end the session"
            );
        }

        // And a session told this is no longer offered work.
        let (port, _sent) = session_bridge(ok_status(), AfterSession::Hold);
        let session = SamSession::connect(&config(port)).expect("session");
        assert!(session.healthy(), "a fresh session is alive");
        session
            .life
            .end("the router no longer has our session (test)");
        assert!(!session.healthy(), "a dial may condemn its own session");
        assert!(!session.usable(), "and nothing may be handed to it after");
    }
}
