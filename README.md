# clove 🧄

A modern I2P-only BitTorrent client.

> ⚠️ **Work in progress:** clove is pre-alpha and under active development. If
> your personal safety depends on its anonymity, do not use it.

> 🤖 **Vibe-coded:** Much of this project was developed using AI.
> If that provenance troubles you, use something else.

> 🐧 **Linux-only:** clove targets a modern Linux kernel (6.12), seccomp,
> Landlock, and systemd, and two of its three security layers are built on
> them. Layer 2 applies what the running kernel offers and reports the rest —
> `sandbox require` in `clove.conf` turns anything less into a refusal to
> start. No effort is made to accommodate other platforms.

## Overview

clove is a BitTorrent client for I2P. It is split into a long-running daemon
(`cloved`) and a control CLI (`clove`), and connects to an external I2P router
over SAMv3.

Unlike a general-purpose BitTorrent client with an anonymous mode, clove has no
clearnet mode to misconfigure. Peers are I2P destinations throughout the
engine, non-I2P trackers are discarded, and the only component allowed to open
an IP socket is the small `i2pnet` boundary that connects to a loopback SAM
bridge.

The initial feature set is present and clove has downloaded and seeded on public I2P swarms through both i2pd and Java I2P. Interfaces may still change,
real-network testing remains limited, and the project should be treated as
unaudited.

## TL;DR

You need an I2P router exposing SAMv3 on loopback (by default
`127.0.0.1:7656`) and the Rust toolchain pinned by this repository.

```console
# Build and install clove for the current user.
$ git clone https://github.com/muhnschein/clove.git
$ cd clove
$ make install PREFIX="$HOME/.local"
$ export PATH="$HOME/.local/bin:$PATH"
$ cloved
```

In another terminal:

```console
# Add a torrent file or magnet link.
$ clove add ~/Downloads/release.torrent

# See the client and all hosted torrents.
$ clove list

# Refresh the same stable table every two seconds.
$ watch clove list
```

## Installation

clove is currently built from source.

### Requirements

- Rust 1.94.1 with `cargo`, `clippy`, and `rustfmt` (pinned in
  [`rust-toolchain.toml`](rust-toolchain.toml));
- an external i2pd or Java I2P router exposing SAMv3 over loopback; and
- Linux 6.12 or newer, with `seccomp` and Landlock available, on `x86_64`,
  `aarch64` or `riscv64` see
  [`docs/SCOPE.md`](docs/SCOPE.md) §0).

### Install binaries and man pages

```console
# Current user
$ make install PREFIX="$HOME/.local"

# Or system-wide
$ sudo make install PREFIX=/usr/local
```

## Documentation

The man pages are the primary user documentation:

- [`clove(1)`](man/clove.1) — CLI commands and examples;
- [`cloved(8)`](man/cloved.8) — daemon lifecycle, files, and confinement;
- [`clove.conf(5)`](man/clove.conf.5) — configuration and defaults; and
- [`clove-api(7)`](man/clove-api.7) — the local HTTP API.

Design and protocol documents live in the repository:

- [`docs/SCOPE.md`](docs/SCOPE.md) — goals, non-goals, and engineering scope;
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — resolved design decisions;
- [`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt) — I2P BitTorrent dialect and
  interoperability findings;
- [`docs/STATE-FORMAT.md`](docs/STATE-FORMAT.md) — persistent state format;
- [`DEPENDENCIES.md`](DEPENDENCIES.md) — reviewed dependency allowlist; and
- [`SECURITY.md`](SECURITY.md) — vulnerability policy and security guarantees.

## Security model

clove aspires to be secure and I2P-only through three independent layers.
No layer assumes another is present.

1. **By construction.** Only `i2pnet` may use socket-capable APIs. The engine
   has no IP vocabulary, SAM addresses are restricted to loopback, DNS is not
   used, and non-I2P trackers are discarded. Lints and
   [`ci/check-net-deps.sh`](ci/check-net-deps.sh) enforce the dependency
   boundary in CI.
2. **Self-restriction.** After initialization, `cloved` confines itself with
   Landlock — each directory to the rights that kind of directory actually needs,
   outbound TCP to the SAM port, and, on a new enough kernel, no connecting to
   any unix socket — and drops every syscall it no longer needs with a `seccomp`
   **allowlist**: anything not on the list returns `ENOSYS`. A few calls that
   are on it are restricted by argument as well, where the syscall number alone
   says too little: `socket(2)` to `AF_INET`/`SOCK_STREAM` (so no UDP, which
   Landlock's TCP rule does not reach), `ioctl(2)` to `FIONBIO`, and
   `mmap(2)`/`mprotect(2)` to mappings never writable and executable at once.
   The syscall list is measured from a traced run rather than guessed, and the
   daemon's own tests perform that workload under the live filter. What was
   actually applied stays available from `clove status` and `/v1/status`, since
   an unconfined daemon otherwise looks exactly like a confined one; `sandbox
   require` refuses to start instead.
3. **OS sandbox.** Two systemd units, and `make install` picks by prefix:
   [system](contrib/systemd/system/clove.service) provides a separate
   deployment-level clearnet lock (`IPAddressDeny=any`) and further process
   hardening; [user](contrib/systemd/user/clove.service) provides the part a
   user manager can enforce, which does *not* include that lock — systemd
   offers it to system services only.

This model does not protect against a compromised kernel or I2P router,
resource exhaustion in general, an attacker who already controls the daemon's
account, or the normal visibility of your I2P destination to peers in a public
swarm.

Please report suspected vulnerabilities through GitHub's private vulnerability
reporting—*not a public issue*. See [`SECURITY.md`](SECURITY.md) for scope and the
information that helps investigate a report.

## Limitations

- No clearnet or mixed-network mode—ever.
- No built-in Web UI—ever.
- No embedded I2P router.
- No I2P DHT; discovery currently uses trackers and peer exchange.
- No UDP tracker announces, BitTorrent v2, uTP, or local peer discovery.
- No daemon-less one-shot download mode.

See [`docs/SCOPE.md`](docs/SCOPE.md) for the full goals and non-goals.

## Testing

All routine tests run without an I2P router:

```console
$ make test       # unit, model, hostile-input, and evil-peer tests
$ make smoke      # daemon and CLI end to end
$ make chaos      # crash and failed-state-write scenarios
$ make router     # the SAM path against a fake bridge (no router needed)
$ make man-lint   # mdoc validation, when mandoc is installed
$ make doc-lint   # rustdoc links and warnings
$ make lint       # clippy with warnings denied
$ make fmt        # rustfmt check
```

`make fuzz` requires a nightly toolchain and `cargo-fuzz`; see
[`fuzz/README.md`](fuzz/README.md).

CI also checks the dependency allowlist and fails if a socket-capable crate
crosses the network boundary without review. Live interoperability findings are
recorded in [`docs/PROTOCOL.i2p-bt`](docs/PROTOCOL.i2p-bt).


## License

clove is available under the [ISC License](LICENSE).
