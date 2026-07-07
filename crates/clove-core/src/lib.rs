//! The clove engine: everything between the `i2pnet` boundary below and the
//! daemon/API above. This crate never touches a socket — peer addressing is
//! [`i2pnet::DestHash`] only, enforced by the workspace `clippy.toml`.
//!
//! Modules land in phase order (`docs/PLAN.md`). Present (Phase A):
//! [`bencode`], [`metainfo`], [`config`], [`resume`]. Still to come:
//!
//! - `wire` — BEP 3 message codec + BEP 6 fast extension + BEP 10 handshake.
//! - `peer` — per-peer connection state machine (explicit enum, exhaustive
//!   transition table in docs).
//! - `picker` — rarest-first with endgame; sequential per-torrent flag.
//! - `storage` — file-backed piece store, preallocation option, SHA-1
//!   verification; dedicated worker threads, bounded queues only; atomic
//!   resume writes.
//! - `choker` — choke/interest state machines, I2P-latency-tuned timeouts.
//! - `tracker` — HTTP announces over I2P streams; compact = concatenated
//!   32-byte destination hashes.
//! - `pex` — `i2p_pex`; i2psnark behavior is normative (R4).
//! - `magnet` — BEP 9 metadata exchange.
//! - `supervisor` — torrent lifecycle + "waiting for router" state.

pub mod bencode;
pub mod bitfield;
pub mod choker;
pub mod config;
pub mod extension;
pub mod http;
pub mod json;
pub mod magnet;
pub mod metadata;
pub mod metainfo;
pub mod pex;
pub mod picker;
pub mod resume;
pub mod storage;
pub mod torrent;
pub mod tracker;
pub mod wire;

// i2pnet is not consumed yet (that starts with `wire`/`supervisor`), but
// the dependency edge pins the direction: engine -> i2pnet, never reverse.
use i2pnet as _;
