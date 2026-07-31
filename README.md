# clove

clove is an I2P-only BitTorrent client. Split into daemon (`cloved`) and CLI (`clove`),
speaking SAMv3 to an external I2P router.

## Status

**Pre-alpha.** Feature-complete against its v1 scope — daemon, CLI,
engine, tracker, PEX, magnets, persistence — and it downloads from and seeds to
live swarms on i2pd and Java I2P. It runs many torrents under one budget and a
queue, and it answers Transmission's RPC, so tremc, transgui, Transdroid and
the \*arr download clients drive it without a web UI existing. Phase H added what running *many* torrents
needs: a client-wide peer budget, a download/seed queue, seeding limits,
torrents named by hash prefix, and `clove top`.

## Documents

- [`docs/SCOPE.md`](docs/SCOPE.md) — what clove is and is not (the spec)
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — resolved design questions Q1–Q7
- [`docs/LIVE-TESTING.md`](docs/LIVE-TESTING.md) — running against real routers, and the interop matrix
- [`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt) — the I2P-BitTorrent dialect, and every live finding
- [`docs/PHASE-F.md`](docs/PHASE-F.md) — daemon/CLI/API design, and the TUI decision
- [`docs/PHASE-I.md`](docs/PHASE-I.md) — the Transmission RPC surface, and what it refuses to invent
- [`docs/PHASE-H.md`](docs/PHASE-H.md) — the multi-torrent budget, the queue, seeding limits, and `clove top`
- [`docs/STATE-FORMAT.md`](docs/STATE-FORMAT.md) — the data directory and the resume file
- [`DEPENDENCIES.md`](DEPENDENCIES.md) — the dependency allowlist
- [`SECURITY.md`](SECURITY.md) — how to report a vulnerability, and what counts as one

Man pages are the primary user documentation and live in [`man/`](man):
`clove(1)`, `clove.conf(5)`, `clove-api(7)`, `cloved(8)`. Read them before
installing with `mandoc man/clove.1`, or after with `man clove`. This README
stays short on purpose.

## Confinement

Aspires to be leak-proof by construction. Three independent layers (`docs/SCOPE.md` §5), none assuming another is present:

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

## Development

**This is entirely vibe-coded. Here be dragons.**

Every defect that has mattered was found by running it against a real router
and a real swarm; none was reachable from a router-free test, and the unit
suite was green through all of them. They are recorded in
[`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt), each with the test that catches
it now. Before 0.1: the interop sign-off on i2pd and Java I2P
([`docs/LIVE-TESTING.md`](docs/LIVE-TESTING.md) §6.3). emissary is tracked in
the same table but no longer gates the release, and
[`docs/DECISIONS.md`](docs/DECISIONS.md) S1 says why and what would change it
back.


## Building and testing

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
