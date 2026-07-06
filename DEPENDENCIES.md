# Dependency Allowlist

Every direct dependency is listed here with a justification and its
transitive closure size (SCOPE §9: target ≤ ~15 direct dependencies).
Adding one requires a PR that updates this file; if it has any socket
capability it must also be added to `ci/check-net-deps.sh` in the same
commit. Closure sizes are recorded when the dependency is actually added.

## Current

- **`sha1` 0.10** (RustCrypto, `clove-core`) — entered Phase A (info-hash
  needs it at .torrent parse time, ahead of Phase C piece verification).
  SHA-1 is by protocol; rolling our own cryptographic hash is the one place
  hand-rolling is *worse* engineering. Small, `no_std`-capable, no proc
  macros. Transitive closure: 7 crates (`digest`, `block-buffer`,
  `crypto-common`, `generic-array`, `typenum`, `cpufeatures`, `cfg-if`),
  all RustCrypto-adjacent, none socket-capable.
- **`sha2` 0.10** (RustCrypto, `i2pnet`) — entered Phase D. A peer's
  32-byte destination hash is SHA-256 of its full I2P destination
  (`i2pnet::addr`). Pinned to 0.10 (not 0.11) so it shares `sha1`'s
  `digest` 0.10 tree instead of pulling a second copy. No new socket-
  capable crates.
- **`yosemite` 0.7** (features = `["sync"]`, `i2pnet`) — entered Phase D.
  The reason this project is buildable: SAMv3 sessions/streams/naming.
  MIT, responsive author (R1); consumed only inside `i2pnet` behind our
  traits, vendorable if upstream stalls. The only socket-capable
  dependency — allowlisted in `ci/check-net-deps.sh`. Its closure is the
  largest single cost in the tree (~24 crates: `rand`, `thiserror`, `nom`,
  `tracing`, and the `syn`/`proc-macro2`/`quote` proc-macro trio via
  `thiserror-impl`/`tracing-attributes`). This exceeds the §9 "no
  proc-macro-heavy frameworks" preference, but it is the accepted price of
  the one library that speaks SAM, and it is wrapped so the engine never
  sees it. Reviewed for socket capability: none of the closure beyond
  `yosemite` itself opens sockets.

## Approved, enters at the scheduled phase (docs/PLAN.md)

- **`getrandom`** — Phase F. API token generation straight from the OS RNG
  (peer-ID randomness is covered transitively by yosemite's `rand`). The
  alternative (`rand` directly) pulls a larger tree we don't need.
- **`landlock`** — Phase G. Landlock ABI probing and ruleset application for
  Layer-2 filesystem self-restriction; the raw syscall interface is unsafe
  and fiddly, and this crate is the maintained reference binding.
- **`seccompiler`** — Phase G. seccomp-BPF filter construction (Firecracker
  lineage, small, no proc macros) for Layer-2 syscall dropping.

Everything else in SCOPE §9's hand-roll list (bencode, config, arg parsing,
HTTP/1.1 both ends) is written in-tree, deliberately.
