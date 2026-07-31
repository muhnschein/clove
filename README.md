# clove

clove is an I2P-only BitTorrent client. Split into daemon (`cloved`) and CLI (`clove`),
speaking SAMv3 to an external I2P router.

## Status

**Pre-alpha.** Feature-complete against its v1 scope — daemon, CLI,
engine, tracker, PEX, magnets, persistence — and it downloads from and seeds to
live swarms on i2pd and Java I2P. Phase H added what running *many* torrents
needs: a client-wide peer budget, a download/seed queue, seeding limits,
torrents named by hash prefix, and `clove top`.

## Documents

- [`docs/SCOPE.md`](docs/SCOPE.md) — what clove is and is not (the spec)
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — resolved design questions Q1–Q7
- [`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt) — the I2P-BitTorrent dialect, and every live finding
- [`docs/PHASE-F.md`](docs/PHASE-F.md) — daemon/CLI/API design, and the TUI decision
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

clove talks to any router exposing SAMv3. It is developed against i2pd and
Java I2P, and both have carried full downloads from public i2psnark swarms.

## Development

**This is entirely vibe-coded. Here be dragons.**

Every defect that has mattered was found by running it against a real router
and a real swarm; none was reachable from a router-free test, and the unit
suite was green through all of them. They are recorded in
[`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt), each with the test that catches
it now. clove has since downloaded and seeded a 3.2 GiB torrent from a public
i2psnark swarm on both i2pd and Java I2P.


## Building and testing

```
cargo build                 # or: make install PREFIX=/usr DESTDIR=pkg
make test                   # units, the hostile-input parser sweep, the evil-peer suite
make smoke                  # the daemon end to end, no router needed
make chaos                  # SIGKILL storms and failed state writes
make man-lint               # the manuals still parse
```

Everything above runs from a clean checkout with no infrastructure, and
nothing in this repo needs a router. `make fuzz` wants a nightly toolchain
(see [`fuzz/README.md`](fuzz/README.md)).

To exercise clove against a real router, run the daemon against one: point
`cloved` at your router's SAM port, add a torrent, and watch it.

CI runs all of the above plus rustfmt, `clippy::pedantic` denied,
`cargo deny`, and `ci/check-net-deps.sh` — the gate that fails the build if a
socket-capable crate reaches the dependency tree without being allowlisted.
Debug builds additionally carry invariant assertions over the piece
accounting, the choke scheduler and the peer table; release builds do not.
