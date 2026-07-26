//! I2P network access layer — the ONLY crate in this workspace permitted to
//! open sockets (Layer 1 no-clearnet enforcement, `docs/SCOPE.md` §5).
//!
//! Everything network-shaped in clove goes through the traits defined here.
//! The production implementation (Phase D, `docs/PLAN.md`) wraps the
//! `yosemite` `SAMv3` library with its `sync` feature; [`mock`] provides an
//! in-memory implementation so the engine is testable without a router.
//!
//! Invariants this crate owns:
//!
//! - The one permitted IP socket is a TCP connection to the configured SAM
//!   bridge, which must be loopback (or a unix socket) unless the operator
//!   sets the explicit, documented-as-dangerous remote-SAM override.
//! - The local HTTP API's opt-in localhost-TCP listener is also created
//!   *here* (a planned `bind_local_api()` that refuses non-loopback
//!   addresses), so every IP-socket construction site in the codebase lives
//!   in this crate. `cloved` gets a listener handle, never a socket API.
//! - No DNS: names are I2P names, resolved via SAM `NAMING LOOKUP` only.
//!
//! Peer addressing uses [`DestHash`] exclusively — there is no `IpAddr`
//! anywhere in the engine's type vocabulary.

// Layer-1 exception (SCOPE §5): this crate implements the boundary the
// workspace clippy.toml enforces on everyone else.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::io::{self, Read, Write};
use std::time::Duration;

pub mod addr;
pub mod api;
pub mod mock;
pub mod naming;
pub mod sam;
pub mod supervisor;

/// A peer address: the 32-byte SHA-256 hash of an I2P destination.
///
/// This is the only peer-identity type in clove. It is what appears in
/// compact tracker responses and `i2p_pex` messages, and what `dial` takes
/// (the router resolves it via its b32 form).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DestHash(pub [u8; 32]);

/// A bidirectional I2P stream. Blocking `Read`/`Write` per the Q5 decision
/// (sync thread-per-peer, `docs/DECISIONS.md`).
///
/// A connection is used duplex for the handshake, then [`split`](Self::split)
/// into independent read and write halves so a peer's reader and writer run
/// on separate threads. This mirrors `yosemite`'s `Stream::split` (the real
/// backend cannot hand out two duplex clones — see `docs/PROTOCOL.i2p-bt`),
/// so the abstraction stays honest across the mock and SAM implementations.
pub trait I2pStream: Read + Write + Send + Sized {
    /// The read half yielded by [`split`](Self::split).
    type Reader: Read + Send + 'static;
    /// The write half yielded by [`split`](Self::split).
    type Writer: Write + Send + 'static;

    /// Consume the stream, splitting it into independent read and write
    /// halves. Both must reference the same underlying connection; closing
    /// or dropping one ends the stream.
    ///
    /// # Errors
    /// The underlying implementation cannot produce independent halves.
    fn split(self) -> io::Result<(Self::Reader, Self::Writer)>;

    /// Bound how long reads and writes on this stream may block, where the
    /// backend can do it. `None` restores blocking behaviour.
    ///
    /// Worth setting around anything that waits on a peer's *first* bytes: a
    /// connection that arrives and then says nothing otherwise parks a thread
    /// for the life of the process. The default does nothing, for backends with
    /// no timeout of their own to set (a SAM virtual stream has none), so a
    /// caller must treat this as best-effort rather than a guarantee.
    ///
    /// # Errors
    /// The underlying socket refused the option.
    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        let _ = timeout;
        Ok(())
    }
}

/// Outbound I2P stream connections (SAM `STREAM CONNECT`).
pub trait I2pDialer {
    /// The connected-stream type.
    type Stream: I2pStream;

    /// Connect to a peer by destination hash, failing after `timeout`.
    ///
    /// # Errors
    /// Session down, lease-set lookup failure, refusal, or timeout. Errors
    /// carry operator-readable text (a log line at 2 a.m. must make sense).
    fn dial(&self, peer: DestHash, timeout: Duration) -> io::Result<Self::Stream>;
}

/// Inbound I2P stream connections (SAM `STREAM ACCEPT`/`FORWARD`), bound to
/// one local destination — which is why the local identity lives here.
pub trait I2pListener {
    /// The accepted-stream type.
    type Stream: I2pStream;

    /// The destination hash peers reach this session at: the identity used
    /// in handshakes and tracker announces.
    fn local_dest(&self) -> DestHash;

    /// Block until a peer connects.
    ///
    /// `Ok(Some(..))` is a usable connection and who dialed us. `Ok(None)` means
    /// *that one connection* was not usable — a forwarded stream whose
    /// destination header never arrived, a local process poking the forward
    /// port — and the caller should go straight back to accepting. The two are
    /// separate outcomes on purpose: an accept loop that treats a single bad
    /// connection as the end of the listener stops serving inbound peers
    /// entirely until the session is rebuilt, which is a denial of service
    /// anything on the loopback port could trigger.
    ///
    /// # Errors
    /// Session loss — the listener itself is finished. The supervisor (Phase D)
    /// owns re-establishment; callers treat an error as "listener gone, wait for
    /// resurrection".
    fn accept(&self) -> io::Result<Option<(Self::Stream, DestHash)>>;
}

/// Sharing a dialer across threads: an `Arc`'d dialer dials. This is how the
/// one SAM session (or a mock) is handed to every torrent's swarm runner.
impl<T: I2pDialer> I2pDialer for std::sync::Arc<T> {
    type Stream = T::Stream;

    fn dial(&self, peer: DestHash, timeout: Duration) -> io::Result<Self::Stream> {
        T::dial(self, peer, timeout)
    }
}

/// I2P naming resolution (SAM `NAMING LOOKUP`), e.g. `tracker2.postman.i2p`.
///
/// Implementations cache positive results aggressively and apply
/// negative-result backoff (R6) — callers just call `lookup`.
pub trait I2pNamingLookup {
    /// Resolve an I2P hostname or b32 name to a destination hash.
    ///
    /// # Errors
    /// Unknown name, lookup timeout, or session loss. Non-I2P hostnames are
    /// rejected here, never resolved via DNS.
    fn lookup(&self, name: &str) -> io::Result<DestHash>;
}

/// As with dialing, an `Arc`'d resolver resolves.
impl<T: I2pNamingLookup> I2pNamingLookup for std::sync::Arc<T> {
    fn lookup(&self, name: &str) -> io::Result<DestHash> {
        T::lookup(self, name)
    }
}
