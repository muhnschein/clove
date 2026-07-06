//! The clove engine: everything between the `i2pnet` boundary below and the
//! daemon/API above. This crate never touches a socket — peer addressing is
//! [`i2pnet::DestHash`] only, enforced by the workspace `clippy.toml`.
//!
//! Module plan (built in phase order, `docs/PLAN.md`):
//!
//! - `bencode` — hand-rolled codec (~200 lines), hostile-input hardened,
//!   fuzz target from day one. Also the resume-format encoding (Q2).
//! - `metainfo` — .torrent parsing, I2P-only announce-URL filtering (BEP 12).
//! - `config` — flat `key value` parser; unknown keys are fatal; `-C` check.
//! - `resume` — versioned per-torrent state (`docs/STATE-FORMAT.md` at M4).
//! - `wire` — BEP 3 message codec + BEP 6 fast extension + BEP 10 handshake.
//! - `peer` — per-peer connection state machine (explicit enum, exhaustive
//!   transition table in docs).
//! - `picker` — rarest-first with endgame; sequential per-torrent flag.
//! - `storage` — file-backed piece store, preallocation option, SHA-1
//!   verification; dedicated worker threads, bounded queues only.
//! - `choker` — choke/interest state machines, I2P-latency-tuned timeouts.
//! - `tracker` — HTTP announces over I2P streams; compact = concatenated
//!   32-byte destination hashes.
//! - `pex` — `i2p_pex`; i2psnark behavior is normative (R4).
//! - `magnet` — BEP 9 metadata exchange.
//! - `supervisor` — torrent lifecycle + "waiting for router" state.

// Intentionally empty at bootstrap: modules land per docs/PLAN.md phases.
// The unused-dep on i2pnet pins the dependency direction (engine -> i2pnet).
