# Dependency Allowlist

Every direct dependency is listed here with a justification and its
transitive closure size (SCOPE §9: target ≤ ~15 direct dependencies).
Adding one requires a PR that updates this file; if it has any socket
capability it must also be added to `ci/check-net-deps.sh` in the same
commit. Closure sizes are recorded when the dependency is actually added.

## Current

- **`sha1` 0.11** (RustCrypto, `clove-core`).
  SHA-1 is by protocol; rolling our own cryptographic hash is the one place
  hand-rolling is *worse* engineering. Small, `no_std`-capable, no proc
  macros. Transitive closure: RustCrypto-adjacent only (`digest`,
  `block-buffer`, `crypto-common`, `hybrid-array`, `typenum`, `cpufeatures`,
  `cfg-if`), none socket-capable.
- **`sha2` 0.11** (RustCrypto, `i2pnet`). A peer's
  32-byte destination hash is SHA-256 of its full I2P destination
  (`i2pnet::addr`). Kept on the same major as `sha1` so the two share one
  `digest` tree instead of pulling a second copy; when one moves, both move.
  No new socket-capable crates.
- **`landlock` 0.4** (`cloved`, Linux only). Layer-2
  filesystem (and, on ABI 4+, outbound-TCP) self-restriction; see
  `crates/cloved/src/sandbox.rs`. The raw `landlock_*` syscalls are unsafe and
  the ABI negotiation is fiddly enough to get subtly wrong, which is the whole
  failure mode this layer exists to avoid — and the workspace forbids
  `unsafe_code`, so a hand-rolled binding is not on the table. Maintained by
  the Landlock authors. Not socket-capable: it takes rights away. Closure:
  `enumflags2` (+ its derive), `thiserror` 2, `libc`.
  Held at `0.4.7` or newer, for two things `0.4.5` did not have: `ABI::V9`, and
  with it `AccessFs::ResolveUnix` (Linux 7.1), which is how the daemon is stopped
  from connecting to any pathname unix socket; and `CompatLevel`, which is what
  lets ABI 6 be demanded as the documented floor while ABI 9 stays a bonus. The
  crate deliberately keeps the running kernel's ABI private — deriving access
  sets from it would make one build behave differently on two machines — so
  `CompatLevel` is the only sanctioned way to mix required and optional
  accesses, and the code uses it rather than probing.
- **`seccompiler` 0.5** (`cloved`, Linux only). Builds and
  installs the post-init seccomp-BPF allowlist filter. Firecracker lineage, small,
  the `json` feature (and its serde dependency) left off, so what we compile is
  the BPF backend and nothing else. Closure: `libc`.
- **`libc` 0.2** (`cloved`, Linux only). The seccomp filter
  names syscalls and the one address family it permits; these are the C ABI constants for them,
  and getting them from the maintained table beats a hand-written per-
  architecture list that is wrong on the one machine nobody tested. Already in
  the tree under `getrandom`, `landlock` and `seccompiler`, so this is a direct
  edge to an existing node, not a new crate. Not socket-capable in our use: we
  reference constants, and `unsafe_code = "forbid"` means we cannot call it.
- **`rustix` 1.1** (features = `["fs", "std"]`, `default-features = false`,
  `clove-core`). Torrent file names are attacker-supplied, and validating them
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
  `default-features = false` leaves it out. Because the capability is one word
  in a `Cargo.toml` away, `rustix` is listed in *both* the deny and allow sets
  of `ci/check-net-deps.sh`, and that script now also fails if any manifest
  turns the `net` feature on — the allowlist alone would have said nothing.
  That check is per-manifest, so it covers the second consumer below as well.

- **`getrandom` 0.2** (`cloved`). The API token and the
  peer-ID suffix are bytes straight from the OS RNG; `getrandom` is the
  maintained thin wrapper over `getrandom(2)`/`/dev/urandom`, exactly the
  syscall access we do not want to hand-roll. Tiny closure (`cfg-if`, `libc`,
  both already in the tree); not socket-capable.