# Dependency Allowlist

Every direct dependency is listed here with a justification and its
transitive closure size (SCOPE §9: target ≤ ~15 direct dependencies).
Adding one requires a PR that updates this file; if it has any socket
capability it must also be added to `ci/check-net-deps.sh` in the same
commit. Closure sizes are recorded when the dependency is actually added.

## Current

- **`sha1` 0.11** (RustCrypto, `clove-core`) — entered Phase A (info-hash
  needs it at .torrent parse time, ahead of Phase C piece verification).
  SHA-1 is by protocol; rolling our own cryptographic hash is the one place
  hand-rolling is *worse* engineering. Small, `no_std`-capable, no proc
  macros. Transitive closure: RustCrypto-adjacent only (`digest`,
  `block-buffer`, `crypto-common`, `hybrid-array`, `typenum`, `cpufeatures`,
  `cfg-if`), none socket-capable.
- **`sha2` 0.11** (RustCrypto, `i2pnet`) — entered Phase D. A peer's
  32-byte destination hash is SHA-256 of its full I2P destination
  (`i2pnet::addr`). Kept on the same major as `sha1` so the two share one
  `digest` tree instead of pulling a second copy; when one moves, both move.
  No new socket-capable crates.
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
- **`getrandom` 0.2** (`cloved`) — entered Phase F. The API token and the
  peer-ID suffix are bytes straight from the OS RNG; `getrandom` is the
  maintained thin wrapper over `getrandom(2)`/`/dev/urandom`, exactly the
  syscall access we do not want to hand-roll. **Deliberately held at 0.2**
  rather than 0.3+: yosemite's `rand` already pulls 0.2, so sharing it costs
  one call-site API name and saves a duplicate crate in the tree. Revisit when
  yosemite moves to a `rand` built on 0.3. Tiny closure (`cfg-if`, `libc`);
  not socket-capable.

## Currency

Checked against crates.io on 2026-07-25. `yosemite` 0.7.0 is current;
`sha1`/`sha2` were moved 0.10 → 0.11 together; `getrandom` is held at 0.2 on
purpose (above). Total transitive closure: **39 crates**, the bulk of it
yosemite's (`rand`, `thiserror`, `nom`, `tracing` and the proc-macro trio).
`cargo tree -d` should report no duplicates; if it does, that is a review
topic, not a shrug.

## Approved, enters at the scheduled phase (docs/PLAN.md)

- **`landlock`** — Phase G. Landlock ABI probing and ruleset application for
  Layer-2 filesystem self-restriction; the raw syscall interface is unsafe
  and fiddly, and this crate is the maintained reference binding.
- **`seccompiler`** — Phase G. seccomp-BPF filter construction (Firecracker
  lineage, small, no proc macros) for Layer-2 syscall dropping.

Everything else in SCOPE §9's hand-roll list (bencode, config, arg parsing,
HTTP/1.1 both ends) is written in-tree, deliberately.
