//! `cloved(8)` — the clove daemon.
//!
//! Hosts the engine, speaks `SAMv3` to the local router through `i2pnet`, and
//! serves the local HTTP API (hand-rolled HTTP/1.1 per Q6, unix socket by
//! default). Built in Phase F; Layer-2 self-restriction (Landlock/seccomp
//! with graceful fallback) lands in Phase G. See `docs/PLAN.md`.

fn main() {
    eprintln!("cloved: not implemented yet — bootstrap skeleton (see docs/PLAN.md)");
    std::process::exit(1);
}
