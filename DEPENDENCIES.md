# Dependency Allowlist

Every direct dependency is listed here with a justification and its
transitive closure size (SCOPE §9: target ≤ ~15 direct dependencies).
Adding one requires a PR that updates this file; if it has any socket
capability it must also be added to `ci/check-net-deps.sh` in the same
commit. Closure sizes are recorded when the dependency is actually added.

## Current

- **`sha1` 0.10** (RustCrypto) — entered Phase A (info-hash needs it at
  .torrent parse time, ahead of Phase C piece verification). SHA-1 is by
  protocol; rolling our own cryptographic hash is the one place
  hand-rolling is *worse* engineering. Small, `no_std`-capable, no proc
  macros. Transitive closure: 7 crates (`digest`, `block-buffer`,
  `crypto-common`, `generic-array`, `typenum`, `cpufeatures`, `cfg-if`),
  all RustCrypto-adjacent, none socket-capable.

## Approved, enters at the scheduled phase (docs/PLAN.md)

- **`getrandom`** — Phase C. Peer-ID randomness and API token generation
  straight from the OS RNG; the alternative (`rand`) pulls a larger tree we
  don't need.
- **`yosemite`** (features = `["sync"]`) — Phase D. The reason this project
  is buildable: SAMv3 sessions/streams/naming. ~3.2k lines, MIT, responsive
  author (R1); consumed only inside `i2pnet` behind our traits, vendorable
  if upstream stalls. The only socket-capable dependency — allowlisted in
  `ci/check-net-deps.sh`.
- **`landlock`** — Phase G. Landlock ABI probing and ruleset application for
  Layer-2 filesystem self-restriction; the raw syscall interface is unsafe
  and fiddly, and this crate is the maintained reference binding.
- **`seccompiler`** — Phase G. seccomp-BPF filter construction (Firecracker
  lineage, small, no proc macros) for Layer-2 syscall dropping.

Everything else in SCOPE §9's hand-roll list (bencode, config, arg parsing,
HTTP/1.1 both ends) is written in-tree, deliberately.
