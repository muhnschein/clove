# Security Policy

clove exists to move data over I2P and nothing else. A bug that puts traffic,
or a user's identity, anywhere but I2P is the most serious class of defect this
project has. This file says how to report one.

## Reporting a vulnerability

**Use GitHub's private vulnerability reporting on this repository**. That is the
only reporting channel. Do not open a public issue for a suspected vulnerability.

Please include the clove version or commit, the I2P router and version, what you
observed, and a reproduction if you have one.

## What counts as a vulnerability

**Leak-class — the highest severity here.** Anything that causes clove to
communicate outside I2P, or that ties an I2P identity to a network identity:

- Any traffic to a non-loopback address other than the configured SAM bridge.
- Any DNS lookup, or any resolution path outside SAM naming.
- Contacting a non-I2P tracker or peer, however the address arrived.
- Leaking the client's destination, or a peer's, to somewhere it does not
  belong — including logs, error messages, or the local API.
- A way to defeat the `i2pnet` boundary from engine code.

**Also in scope:**

- Remote crashes, hangs, or unbounded memory growth triggered by a peer,
  tracker, `.torrent`, magnet, or resume file. Parsers here are hostile-input
  surfaces by assumption.
- Local API authentication bypass, or disclosure of the API token or the
  client's destination key to another local user.
- The destination key, or any part of the SAM `DESTINATION=` blob behind it,
  reaching a tracker, a peer, a log or the control API. Only the public
  destination on the front of that blob may ever leave the process
  (`docs/PROTOCOL.i2p-bt` §5.1c).
- State-file corruption that survives a restart, or resume data that causes
  clove to trust unverified pieces.
- Path traversal from torrent file names.

**Out of scope:**

- Weaknesses in I2P itself, or in the router. Report those to the router
  project; we will help you route it if you are unsure.
- Attacks that require an already-compromised machine or the daemon's own user
  account. The API token protects against other local users, not against
  someone who can read the data directory.
- Deanonymisation inherent to running a public BitTorrent swarm — that your
  destination is visible to peers you connect to is how the protocol works.

## Design guarantees a report can hold us to

These are enforced in the codebase and checked in CI, not merely intended:

- Only the `i2pnet` crate may open a socket. The engine crates cannot: it is
  enforced by `clippy` type bans and by `ci/check-net-deps.sh`, which fails the
  build if a socket-capable dependency appears without being allowlisted.
- The engine has no IP or port vocabulary at all. Peers are 32-byte I2P
  destination hashes end to end.
- The only outbound IP socket is to the SAM bridge, which must be loopback.
  A remote `sam_address` is refused when the configuration is parsed, and
  there is no override — see `clove.conf(5)`.
- Announce URLs that are not I2P URLs are dropped when the torrent is parsed —
  never contacted, never logged beyond a count.
- State files are written temp-then-rename, so a crash cannot corrupt them.
- After initialisation the daemon holds a `seccomp` **allowlist**: a syscall it
  does not need returns `ENOSYS`, whether or not anyone thought to name it. The
  permitted set is measured from a traced run rather than guessed, and the test
  suite performs that workload under the live filter.
- **What the sandbox came to is observable and can be demanded.** Which of
  Landlock and `seccomp` applied is reported at startup and, for the life of the
  process, in `clove status` and `GET /v1/status`. `sandbox require` makes an
  incomplete confinement a refusal to start; it is off by default.
- **No post-initialisation UDP.** `socket(2)` is permitted only for
  `AF_INET`/`SOCK_STREAM`, so the datagram path a Landlock TCP rule cannot
  reach is closed by the syscall filter instead. `ioctl(2)` is likewise
  permitted only for `FIONBIO`, which leaves no route to `SIOCGIFCONF` and the
  local interface addresses.
- **One peer destination cannot take over a torrent.** It may hold at most a
  couple of concurrent connections, so it can neither fill the peer table nor
  become the only availability the piece picker can see.
- **Peers advertised over PEX cannot crowd out a tracker's.** The known-peer set
  is capped, and at the cap a PEX-learned entry is evicted to make room for a
  destination from a tracker, an inbound connection or the operator. PEX itself
  evicts nothing.
