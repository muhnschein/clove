# clove 🧄

A modern I2P-only BitTorrent client designed to have no clearnet path.

> ⚠️ **Work in progress:** clove is pre-alpha and under active development. If
> your personal safety depends on its anonymity, do not use it.

> 🤖 **Vibe-coded:** Much of this project was developed using AI.
> Treat its security properties as claims to verify, not a substitute for
> independent review. If that provenance conflicts with your requirements, use
> something else.

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
$ git clone https://github.com/vittuusaatanaperkele/clove.git
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

`cloved` runs in the foreground and logs to standard error. For regular use,
see [Running as a service](#running-as-a-service).

## Installation

clove is currently built from source.

### Requirements

- Rust 1.94.1 with `cargo`, `clippy`, and `rustfmt` (pinned in
  [`rust-toolchain.toml`](rust-toolchain.toml));
- an external i2pd or Java I2P router exposing SAMv3 over loopback; and
- Linux for Landlock/seccomp self-confinement and the bundled sandboxing
  recipes. Other platforms fall back without those Linux-specific layers and
  are not the primary deployment target.

### Install binaries and man pages

```console
# Current user
$ make install PREFIX="$HOME/.local"

# Or system-wide
$ sudo make install PREFIX=/usr/local
```

The installed manuals are `clove(1)`, `clove.conf(5)`, `clove-api(7)`, and
`cloved(8)`.

### Running as a service

After installing with `PREFIX="$HOME/.local"`, install the bundled user unit:

```console
$ install -Dm 0644 contrib/systemd/clove-user.service \
    ~/.config/systemd/user/clove.service
$ systemctl --user daemon-reload
$ systemctl --user enable --now clove
$ journalctl --user -u clove -f
```

The user unit applies filesystem, process, and syscall hardening. On most
systems a user service cannot enforce systemd's IP address filtering. For an
independent kernel-enforced clearnet lock, use the bundled
[`clove.service`](contrib/systemd/clove.service) system unit.

## Configuration

See [`clove.conf(5)`](man/clove.conf.5) for every setting and its default.

## Security model

clove aspires to be secure and I2P-only through three independent layers.
No layer assumes another is present.

1. **By construction.** Only `i2pnet` may use socket-capable APIs. The engine
   has no IP vocabulary, SAM addresses are restricted to loopback, DNS is not
   used, and non-I2P trackers are discarded. Lints and
   [`ci/check-net-deps.sh`](ci/check-net-deps.sh) enforce the dependency
   boundary in CI.
2. **Self-restriction.** After initialization, `cloved` attempts to confine its
   filesystem and outbound TCP with Landlock and deny unneeded syscall classes
   with seccomp. These mechanisms are best-effort; the daemon reports what was
   applied and continues if the running kernel cannot provide them.
3. **OS sandbox.** The system service and network-namespace recipe provide a
   separate deployment-level clearnet lock and further process hardening.

This model does not protect against a compromised kernel or I2P router,
resource exhaustion, an attacker who already controls the daemon's account, or
the normal visibility of your I2P destination to peers in a public swarm.

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

## License

clove is available under the [ISC License](LICENSE).