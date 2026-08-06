# Scope Document — clove: an I2P-Only BitTorrent Client

**Name:** clove (daemon: `cloved`, control CLI: `clove`)

**Language:** Rust (stable toolchain) 

**SAM library:** yosemite (eepnet/yosemite)

**Engineering ethos:** see §9 — OpenBSD/OpenSSH/doas/opentracker/SQLite as the quality reference class

---

## 0. Target Platform

**Modern Linux, and nothing else.** clove requires a kernel of **6.12 or newer**,
`seccomp`, Landlock, and systemd, and is built and tested only there. The three
architectures the syscall filter is emitted for are `x86_64`, `aarch64` and
`riscv64`.

The choice follows OpenSSH's: support one platform properly rather than support
every platform partially. No portability shims, no feature detection beyond what
the kernel baseline already guarantees, no accommodation for other operating
systems in the code, the documentation, or the packaging.

---

## 1. Goals

Build a standalone, SAMv3-based BitTorrent client for the I2P network that is:

1. **Leak-proof by construction.** The client must be architecturally incapable of clearnet communication, independent of any OS-level sandboxing.
2. **Robust.** Correct handling of session loss, tunnel churn, router restarts, and misbehaving peers. This is the primary quality bar — the reason this project exists is that XD is flakey.
3. **Interoperable.** A first-class citizen on existing I2P swarms (i2psnark-dominated) and with both major router implementations.
4. **Operable.** A CLI pleasant enough for daily use, plus a local HTTP API for future frontends.

## 2. Non-Goals

- Clearnet BitTorrent support. Ever. This is an I2P-only client by design.
- No Web UI. Ever. The HTTP API is designed so another project can add a Web UI on top. This is not that project.
- i2p_dht (DHT over I2P). Deferred to v2+. Tracker + PEX covers current I2P swarm reality; i2psnark's DHT is its own protocol variant and a significant sub-project.
- UDP tracker announces (Prop 160, finalized 2025-06). Deferred; requires Datagram2/3 support end-to-end (router, SAM lib, trackers). HTTP announces work everywhere today. Revisit when tracker deployment exists.
- BitTorrent v2 (BEP 52), uTP, Local Peer Discovery, IP-based anything.
- Embedded router. We require an external router exposing SAMv3 (i2pd or Java I2P).
- Any platform that is not modern Linux (§0).
- A daemon-less one-shot download mode (think `clove fetch`). It would need a second lifecycle (session setup, download, teardown, all in one process) whose failure modes are not the daemon's, and every hour spent on it is an hour not spent on improving the daemon.

## 3. Initial Feature Cut

### Core engine
- BEP 3 peer wire protocol over I2P streams (SAM STREAM), including keep-alives, proper choke/interest state machines.
- BEP 10 extension protocol.
- BEP 9 metadata exchange (magnet links, `maggot`/i2p-style magnet handling as encountered in the wild).
- i2p_pex peer exchange (32-byte destination-hash format per the I2P BT spec).
- HTTP tracker announces over I2P (announce via SAM stream to tracker destination; compact peer response = concatenated 32-byte SHA-256 destination hashes, no port; non-compact destinations used directly).
- Multi-tracker support (BEP 12) — I2P announce URLs only; non-I2P announce URLs in torrent files are ignored, never resolved, never logged as anything but "skipped non-I2P tracker."
- Piece selection: rarest-first with endgame mode. Sequential mode as a per-torrent flag (nice for media).
- Seeding, super-seeding excluded (v2+ candidate).
- Fast extension (BEP 6) — i2psnark supports it, improves swarm behavior.
- SHA-1 verification on read-in and download; full recheck command.

### Address handling
- Peer identity = 32-byte destination hash. Full destination cache with b32 naming lookups (via SAM `NAMING LOOKUP`) and negative-result backoff.
- Never emit or accept IP/port peer representations anywhere in the codebase. There is no `IpAddr` in the engine's type vocabulary.