- **Foreign text is scrubbed where it becomes a terminal's input**, in the
  daemon's log lines and in `clove`'s output alike: control characters and
  bidirectional overrides out of a torrent, a tracker or a SAM bridge cannot
  forge a log line or rewrite what the rest of the screen says. The stored value
  and the API's JSON keep the real name.

## Where each guarantee is checked

A guarantee that nothing executes is a comment. Each of the above is held by
something that runs in CI on every push (`.github/workflows/ci.yml`) and can
be run from a clean checkout with `make test`, `make smoke`, `make chaos` and
`make router`; the fuzz targets run nightly. When one of these rows changes,
change the other.

| Guarantee | What enforces it |
|---|---|
| Only `i2pnet` may open a socket; no IP or port vocabulary in the engine | `clippy.toml` type and method bans, denied workspace-wide; `ci/check-net-deps.sh` over `Cargo.lock` and the `rustix` feature list |
| The SAM bridge is loopback, and there is no override | `config::tests::a_sam_address_this_build_cannot_dial_is_refused`; `i2pnet::sam` carries only a port and dials `Ipv4Addr::LOCALHOST` by type |
| Non-I2P announce URLs are dropped at parse time and never contacted | `metainfo::tests::i2p_tracker_filter`, `filters_and_counts_non_i2p_trackers`; `magnet::tests`; the `metainfo` and `magnet` fuzz targets assert every surviving tracker is an I2P URL |
| State files are written temp-then-rename | `ci/chaos.sh`: SIGKILL storms during state writes, torn temporaries, an unwritable state directory — and it checks that each kill actually landed |
| The post-initialisation `seccomp` allowlist, and no post-init UDP | `sandbox::tests::child_under_landlock` performs the daemon's workload under the live filter and proves `exec`, `link`, `symlink` and `socket(AF_UNIX, SOCK_DGRAM)` are refused; `ci/router.sh --trace` fails if the daemon makes any post-init call the filter refuses |
| What the sandbox came to is reported and can be required | `main::tests::sandbox_require_refuses_an_incomplete_confinement`, `the_status_reports_the_sandbox_verdict`; `ci/router.sh` prints the verdict on every run |
| The private key never leaves the process | `ci/router.sh` plants a marker in the bridge's key blob and fails if it appears in the daemon's log, the announce the tracker received, the CLI's output or any state file but `destination.key`; `sam::tests::a_session_publishes_its_destination_and_never_its_private_keys`, `a_config_debug_print_redacts_the_private_key` |
| Local API: token on every request, secrets `0600` and refused otherwise | `main::tests::every_request_needs_the_token`, `authentication_precedes_routing`, `an_empty_token_authenticates_nobody`, `a_world_readable_secret_is_refused_rather_than_used`, `a_symlinked_secret_is_refused`; `ci/smoke.sh` checks the token file mode |
| One daemon per data directory; a live control socket is never unlinked | `ci/smoke.sh` starts a second `cloved` on the same directory and expects a refusal; `api::tests::a_live_socket_is_not_taken_away_from_its_listener`, `a_stale_socket_is_replaced` |
| One peer destination cannot take over a torrent | `evil_peer::one_destination_cannot_monopolise_the_peer_table`, and the hostile-peer suite in `crates/clove-core/tests/evil_peer.rs` (slow-loris, stop-reading, corrupt blocks, straddling requests, silent peers) |
| PEX cannot crowd out a tracker's peers | `evil_peer::a_pex_flood_cannot_shut_out_the_trackers_peers`, `peer_exchange_stays_within_the_limit_it_enforces`; the `extensions` fuzz target asserts the PEX cap |
| Foreign text is scrubbed wherever it becomes a terminal's input | `text::tests` (controls, bidi, format characters), `text::tests::the_i2pnet_scrubber_agrees_on_every_character` (the two scrubbers hold one table), `clove::tests::a_torrent_name_cannot_drive_the_terminal`, `sam::tests::a_forged_ok_inside_a_refusal_is_still_a_refusal_and_is_scrubbed`; the `text` fuzz target asserts the class property over arbitrary input |
| Parsers survive hostile input | `crates/clove-core/tests/hostile.rs` (a deterministic mutation sweep on every push) and eleven `cargo-fuzz` targets with dictionaries and a committed seed corpus (`fuzz/README.md`) |

If you find a way to violate one of these, that is a vulnerability by
definition, even without a demonstrated exploit.