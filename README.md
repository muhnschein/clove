# clove

An I2P-only BitTorrent client. Daemon (`cloved`) + CLI (`clove`), speaking
SAMv3 to an external I2P router (i2pd, Java I2P, or emissary). Leak-proof by
construction: the engine has no IP vocabulary, and only the `i2pnet` crate
may touch a socket — enforced by lint and CI, not convention.

**Status: pre-alpha.** The client is feature-complete against its v1 scope and
proven end to end over an in-memory network — daemon, CLI, engine, tracker,
PEX, magnets, persistence. What it has *not* had is a live I2P router: that
sign-off is the outstanding work before 0.1 (see
[`docs/LIVE-TESTING.md`](docs/LIVE-TESTING.md)).

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

## Confinement

Three independent layers (`docs/SCOPE.md` §5), none assuming another is present:

1. **By construction** — the engine has no IP vocabulary and cannot open a
   socket; only `i2pnet` can, and only to a loopback SAM bridge. Enforced by
   `clippy.toml` and `ci/check-net-deps.sh`.
2. **Self-restriction** — after initialisation `cloved` confines itself with
   Landlock (filesystem down to the data directory; on ABI 4+, outbound TCP
   down to the SAM port) and a seccomp filter refusing exec, ptrace, module and
   BPF loading, mount, and unfamiliar address families. Best-effort: a kernel
   without them gets one log line, not a failed start.
3. **OS sandbox** — `contrib/systemd/` has a system unit and a per-user unit;
   the system one carries the `IPAddressDeny=any` clearnet lock.
   `contrib/netns/` documents the same lock for non-systemd hosts.

`contrib/podman/` has the i2pd quadlet used for live testing.

## Building and testing

Stable Rust, four runtime dependencies, no async runtime.

```
cargo build                 # or: make install PREFIX=/usr DESTDIR=pkg
make test                   # units + the hostile-input parser sweep
make smoke                  # the daemon end to end, no router needed
make chaos                  # SIGKILL storms and failed state writes
make man-lint               # the manuals still parse
```

Everything above runs from a clean checkout with no infrastructure. Two tiers
need more: `make test-live` wants a local I2P router (see
[`docs/LIVE-TESTING.md`](docs/LIVE-TESTING.md)), and `make fuzz` wants a
nightly toolchain (see [`fuzz/README.md`](fuzz/README.md)).

CI runs all of the above plus rustfmt, `clippy::pedantic` denied,
`cargo deny`, and `ci/check-net-deps.sh` — the gate that fails the build if a
socket-capable crate reaches the dependency tree without being allowlisted.
Debug builds additionally carry invariant assertions over the piece
accounting, the choke scheduler and the peer table; release builds do not.
