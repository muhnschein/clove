//! The clove engine: everything between the `i2pnet` boundary below and the
//! daemon/API above. This crate never touches a socket — peer addressing is
//! [`i2pnet::DestHash`] only, enforced by the workspace `clippy.toml`.
//!
//! All of Phases A–E are present (`docs/PLAN.md`); the list below is what is
//! here, grouped by what each module is for rather than by the phase it
//! arrived in.
//!
//! **Parsers — pure, hostile-input hardened, every one a fuzz target.**
//! [`bencode`] (the codec everything else is built on), [`metainfo`]
//! (.torrent, with the I2P-only announce filter), [`magnet`] (BEP 9 links),
//! [`config`] (flat `key value`, unknown key fatal), [`resume`] (versioned
//! bencode state), [`http`] (the minimal HTTP/1.1 both the tracker client and
//! the API server use, Q6) and [`json`] (API bodies, hand-rolled per SCOPE §9).
//!
//! **Protocol.** [`wire`] (BEP 3 codec, BEP 6 fast extension, the handshake),
//! [`extension`] (BEP 10 negotiation), [`metadata`] (`ut_metadata`),
//! [`pex`] (`i2p_pex`, where i2psnark's behaviour is normative — R4),
//! [`tracker`] (announces over I2P streams; compact responses are
//! concatenated 32-byte destination hashes, never IP/port).
//!
//! **Policy — pure decisions, no I/O, so they test without a network.**
//! [`picker`] (rarest-first, endgame, per-torrent sequential) and [`choker`]
//! (choke/interest, all intervals config-tunable per R5). [`bitfield`] is the
//! piece-set representation they share with `wire` and `resume`.
//!
//! **Moving parts.** [`storage`] (file-backed pieces, SHA-1 verify, recheck),
//! [`torrent`] (the coordinator: peer table, connection lifecycle, the Q5
//! thread-per-peer model) and [`swarm`] (peer acquisition — the dial sweep and
//! the inbound acceptor, generic over the `i2pnet` traits so the mock network
//! proves the logic in CI).
//!
//! Two module names the plan used did not survive contact: `peer` is part of
//! [`torrent`], because a connection's lifecycle is inseparable from the table
//! that owns it, and the session supervision the plan filed here lives in
//! [`i2pnet::supervisor`] — below this crate, since it is the session tree's
//! concern and the engine only ever sees "waiting for router".

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
pub mod swarm;
pub mod torrent;
pub mod tracker;
pub mod wire;

// Re-exported so the engine's own peer-address vocabulary has one name and
// one source: a 32-byte destination hash, never an IP. The dependency edge
// only ever points this way, engine -> i2pnet.
pub use i2pnet::DestHash;