### Persistence
- Single client state directory (XDG-compliant, overridable).
- Per-torrent resume data: bitfield, verified-piece state, file priorities, stats (up/down totals), tracker state. Format: bencoded per Q2 decision; forward-versioned.
- Client destination keys persisted (stable identity across restarts) with an ephemeral-keys option per client or per torrent profile ("transient identity" mode).
- Atomic writes (write-temp-rename) for all state files. Crash at any point must never corrupt resume state — worst case is re-verification.
- **The file format is an API:** the resume/state format gets a written specification and a version field from day one. Policy: newer clove always reads older state; older clove refuses newer state cleanly (clear error, no write, no corruption). Format changes are release-notes headline items.

### Configuration
- Aspire to a doas/sshd-level of discipline.
- One config file, one format: flat `key value` lines, hand-parsed, comments with `#`. No TOML/YAML dependency, no nesting.
- **Unknown keys are a fatal error.** A typo'd option must fail startup loudly, never be silently ignored.
- `cloved -C` (or `--check`): parse and validate config, report, exit — for testing before restart.
- **Empty config is the safe, working default.** A fresh install against a default-configured local router (SAM at the standard address) downloads a torrent with zero config lines written. Every key exists to *deviate* from a sane default, not to enable basic function.
- All defaults documented in `clove.conf(5)`, actual-default values stated, not prose-approximated.

### CLI
- Daemon `cloved` + control CLI `clove`, speaking to the local HTTP API over a unix socket (default) or localhost TCP (opt-in).
- Commands: add (file/magnet), remove (with/without data), list, status (per torrent: peers, tunnels, speeds, availability), pause/resume, verify, set file priorities, client-level stats.
- Human-friendly default output (aligned tables, progress, rates); `--json` on every read command for scripting.
- Sensible exit codes, shell completion generation.
- Peer identity on the wire: Azureus-style peer-ID prefix and client name string chosen per Q7 and kept stable thereafter; checked against the informal BEP 20 registry to avoid collisions.

### HTTP API
- Local-only (unix socket default). Token auth even on localhost TCP.
- REST-ish JSON; versioned under `/v1/`. Explicitly not compatible with the Transmission/qBittorrent APIs in v1. Another project can add this if need be. This is not that project.

## 4. Architecture

```
+-------------------------------------------------------+
|  CLI (clove)          --unix socket-->   HTTP API      |
+-------------------------------------------------------+
|                Engine (sync threads) [rev]              |
|  torrent supervisor / piece picker / choker / storage   |
|  tracker client / pex / destination address book        |
+-------------------------------------------------------+
|                 i2pnet module (THE ONLY                 |
|              NETWORK-TOUCHING CODE, wraps               |
|                      yosemite)                          |
+-------------------------------------------------------+
                    | SAMv3 (localhost)
                    v
                I2P router
```

- **`i2pnet` module boundary.** All of yosemite is consumed behind our own trait (`I2pDialer`, `I2pListener`, `I2pNamingLookup`, later `I2pDatagram`). Rationale: yosemite is young and small; if it stalls or we outgrow it, we swap the impl without touching the engine. Also gives us a mock implementation for engine tests without a router.
- **Session topology:** one SAM PRIMARY session per client identity, with stream subsession for peer traffic. Tracker announces share the peer session's destination (this is what i2psnark does and what trackers expect — announced identity must match peer identity).
- **Reconnect discipline:** the SAM control socket, sessions, and forwarded listeners are supervised. Router restart ⇒ exponential-backoff resurrection of the full session tree, torrents transition to a visible "waiting for router" state, no thundering-herd re-announce. This state machine gets designed and tested explicitly.
- **Concurrency model:** synchronous, thread-per-peer with blocking I/O — the most simple and mostauditable; entirely viable at I2P scale (50–200 peers, high tunnel latency makes per-connection thread cost irrelevant). yosemite ships a first-class `sync` feature. Fallback if a concrete wall is hit: smol via yosemite's `smol` feature. The engine is written against narrow internal traits so this choice stays swappable longer than usual.
- **Storage:** file-backed with preallocation option; mmap explicitly out (predictable memory > speed here). Disk I/O and hashing on dedicated worker threads; bounded queues everywhere (no unbounded channels anywhere in the engine — lint-enforced).

