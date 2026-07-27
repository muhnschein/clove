# clove

An I2P-only BitTorrent client. Daemon (`cloved`) + CLI (`clove`), speaking
SAMv3 to an external I2P router (i2pd, Java I2P, or emissary). Leak-proof by
construction: the engine has no IP vocabulary, and only the `i2pnet` crate
may touch a socket — enforced by lint and CI, not convention.

**Status: pre-alpha.** The client is feature-complete against its v1 scope and
downloads from live I2P swarms — daemon, CLI, engine, tracker, PEX, magnets,
persistence. First contact with real routers and real trackers (2026-07)
turned up seven defects that no router-free test could reach; they are fixed,
and the findings are in [`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt). The
remaining work before 0.1 is the interop sign-off across all three routers
(see [`docs/LIVE-TESTING.md`](docs/LIVE-TESTING.md)).

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

`contrib/podman/` has quadlets for all three I2P routers clove targets — i2pd,
Java I2P and emissary — which run side by side for the interop matrix.

## Building and testing

Stable Rust, seven direct dependencies (three of them Linux-only sandboxing),
no async runtime.

```
cargo build                 # or: make install PREFIX=/usr DESTDIR=pkg
make test                   # units, the hostile-input parser sweep, the evil-peer suite
make smoke                  # the daemon end to end, no router needed
make chaos                  # SIGKILL storms and failed state writes
make man-lint               # the manuals still parse
```

Everything above runs from a clean checkout with no infrastructure. The tiers
that need more want a local I2P router (see
[`docs/LIVE-TESTING.md`](docs/LIVE-TESTING.md)); `make fuzz` wants a nightly
toolchain (see [`fuzz/README.md`](fuzz/README.md)).

```
make swarm TORRENT='magnet:?xt=urn:btih:…'   # the real thing, against a real swarm
make test-live                               # the router-gated loopback tests
make report ARGS="--up --swarm magnet:?…"    # every tier, into one file
```

`make swarm` is the one to run first: it builds the binaries, points `cloved`
at your router, downloads a torrent you name from live i2psnark peers, seeds it
back, and prints a milestone table saying how far it got — tracker announce,
metadata, first peer, first verified piece, completion, PEX, bytes served, and
whether a remote peer dialed us. `make report` runs every tier that applies on
the machine and writes one file with the verdicts, the router versions and the
container logs, so a live session produces something reviewable rather than a
scrollback.

CI runs all of the above plus rustfmt, `clippy::pedantic` denied,
`cargo deny`, and `ci/check-net-deps.sh` — the gate that fails the build if a
socket-capable crate reaches the dependency tree without being allowlisted.
Debug builds additionally carry invariant assertions over the piece
accounting, the choke scheduler and the peer table; release builds do not.
