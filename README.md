# clove

An I2P-only BitTorrent client. Daemon (`cloved`) + CLI (`clove`), speaking
SAMv3 to an external I2P router (i2pd, Java I2P, or emissary). Leak-proof by
construction: the engine has no IP vocabulary, and only the `i2pnet` crate
may touch a socket — enforced by lint and CI, not convention.

**Status: pre-alpha bootstrap.** The workspace skeleton, no-clearnet
enforcement gates, and design documents are in place; the engine is being
built per the phase plan.

## Documents

- [`docs/SCOPE.md`](docs/SCOPE.md) — what clove is and is not (the spec)
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — resolved design questions Q1–Q7
- [`docs/PLAN.md`](docs/PLAN.md) — implementation phases and milestones
- [`docs/LIVE-TESTING.md`](docs/LIVE-TESTING.md) — closing M1/M3 against a real router (podman/quadlet)
- [`DEPENDENCIES.md`](DEPENDENCIES.md) — the dependency allowlist

Man pages (`cloved(8)`, `clove(1)`, `clove.conf(5)`) become the primary user
documentation from M4; this README stays short on purpose.

## Building

Stable Rust. `cargo build`, `cargo test`. CI additionally runs rustfmt,
`clippy::pedantic`, `cargo deny`, and `ci/check-net-deps.sh` (the
no-clearnet dependency gate).