## 5. No-Clearnet Enforcement (defense in depth, three independent layers)

**Layer 1 — by construction:**
- Only the `i2pnet` crate may depend on socket-capable APIs. The engine crates forbid `std::net` and `socket2` via `clippy` `disallowed_types`/`disallowed_methods` config + a CI grep gate over `Cargo.lock` (`ci/check-net-deps.sh`).
- `i2pnet` itself may open exactly one kind of socket: a TCP connection to the configured SAM address, which must be a loopback address or unix socket unless `--i-know-sam-is-remote` (explicit, ugly, documented as dangerous) is set. The opt-in localhost-TCP listener for the local HTTP API is also constructed inside `i2pnet` (loopback-validating helper), so every IP-socket construction site is in one crate.
- No DNS resolution code paths: hostnames are rejected in config except `localhost`; naming is I2P naming only.
- Dependency budget: every new transitive dependency with network capability requires justification in the PR. `cargo deny` config committed in-repo.

**Layer 2 — runtime self-restriction (pledge/unveil doctrine, Linux mechanisms):**
- The daemon restricts *itself* as it passes lifecycle phases, OpenSSH-style. After initialization (config read, data directory opened, SAM connected, control socket bound), it applies:
  - **Landlock**: filesystem access reduced to the data directory (and the watch directory, when one is configured), each to the rights that kind of directory actually needs — not `from_all()`. The state role gets 8 of the 16 filesystem rights; the watch role gets 4. What is withheld is the point: no `Execute` (which also denies mapping a file executable, something `seccomp`'s `execve` denial does not reach), no `MakeSym`, no device/socket/FIFO creation, and no `Refer`, since every rename the daemon makes is within one directory. Outbound TCP is confined to the SAM port (ABI 4), and reaching outside the domain by abstract unix socket or signal is scoped off (ABI 6). On ABI 9 (Linux 7.1) connecting to any *pathname* unix socket is denied too — handled and granted to nothing, since the control socket is bound before this point and only accepted on afterwards.
  - ABI 6 is the floor §0 commits to, so it is asked for as a `HardRequirement`; only the ABI 9 addition is best-effort. That is what makes the reported status mean something: the required tier cannot be partial, so a partial result has exactly one cause and the log line can name it. A kernel that cannot provide ABI 6 is reported as below the documented baseline, distinctly from Landlock being present but disabled. Still never fails startup, per the no-layer-assumes-another rule.
  - The control socket's directory is deliberately *not* granted. It was, so the socket "could be unlinked at exit" — but nothing unlinks it, and with `XDG_RUNTIME_DIR` set that directory is `/run/user/<uid>`, shared with every other application's runtime state.
  - **seccomp**: post-init syscall **allowlist** — anything not needed after initialization returns `ENOSYS`, which is the answer a libc's fallback paths are written for; `EPERM` is what stops glibc falling back from `clone3` to `clone`, and a hand-maintained list this short will be probed past eventually.
  - A syscall number is not always the whole policy, so four calls are restricted by **argument** as well. `socket(2)` to `AF_INET` *and* `SOCK_STREAM`: the family because the control listener is bound during initialization and only `accept4`-ed afterwards, and the type because Landlock's `ConnectTcp` says nothing about UDP, so this is where Layer 2 closes datagram egress. `ioctl(2)` to `FIONBIO`, its only use here, which is where interface enumeration via `SIOCGIFCONF` is closed — the last route by which a client with no IP vocabulary could ask what its IP is. `mmap(2)`/`mprotect(2)` to W^X, which is what Layer 3's `MemoryDenyWriteExecute=yes` has always required of the same process.
  - The allowlist is **measured, not guessed**, and re-measured whenever the daemon learns to do something new. The procedure: run `cloved` under `strace -f` against a SAM bridge complete enough to bring the whole network path up (session, forwarded listener, naming lookup, tracker announce, inbound peer), drive it through every API operation, split the trace at the `seccomp(2)` call that installs the filter, and take everything after it. Then add, with a written reason at each entry, the calls a trace cannot reach (clean shutdown, a block actually being written, `preallocate yes`) and the several spellings a libc or architecture may choose for one operation (`clone`/`clone3`, `poll`/`ppoll`, `mkdir`/`mkdirat`, the vDSO time fallbacks). `cloved`'s own confinement test then performs that workload under the live filter, so an omission fails in CI rather than in the field. The rig is committed, not a one-off: `ci/fake-sam.py` is the bridge and `ci/router.sh --trace` runs the measurement and fails if the allowlist refused anything the daemon needed.
- Destination-level restriction (loopback-only) is not expressible in these mechanisms alone; that remains Layer 1's (by construction) and Layer 3's (sandbox) job. Layer 2's guarantee is about post-init *capabilities*: no exec, no filesystem outside the data dir, no new privilege.

**Layer 3 — OS sandbox (shipped, documented, default in packaging):**
- systemd unit with `IPAddressDeny=any` + `IPAddressAllow=localhost`, `RestrictAddressFamilies=AF_UNIX AF_INET`, `PrivateDevices`, `ProtectSystem=strict` + `ReadWritePaths` for data dir, `NoNewPrivileges`, syscall filter, `LimitCORE=0` and `UMask=0077` (the destination key and the API token are in that address space).
- **Two units, because a user manager is not a system manager.** `IPAddressDeny=` is "only available for system services, not for per-user services" (systemd.resource-control(5)), and the filesystem-namespacing settings are unavailable to user services without `PrivateUsers=`. Neither is *refused* there, which is the trap: the firewall warns once per boot and passes every packet, and the namespacing logs at debug level and runs the service unconfined. A single file asking for both would therefore start, look healthy, and enforce neither. So `contrib/systemd/system/` carries the full unit and `contrib/systemd/user/` carries the seccomp-and-limits subset that a user manager applies in full, stating in its own header what it is not enforcing and offering the `PrivateUsers=` block that makes the rest either work or fail loudly. `make install` picks by prefix; the two cannot be merged anyway, since `DynamicUser=`, `StateDirectory=` and the `/etc` config path have no conditional form inside a unit.
- The client must behave correctly *inside* this sandbox (e.g., never attempt anything the sandbox would kill it for), and correctly *without* it (Layers 1–2 unaffected).

No layer assumes another is present.

## 6. Interoperability & Test Matrix

**Routers (SAM side):**

| Router | Priority | Notes |
|---|---|---|
| Java I2P | P0 | Sanity/reference; what most peers run behind. When routers disagree, this one is presumed right |
| i2pd | P0 | Also popular; SAM quirks differ from Java |

Priority is where to spend attention first, not what to skip. i2pd and Java
I2P — the deployment target and the reference, one C++ and one Java SAM
implementation — are the two clove is developed against, and both have carried
full downloads from public i2psnark swarms.

**Peer clients (swarm side):**

| Client | Priority | Notes |
|---|---|---|
| i2psnark | P0 | De facto reference for the I2P BT dialect; majority of swarm peers |
| BiglyBT (+I2P Helper) | P1 | Second-largest population; different codebase = different bugs |
| XD | P2 | Same-protocol sibling; useful for controlled two-client lab tests |

**Testing:** unit + engine tests against mocked `i2pnet`, the binaries end to
end (`make smoke`), crash resilience (`make chaos`) and coverage-guided fuzzing.
All of it is router-free and runs in CI. Behaviour against a real router is not
covered by any test in this repo — `crates/clove-core`'s loopback download is
`#[ignore]`d and needs a router and an environment variable to run by hand.

## 7. Risks & Open Questions

| # | Item | Plan |
|---|---|---|
| R1 | yosemite maturity (v0.7, few users) | Wrap behind `i2pnet` trait; vendor if needed; upstream fixes (author is responsive/active) |
| R2 | [Outdated] | [Outdated] |
| R3 | Datagram2/3 availability in yosemite + routers (gates future UDP announces, DHT) | Not needed for v1; track upstream |
| R4 | i2p_pex flag semantics underspecified ("review libtorrent source") | Conformance testing vs i2psnark; treat i2psnark behavior as normative |
| R5 | Tunnel latency vs choker/timeout tuning (clearnet BT timing assumptions are wrong on I2P) | Make all timeouts config-tunable; benchmark on live swarms; expect several rounds |
| R6 | Naming lookups for large peer sets (b32 resolution latency/failures) | Cache aggressively, cap concurrent lookups, negative caching |

## 8. Milestones

[Outdated]

## 9. Engineering Standards

Reference class: OpenBSD base, OpenSSH (non-portable), doas, opentracker, SQLite. This may be slop, but let's at least make it high quality slop.

### Smallness
- **Dependency allowlist, committed in-repo.** Every direct dependency is named in `DEPENDENCIES.md` with a one-paragraph justification and the size of its transitive closure. Target: **≤ ~15 direct dependencies**, transitive closure small enough that `cargo vendor` output is human-reviewable. Additions require the same scrutiny as adding code — because they are adding code.
- Prefer writing 300 focused lines over importing 30,000 general ones: arg parsing, the HTTP/1.1 server for the local API, bencode, and config parsing are all candidates for hand-rolled implementations (bencode especially — it is ~200 lines done carefully, and we need hostile-input control over it anyway).
- No proc-macro-heavy frameworks.
- **LOC as a watched metric.** Not a hard cap, but unexplained growth is considered a review topic. Take pride in what *isn't* there.

### Code quality
- `#![forbid(unsafe_code)]` in every crate except (if ever needed) a single documented exception with rationale — expected count: zero.
- `clippy::pedantic` baseline with a short, committed, individually-justified exception list. rustfmt, no bespoke style.
- Every state machine (choker, peer connection lifecycle, SAM session supervision) documented as an explicit enum with an exhaustive transition table in comments or docs — no implicit states smeared across booleans.
- Errors: small hand-written error enums per module. No `anyhow` in library code; error text written for the operator reading a log at 2 a.m.
- Panics are bugs. `unwrap`/`expect` forbidden by lint outside tests; every `expect` in test-support code states its invariant.

### Documentation
- **Man pages are the primary user documentation**: `cloved(8)`, `clove(1)`, `clove.conf(5)`, and the HTTP API documented in a `clove-api(7)`-style page. Every page has a real EXAMPLES section. README stays short and defers to them.
- **`PROTOCOL.i2p-bt`**: file recording our precise interpretation of the I2P BitTorrent dialect — every place the upstream spec is vague gets our observed-behavior notes and the decision we made. This doubles as our interop lab notebook and is a deliverable, not an afterthought.
- rustdoc on every public item (`#![deny(missing_docs)]` on library crates); module-level docs explain *why*, not just *what*.

### Testing
- Aspiration: **test code volume exceeds source code volume.** Tracked, not enforced by gate.
- **Paranoid debug builds:** release builds stay lean; debug builds are dense with invariant assertions (piece accounting sums, choker state consistency, bitfield/on-disk agreement, session-tree supervision invariants) that run continuously under CI and fuzzing. Bugs should be unable to survive contact with the assertion net even when no test targets them directly.
- Hostile-input torture suites as first-class citizens: malformed/adversarial bencode, evil peer behavior (protocol violations, slow-loris, bad hashes, PEX spam), truncated/corrupted resume files, SAM bridge lying or dying mid-operation. Fuzzing (cargo-fuzz) for every parser.
- Chaos tests (router kill/restart, disk-full, SIGKILL during state write) run in CI, not just manually.
- **Regress runnable by anyone:** `make test` from a clean checkout runs the whole suite with zero infrastructure. If contributors can't run the tests, the tests decay into ours alone, then nobody's.

### Releases & project hygiene
- **Date-based versioning:** No major/minor/patch-releases. Just simple iterations, e.g. *2026.08*.
- **Boring is good:** Few, boring, well-tested releases over frequent ones.
- **Culture of deletion:** every feature must justify its continued existence at each release; removals are announced proudly in release notes, not buried. The LOC metric above is allowed — encouraged — to go down.

## 10. Out of Scope Forever

Clearnet peers/trackers, mixed-network mode, outproxy usage of any kind, telemetry of any kind.
