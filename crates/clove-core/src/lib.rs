//! The clove engine: everything between the `i2pnet` boundary below and the
//! daemon/API above. This crate never touches a socket — peer addressing is
//! [`i2pnet::DestHash`] only, enforced by the workspace `clippy.toml`.

pub mod bencode;
pub mod bitfield;
pub mod budget;
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
