# clove 🧄

clove is an I2P-only BitTorrent client.

> ⚠️ **Work in progress:** clove is currently under active development.
> If your personal safety depends on its anonymity, do not use it.

> 🤖 **Vibe-coded:** Much of this project was developed using AI.
>If you have any ethical or security concerns because of this,
> use something else.

## Overview

clove is a BitTorrent client for I2P. It is split into a long-running daemon
(`cloved`) and a control CLI (`clove`), and connects to an external I2P router
over SAMv3.

Unlike a general-purpose BitTorrent client with an anonymous mode, clove has no
clearnet mode to misconfigure. Peers are I2P destinations throughout the
engine, non-I2P trackers are discarded, and the only component allowed to open
an IP socket is the small `i2pnet` boundary that connects to a loopback SAM
bridge.

## Documents

- [`docs/SCOPE.md`](docs/SCOPE.md) — the spec: what clove is and is not
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — resolved design questions Q1–Q7
- [`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt) — the I2P-BitTorrent dialect, and every live finding
- [`docs/STATE-FORMAT.md`](docs/STATE-FORMAT.md) — the data directory and the resume file
- [`DEPENDENCIES.md`](DEPENDENCIES.md) — the dependency allowlist
- [`SECURITY.md`](SECURITY.md) — how to report a vulnerability, and what counts as one

Man pages are the primary user documentation and live in [`man/`](man):
`clove(1)`, `clove.conf(5)`, `clove-api(7)`, `cloved(8)`. Read them before
installing with `mandoc man/clove.1`, or after with `man clove`. This README
stays short on purpose.

## Confinement

Aspires to be leak-proof by construction, based on three independent layers (`docs/SCOPE.md` §5). No layer assumes that another is present:

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
