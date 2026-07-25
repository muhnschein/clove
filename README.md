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
- [`docs/PHASE-F.md`](docs/PHASE-F.md) — daemon/CLI/API design, and the TUI decision
- [`DEPENDENCIES.md`](DEPENDENCIES.md) — the dependency allowlist
- [`SECURITY.md`](SECURITY.md) — how to report a vulnerability, and what counts as one

Man pages are the primary user documentation and live in [`man/`](man):
`clove(1)`, `clove.conf(5)`, `clove-api(7)`, `cloved(8)`. Read them before
installing with `mandoc man/clove.1`, or after with `man clove`. This README
stays short on purpose.

## Packaging

`contrib/systemd/` has a system unit and a per-user unit; the system one carries
the `IPAddressDeny=any` clearnet lock (Layer 3, `docs/SCOPE.md` §5).
`contrib/netns/` documents the same lock for non-systemd hosts.
`contrib/podman/` has the i2pd quadlet used for live testing.

## Building

Stable Rust. `cargo build`, `cargo test`, `make smoke` (end-to-end, no router
needed), `make install` (honors `PREFIX`/`DESTDIR`). CI additionally runs rustfmt,
`clippy::pedantic`, `cargo deny`, and `ci/check-net-deps.sh` (the
no-clearnet dependency gate).
