//! `SAMv3` backend over `yosemite` (Phase D, `docs/PLAN.md`).
//!
//! This wraps `yosemite`'s synchronous API behind the crate's traits. It is
//! the *only* code here that talks to a real router, and it is the one part
//! of clove that cannot be verified without one — so its scope is drawn at
//! what is structurally sound to write against the API as reviewed
//! (`docs/PROTOCOL.i2p-bt`):
//!
//! - [`SamSession`] implements [`I2pDialer`] (SAM `STREAM CONNECT`, via the
//!   peer's `.b32.i2p` address) and [`I2pNamingLookup`] (SAM `NAMING
//!   LOOKUP`), and wraps yosemite [`yosemite::Stream`] as [`SamStream`],
//!   whose [`I2pStream::split`] maps straight onto yosemite's own
//!   `Stream::split`.
//! - **Inbound accept is deliberately not implemented here yet.** yosemite's
//!   `Session::accept`/`forward` take `&mut self` and block, so the
//!   inbound-stream topology (SAM `FORWARD` to a loopback listener, deriving
//!   each peer's dest-hash from the forwarded handshake) and the
//!   concurrency question R2 raises both need a live router to settle. That
//!   is M1 / R2-harness work; see `docs/PROTOCOL.i2p-bt`.
//!
//! yosemite hardcodes the SAM host to `127.0.0.1` (only the port is
//! configurable), which happens to match Layer 1's loopback-only rule; the
//! `--i-know-sam-is-remote` escape hatch is therefore not expressible
//! through this backend and is noted as such.

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::time::Duration;

use yosemite::{DestinationKind, Session, SessionOptions, style};

use crate::{DestHash, I2pDialer, I2pNamingLookup, I2pStream};

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
            ..Default::default()
        };
        let session = Session::<style::Stream>::new(options).map_err(map_err)?;
        let local = DestHash::from_b64_destination(session.destination()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SAM returned an unparseable destination",
            )
        })?;
        Ok(SamSession {
            session: Mutex::new(session),
            local,
            samv3_tcp_port: config.samv3_tcp_port,
        })
    }

    /// This session's own destination hash — the identity peers reach us at
    /// and the one announced to trackers.
    #[must_use]
    pub fn local_dest(&self) -> DestHash {
        self.local
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
