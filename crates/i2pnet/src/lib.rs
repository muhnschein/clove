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

/// A peer address: the 32-byte SHA-256 hash of an I2P destination.
///
/// This is the only peer-identity type in clove. It is what appears in
/// compact tracker responses and `i2p_pex` messages, and what `dial` takes
/// (the router resolves it via its b32 form).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DestHash(pub [u8; 32]);

/// Outbound I2P stream connections (SAM `STREAM CONNECT`).
pub trait I2pDialer {
    /// The connected-stream type. `Read + Write` blocking I/O per the Q5
    /// decision (sync thread-per-peer, `docs/DECISIONS.md`).
    type Stream: Read + Write + Send;

    /// Connect to a peer by destination hash, failing after `timeout`.
    ///
    /// # Errors
    /// Session down, lease-set lookup failure, or timeout. Errors carry
    /// operator-readable text (a log line at 2 a.m. must make sense).
    fn dial(&self, peer: DestHash, timeout: Duration) -> io::Result<Self::Stream>;
}

/// Inbound I2P stream connections (SAM `STREAM ACCEPT`/`FORWARD`).
pub trait I2pListener {
    /// The accepted-stream type; see [`I2pDialer::Stream`].
    type Stream: Read + Write + Send;

    /// Block until a peer connects; returns the stream and who dialed us.
    ///
    /// # Errors
    /// Session loss. The supervisor (Phase D) owns re-establishment;
    /// callers treat an error as "listener gone, wait for resurrection".
    fn accept(&self) -> io::Result<(Self::Stream, DestHash)>;
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

/// In-memory implementation of the `i2pnet` traits for engine tests.
///
/// Implemented in Phase B (`docs/PLAN.md`): a process-local "network" where
/// mock destinations connect to each other over piped streams, with fault
/// injection (session drop, stalled streams, lookup failure) for chaos tests.
pub mod mock {}
