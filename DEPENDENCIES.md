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

  **Narrowed to `NAMING LOOKUP` alone**, over two steps on 2026-07-27/28.
  Dialing left first: yosemite's session controller is one state machine shared
  by the control connection and every stream operation, and a stream failure on
  an unexpected path poisons it for the life of the session
  (`docs/PROTOCOL.i2p-bt` §2.12). `SESSION CREATE` and `STREAM FORWARD`
  followed, for a different reason — clove has to *read* its own control
  connection for the life of the session, to answer the router's `PING` (Java
  I2P ends a session that does not) and to hear what the router says when a
  session ends, and yosemite owns that socket and exposes it only through a
  write-then-read-one-line call (§2.13). Closing §2.7's `SESSION CREATE` hang
  fell out of the same change, the deadline now being ours to set.

  What remains is `RouterApi::lookup_name`: one socket, opened and closed per
  lookup, with no session state behind it — the shape yosemite is unambiguously
  good at. Still worth its place for that, but it now carries very little of
  clove's runtime, and R1's "vendor if upstream stalls" is cheaper than it was.
  The closure has not shrunk: `yosemite` is one crate in the `Cargo.toml`
  either way, and the ~24-crate cost above is unchanged.

- **`landlock` 0.4** (`cloved`, Linux only) — entered Phase G. Layer-2
  filesystem (and, on ABI 4+, outbound-TCP) self-restriction; see
  `crates/cloved/src/sandbox.rs`. The raw `landlock_*` syscalls are unsafe and
  the ABI negotiation is fiddly enough to get subtly wrong, which is the whole
  failure mode this layer exists to avoid — and the workspace forbids
  `unsafe_code`, so a hand-rolled binding is not on the table. Maintained by
  the Landlock authors. Not socket-capable: it takes rights away. Closure:
  `enumflags2` (+ its derive), `thiserror` 2, `libc`.
- **`seccompiler` 0.5** (`cloved`, Linux only) — entered Phase G. Builds and
  installs the post-init seccomp-BPF deny filter. Firecracker lineage, small,
  the `json` feature (and its serde dependency) left off, so what we compile is
  the BPF backend and nothing else. Closure: `libc`.
- **`libc` 0.2** (`cloved`, Linux only) — entered Phase G. The seccomp filter
  names syscalls and address families; these are the C ABI constants for them,
  and getting them from the maintained table beats a hand-written per-
  architecture list that is wrong on the one machine nobody tested. Already in
  the tree under `getrandom`, `landlock` and `seccompiler`, so this is a direct
  edge to an existing node, not a new crate. Not socket-capable in our use: we
  reference constants, and `unsafe_code = "forbid"` means we cannot call it.
- **`rustix` 1.1** (features = `["fs", "std"]`, `default-features = false`,
  `clove-core`) — entered 2026-07-29, closing the path-traversal finding
  (M-01). Torrent file names are attacker-supplied, and validating them
  lexically — no separators, no `..` — says nothing about what the filesystem
  does with them: a symlink already sitting under the download directory turns
  an ordinary join-and-open into a write outside it. The fix is to walk the
  components as directory descriptors with `openat`/`mkdirat`/`unlinkat`
  carrying `O_NOFOLLOW`, so the refusal is the kernel's and there is no window
  between checking and acting. std has no `openat`, and `unsafe_code =
  "forbid"` rules out calling it through `libc` ourselves — the same reasoning
  that brought in `landlock`.

  Chosen over `cap-std`, which offers the same guarantee through a much larger
  surface (it replaces `std::fs` wholesale and pulls `rustix` in anyway).
  Closure: `bitflags`, `linux-raw-sys` on Linux; `errno` and `windows-sys` are
  target-gated and never compiled here. Six lockfile entries, three of them
  built.

  **Socket-capable behind a feature we do not enable.** `rustix::net` exists;
  `default-features = false` with only `fs` and `std` leaves it out. Because
  the capability is one word in a `Cargo.toml` away, `rustix` is listed in
  *both* the deny and allow sets of `ci/check-net-deps.sh`, and that script now
  also fails if any manifest turns the `net` feature on — the allowlist alone
  would have said nothing.
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
purpose (above). Total transitive closure: **48 crates**, the bulk of it
yosemite's (`rand`, `thiserror`, `nom`, `tracing` and the proc-macro trio).

Counted as `Cargo.lock` entries less the four workspace members:

```
grep -c '^name = ' Cargo.lock   # 52, less the 4 workspace crates
```

It counts the duplicated crates below twice and includes the target-gated
`rustix` entries (`errno`, `windows-sys`, `windows-link`) that are never
compiled on Linux. **Recounted 2026-07-30**, and restated against that command
rather than left as a bare figure: it read 46 when written, which was the whole
lockfile *including* the workspace members, and `rustix` then added its
documented six entries. Two different countings of one number is how it drifted
in the first place — the command above is now the definition.

`cargo tree -d` should report no duplicates; if it does, that is a review
topic, not a shrug. It currently reports one, arriving with Phase G:
`thiserror` (and so `thiserror-impl` and `syn`) exists twice, because
`yosemite` is on `thiserror` 1 and `landlock` is on `thiserror` 2. Three
duplicated crates, all build-time-only in the second copy. The two ways out
are both worse than the duplicate: pinning `landlock` back to a 0.4.x on
`thiserror` 1 downgrades the crate whose correctness is the entire point of
Layer 2, and dropping `landlock` means hand-written `landlock_*` syscalls in a
workspace that forbids `unsafe_code`. The real fix is upstream — recheck when
`yosemite` moves to `thiserror` 2.