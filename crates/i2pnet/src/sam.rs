//! `SAMv3` backend over `yosemite` (Phase D, `docs/PLAN.md`).
//!
//! This wraps `yosemite`'s synchronous API behind the crate's traits. It is
//! the *only* code here that talks to a real router. Its runtime behavior
//! against a live router is verified out-of-CI (`docs/LIVE-TESTING.md`); the
//! logic that does not need a router — address derivation and the forwarded
//! destination-line parse — is unit-tested here over loopback TCP.
//!
//! - [`SamSession`] implements [`I2pDialer`] (SAM `STREAM CONNECT`, via the
//!   peer's `.b32.i2p` address) and [`I2pNamingLookup`] (SAM `NAMING
//!   LOOKUP`), and wraps yosemite [`yosemite::Stream`] as [`SamStream`],
//!   whose [`I2pStream::split`] maps straight onto yosemite's own
//!   `Stream::split`.
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
}

impl Default for SamConfig {
    fn default() -> Self {
        SamConfig {
            samv3_tcp_port: DEFAULT_SAM_PORT,
            nickname: "clove".to_owned(),
            persistent_key: None,
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
}

impl SamSession {
    /// Establish the session against the router named by `config`.
    ///
    /// # Errors
    ///
    /// The router is unreachable, refused the session, or returned a
    /// destination we cannot parse.
    pub fn connect(config: &SamConfig) -> io::Result<SamSession> {
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
        let local = DestHash::from_b64_destination(session.destination()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SAM returned an unparseable destination",
            )
        })?;
        let local_b64 = session.destination().to_owned();
        Ok(SamSession {
            session: Mutex::new(session),
            local,
            local_b64,
            samv3_tcp_port: config.samv3_tcp_port,
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

    fn accept(&self) -> io::Result<(ForwardedStream, DestHash)> {
        let (mut stream, _addr) = self.listener.accept()?;
        // Bound the header read so a silent/misbehaving router cannot wedge
        // the acceptor; then hand a blocking socket to the reader thread.
        stream.set_read_timeout(Some(DEST_LINE_TIMEOUT))?;
        let dest = read_dest_line(&mut stream, MAX_DEST_LINE)?;
        stream.set_read_timeout(None)?;
        Ok((ForwardedStream { inner: stream }, dest))
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
pub struct ForwardedStream {
    inner: TcpStream,
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
}

impl I2pDialer for SamSession {
    type Stream = SamStream;

    fn dial(&self, peer: DestHash, _timeout: Duration) -> io::Result<SamStream> {
        // yosemite's synchronous `connect` has no timeout parameter; the
        // caller's timeout is honored at the supervision layer, not here.
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stream = session.connect(&peer.to_b32()).map_err(map_err)?;
        Ok(SamStream { inner: stream })
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

/// A SAM virtual stream. Duplex for the handshake, then [`split`] into
/// yosemite's own read/write halves for the peer's reader/writer threads.
///
/// [`split`]: I2pStream::split
pub struct SamStream {
    inner: yosemite::Stream,
}

impl Read for SamStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for SamStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl I2pStream for SamStream {
    type Reader = yosemite::ReadHalf;
    type Writer = yosemite::WriteHalf;

    fn split(self) -> io::Result<(yosemite::ReadHalf, yosemite::WriteHalf)> {
        self.inner
            .split()
            .ok_or_else(|| io::Error::other("SAM stream could not be split"))
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
    //! Router-free coverage: the SAM session itself needs a live router
    //! (exercised via `docs/LIVE-TESTING.md`), but the inbound path's two
    //! bits of pure logic — splitting the forwarded socket and parsing the
    //! `SILENT=false` destination header — are tested here over loopback TCP
    //! and in-memory readers.

    use super::*;
    use crate::addr::i2p_base64_encode;
    use std::io::Cursor;

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
        let dest_bytes = [0x42u8; 48];
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
