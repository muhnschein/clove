# Scope Document — clove: an I2P-Only BitTorrent Client

**Status:** Accepted (rev 1). Two `[rev]` edits against the draft: the §4
diagram no longer names tokio (contradicted Q5's lean, since decided — see
`DECISIONS.md`), and sections are renumbered to close the draft's §8→§10 gap.
Open questions Q1–Q7 are resolved in `DECISIONS.md` as reversible defaults.
**Name:** clove (daemon: `cloved`, control CLI: `clove`) — pending final trademark/crates.io/distro-package sweep
**Language:** Rust (stable toolchain; concurrency model per Q5)
**SAM library:** yosemite (eepnet/yosemite)
**Engineering ethos:** see §9 — OpenBSD/OpenSSH/doas/opentracker/SQLite as the quality reference class

---

## 1. Goals

Build a standalone, SAMv3-based BitTorrent client for the I2P network that is:

1. **Leak-proof by construction.** The client must be architecturally incapable of clearnet communication, independent of any OS-level sandboxing.
2. **Robust.** Correct handling of session loss, tunnel churn, router restarts, and misbehaving peers. This is the primary quality bar — the reason this project exists is that XD is flakey.
3. **Interoperable.** A first-class citizen on existing I2P swarms (i2psnark-dominated) and with both major router implementations plus emissary.
4. **Operable.** A CLI pleasant enough for daily use, plus a local HTTP API for future frontends.

## 2. Non-Goals (v1)

- Clearnet BitTorrent support. Ever. This is an I2P-only client by design.
- i2p_dht (DHT over I2P). Deferred to v2. Tracker + PEX covers current I2P swarm reality; i2psnark's DHT is its own protocol variant and a significant sub-project.
- UDP tracker announces (Prop 160, finalized 2025-06). Deferred; requires Datagram2/3 support end-to-end (router, SAM lib, trackers). HTTP announces work everywhere today. Revisit when tracker deployment exists.
- Web UI. Deferred to v2. The HTTP API is designed so a web UI can be added without engine changes.
- BitTorrent v2 (BEP 52), uTP, Local Peer Discovery, IP-based anything.
- Embedded router. We require an external router exposing SAMv3 (i2pd, Java I2P, or emissary).
- `clove fetch`, the daemon-less one-shot download mode. Entered §3 as an explicit stretch goal and is **cut** rather than carried: it needs a second lifecycle (session setup, download, teardown, all in one process) whose failure modes are not the daemon's, and every hour spent on it is an hour not spent on the live-router sign-off that actually gates 0.1. Nothing about the design forecloses it — the engine already runs headless in tests — so it is a v2 candidate, not a rejection.

## 3. v1 Feature Cut

### Core engine
- BEP 3 peer wire protocol over I2P streams (SAM STREAM), including keep-alives, proper choke/interest state machines.
- BEP 10 extension protocol.
- BEP 9 metadata exchange (magnet links, `maggot`/i2p-style magnet handling as encountered in the wild).
- i2p_pex peer exchange (32-byte destination-hash format per the I2P BT spec).
- HTTP tracker announces over I2P (announce via SAM stream to tracker destination; compact peer response = concatenated 32-byte SHA-256 destination hashes, no port; non-compact destinations used directly).
- Multi-tracker support (BEP 12) — I2P announce URLs only; non-I2P announce URLs in torrent files are ignored, never resolved, never logged as anything but "skipped non-I2P tracker."
- Piece selection: rarest-first with endgame mode. Sequential mode as a per-torrent flag (nice for media).
- Seeding, super-seeding excluded (v2 candidate).
- Fast extension (BEP 6) — in v1 per Q3 decision; i2psnark supports it, improves swarm behavior.
- SHA-1 verification on read-in and download; full recheck command.

### Address handling
- Peer identity = 32-byte destination hash. Full destination cache with b32 naming lookups (via SAM `NAMING LOOKUP`) and negative-result backoff.
- Never emit or accept IP/port peer representations anywhere in the codebase. There is no `IpAddr` in the engine's type vocabulary.

### Persistence
- Single client state directory (XDG-compliant, overridable).
- Per-torrent resume data: bitfield, verified-piece state, file priorities, stats (up/down totals), tracker state. Format: bencoded per Q2 decision; forward-versioned.
- Client destination keys persisted (stable identity across restarts) with an ephemeral-keys option per client or per torrent profile ("transient identity" mode).
- Atomic writes (write-temp-rename) for all state files. Crash at any point must never corrupt resume state — worst case is re-verification.
- **The file format is an API (SQLite doctrine):** the resume/state format gets a written specification and a version field from day one. Policy: newer clove always reads older state; older clove refuses newer state cleanly (clear error, no write, no corruption). Format changes are release-notes headline items.

### Configuration (doas/sshd discipline)
- One config file, one format: flat `key value` lines, hand-parsed, comments with `#`. No TOML/YAML dependency, no nesting.
- **Unknown keys are a fatal error.** A typo'd option must fail startup loudly, never be silently ignored.
- `cloved -C` (or `--check`): parse and validate config, report, exit — for testing before restart.
- **Empty config is the safe, working default.** A fresh install against a default-configured local router (SAM at the standard address) downloads a torrent with zero config lines written. Every key exists to *deviate* from a sane default, not to enable basic function.
- All defaults documented in `clove.conf(5)`, actual-default values stated, not prose-approximated.

### CLI
- Daemon `cloved` + control CLI `clove`, speaking to the local HTTP API over a unix socket (default) or localhost TCP (opt-in).
- Commands: add (file/magnet), remove (with/without data), list, status (per torrent: peers, tunnels, speeds, availability), pause/resume, verify, set file priorities, tracker re-announce, client-level stats.
- Human-friendly default output (aligned tables, progress, rates); `--json` on every read command for scripting.
- Sensible exit codes, shell completion generation. The `clove fetch`-style one-shot download mode (no daemon) was the one stretch goal here and is **cut from v1** — see §2.
- Peer identity on the wire: Azureus-style peer-ID prefix and client name string chosen per Q7 and kept stable thereafter; checked against the informal BEP 20 registry to avoid collisions.

### HTTP API
- Local-only (unix socket default). Token auth even on localhost TCP.
- REST-ish JSON; versioned under `/v1/`. Explicitly not compatible with the Transmission/qBittorrent APIs in v1 (compat shim is a v2 candidate — worth it for *arr-style tooling, not worth the constraint now).

## 4. Architecture

```
+-------------------------------------------------------+
|  CLI (clove)          --unix socket-->   HTTP API      |
+-------------------------------------------------------+
|            Engine (sync threads, per Q5) [rev]          |
|  torrent supervisor / piece picker / choker / storage   |
|  tracker client / pex / destination address book        |
+-------------------------------------------------------+
|                 i2pnet module (THE ONLY                 |
|              NETWORK-TOUCHING CODE, wraps               |
|                      yosemite)                          |
+-------------------------------------------------------+
                    | SAMv3 (localhost)
                    v
           i2pd / Java I2P / emissary
```

- **`i2pnet` module boundary.** All of yosemite is consumed behind our own trait (`I2pDialer`, `I2pListener`, `I2pNamingLookup`, later `I2pDatagram`). Rationale: yosemite is young and small; if it stalls or we outgrow it, we swap the impl without touching the engine. Also gives us a mock implementation for engine tests without a router.
- **Session topology:** one SAM PRIMARY session per client identity, with stream subsession for peer traffic. Tracker announces share the peer session's destination (this is what i2psnark does and what trackers expect — announced identity must match peer identity). Q1 resolved: same subsession, see `DECISIONS.md`.
- **Reconnect discipline:** the SAM control socket, sessions, and forwarded listeners are supervised. Router restart ⇒ exponential-backoff resurrection of the full session tree, torrents transition to a visible "waiting for router" state, no thundering-herd re-announce. This state machine gets designed and tested explicitly — it is Suspect #1 for XD-style flakiness.
- **Concurrency model (Q5, decided):** synchronous, thread-per-peer with blocking I/O — the most auditable and most OpenBSD-like; entirely viable at I2P scale (50–200 peers, high tunnel latency makes per-connection thread cost irrelevant). yosemite ships a first-class `sync` feature. Fallback if a concrete wall is hit: smol via yosemite's `smol` feature. The engine is written against narrow internal traits so this choice stays swappable longer than usual.
- **Storage:** file-backed with preallocation option; mmap explicitly out (predictable memory > speed here). Disk I/O and hashing on dedicated worker threads; bounded queues everywhere (no unbounded channels anywhere in the engine — lint-enforced).

## 5. No-Clearnet Enforcement (defense in depth, three independent layers)

**Layer 1 — by construction (primary):**
- Only the `i2pnet` crate may depend on socket-capable APIs. The engine crates forbid `std::net` and `socket2` via `clippy` `disallowed_types`/`disallowed_methods` config + a CI grep gate over `Cargo.lock` (`ci/check-net-deps.sh`).
- `i2pnet` itself may open exactly one kind of socket: a TCP connection to the configured SAM address, which must be a loopback address or unix socket unless `--i-know-sam-is-remote` (explicit, ugly, documented as dangerous) is set. The opt-in localhost-TCP listener for the local HTTP API is also constructed inside `i2pnet` (loopback-validating helper), so every IP-socket construction site is in one crate.
- No DNS resolution code paths: hostnames are rejected in config except `localhost`; naming is I2P naming only.
- Dependency budget: every new transitive dependency with network capability requires justification in the PR. `cargo deny` config committed in-repo.

**Layer 2 — runtime self-restriction (pledge/unveil doctrine, Linux mechanisms):**
- The daemon restricts *itself* as it passes lifecycle phases, OpenSSH-style. After initialization (config read, data directory opened, SAM connected, control socket bound), it applies:
  - **Landlock**: filesystem access reduced to the data directory (+ log path if separate). Applied only **if available** on the running kernel (probe the Landlock ABI at startup); when unavailable, log one clear line stating so and continue — never fail startup, never assume it, per the no-layer-assumes-another rule.
  - **seccomp**: post-init syscall filter dropping everything no longer needed (exec, ptrace, module/bpf syscalls, new address families). Same graceful-degradation rule.
- Phase hooks are structured so that OpenBSD `pledge(2)`/`unveil(2)` calls slot into the same points if/when clove is ported — the design is "pledge-shaped," the Linux mechanisms are the current backends.
- Destination-level restriction (loopback-only) is not expressible in these mechanisms alone; that remains Layer 1's (by construction) and Layer 3's (sandbox) job. Layer 2's guarantee is about post-init *capabilities*: no exec, no filesystem outside the data dir, no new privilege.

**Layer 3 — OS sandbox (shipped, documented, optional but default in packaging):**
- systemd unit with `IPAddressDeny=any` + `IPAddressAllow=localhost`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, `PrivateDevices`, `ProtectSystem=strict` + `ReadWritePaths` for data dir, `NoNewPrivileges`, syscall filter.
- Alternative: documented network-namespace recipe with a veth to loopback-only, for non-systemd users.
- The client must behave correctly *inside* this sandbox (e.g., never attempt anything the sandbox would kill it for), and correctly *without* it (Layers 1–2 unaffected).

No layer assumes another is present.

## 6. Interoperability & Test Matrix

**Routers (SAM side):**

| Router | Priority | Notes |
|---|---|---|
| i2pd | P0 | Your deployment target; SAM quirks differ from Java |
| emissary | P0 | Young router, young SAM impl — expect to find bugs on both sides; coordinate upstream |
| Java I2P | P1 | Sanity/reference; also what most *remote* peers run behind. When routers disagree, this one is presumed right |

Priority is where to spend attention first, not what to skip: **0.1 requires
the live sign-off on all three.** All three have a podman quadlet in
`contrib/podman/` and publish SAM on different loopback ports, so they run side
by side and `make matrix` sweeps them in one command; results go in
`docs/LIVE-TESTING.md` §6.3.

**Peer clients (swarm side):**

| Client | Priority | Notes |
|---|---|---|
| i2psnark | P0 | De facto reference for the I2P BT dialect; majority of swarm peers |
| BiglyBT (+I2P Helper) | P1 | Second-largest population; different codebase = different bugs |
| XD | P2 | Same-protocol sibling; useful for controlled two-client lab tests |

**Test tiers:**
1. Unit + engine tests against mocked `i2pnet` (no router needed; runs in CI).
2. Loopback integration: two client instances over one local router (operator machine, not CI — see `docs/LIVE-TESTING.md` §2), private torrent, full download both directions, kill-router-mid-transfer chaos tests. Repeated against each router in the table above.
3. Live-network smoke tests (manual/nightly): join a well-seeded public I2P swarm, verify download + PEX peer acquisition + sustained seeding against i2psnark peers.

## 7. Risks & Open Questions

| # | Item | Plan |
|---|---|---|
| R1 | yosemite maturity (v0.7, few users) | Wrap behind `i2pnet` trait; vendor if needed; upstream fixes (author is responsive/active) |
| R2 | i2pd SAM behavior under many concurrent streams on one session (possible root of XD flakiness) | Stress-test harness in Phase D, before engine work relies on assumptions |
| R3 | Datagram2/3 availability in yosemite + routers (gates future UDP announces, DHT) | Not needed for v1; track upstream |
| R4 | i2p_pex flag semantics underspecified ("review libtorrent source") | Conformance testing vs i2psnark; treat i2psnark behavior as normative |
| R5 | Tunnel latency vs choker/timeout tuning (clearnet BT timing assumptions are wrong on I2P) | Make all timeouts config-tunable; benchmark on live swarms; expect several rounds |
| R6 | Naming lookups for large peer sets (b32 resolution latency/failures) | Cache aggressively, cap concurrent lookups, negative caching |

Q1–Q7 from the draft are resolved as reversible defaults — see `DECISIONS.md`.

## 8. Milestones

See `PLAN.md` for the phase-level roadmap (Phases A–G) these map onto.

- **M0 — Bootstrap (this repo state):** workspace, lint/CI no-clearnet gates, decision memos Q1–Q7, dependency allowlist. The concurrency spike is dropped (yosemite `sync` verified); the R2 stress harness moves to Phase D where its findings land.
- **M1 — SAM foundation:** `i2pnet` module complete with supervision/reconnect, naming cache, mock impl; chaos-tested against router restarts.
- **M2 — Engine core:** wire protocol, piece picker, storage, verification; downloads a torrent from a single known peer (lab, two instances).
- **M3 — Swarm citizen:** HTTP tracker announces, i2p_pex, BEP 9 magnets, choker; downloads from and seeds to live i2psnark swarms.
- **M4 — Operable:** `cloved` + HTTP API + `clove` CLI incl. `-C` config check, resume/persistence with format spec, Layer-2 self-restriction (Landlock/seccomp with graceful fallback), man pages, SECURITY.md, packaging (systemd unit, sandbox docs), no-clearnet CI gates in place from M1 onward.
- **M5 — Hardening:** chaos tests, long-running seed soak, timeout tuning on live swarms, interop matrix sign-off.

Each milestone ends in something runnable; M2 onward each produce a demo you can verify on your own router.

Status is not tracked here — it lives in `LIVE-TESTING.md` §6.3, per router and dated, because a milestone is only "met" against a specific router version.

## 9. Engineering Standards

Reference class: OpenBSD base, OpenSSH (non-portable), doas, opentracker, SQLite. Interpreted as concrete, checkable commitments — not vibes:

### Smallness
- **Dependency allowlist, committed in-repo.** Every direct dependency is named in `DEPENDENCIES.md` with a one-paragraph justification and the size of its transitive closure. Target: **≤ ~15 direct dependencies**, transitive closure small enough that `cargo vendor` output is human-reviewable. Additions require the same scrutiny as adding code — because they are adding code.
- Prefer writing 300 focused lines over importing 30,000 general ones: arg parsing, the HTTP/1.1 server for the local API, bencode, and config parsing are all candidates for hand-rolled implementations (bencode especially — it is ~200 lines done carefully, and we need hostile-input control over it anyway).
- No proc-macro-heavy frameworks. serde only if the resume-format decision (Q2) genuinely warrants it; otherwise hand-written encoders (bencode makes this natural). Q2 resolved to bencode, so no serde.
- **LOC as a watched metric.** Not a hard cap, but reported per milestone; unexplained growth is a review topic. opentracker-style pride in what *isn't* there.

### Code quality
- `#![forbid(unsafe_code)]` in every crate except (if ever needed) a single documented exception with rationale — expected count: zero.
- `clippy::pedantic` baseline with a short, committed, individually-justified exception list. rustfmt, no bespoke style.
- Every state machine (choker, peer connection lifecycle, SAM session supervision) documented as an explicit enum with an exhaustive transition table in comments or docs — no implicit states smeared across booleans.
- Errors: small hand-written error enums per module. No `anyhow` in library code; error text written for the operator reading a log at 2 a.m.
- Panics are bugs. `unwrap`/`expect` forbidden by lint outside tests; every `expect` in test-support code states its invariant.

### Documentation
- **Man pages are the primary user documentation**, written and shipped from M4: `cloved(8)`, `clove(1)`, `clove.conf(5)`, and the HTTP API documented in a `clove-api(7)`-style page. Every page has a real EXAMPLES section (OpenBSD standard). README stays short and defers to them.
- **`PROTOCOL.i2p-bt`**: an OpenSSH-`PROTOCOL`-style file recording our precise interpretation of the I2P BitTorrent dialect — every place the upstream spec is vague (R4: i2p_pex flags, magnet conventions, tracker edge cases) gets our observed-behavior notes and the decision we made. This doubles as our interop lab notebook and is a deliverable, not an afterthought.
- rustdoc on every public item (`#![deny(missing_docs)]` on library crates); module-level docs explain *why*, not just *what*.

### Testing (the SQLite lesson)
- Aspiration: **test code volume exceeds source code volume.** Tracked, not enforced by gate.
- **Paranoid debug builds:** release builds stay lean; debug builds are dense with invariant assertions (piece accounting sums, choker state consistency, bitfield/on-disk agreement, session-tree supervision invariants) that run continuously under CI and fuzzing. Bugs should be unable to survive contact with the assertion net even when no test targets them directly.
- Hostile-input torture suites as first-class citizens: malformed/adversarial bencode, evil peer behavior (protocol violations, slow-loris, bad hashes, PEX spam), truncated/corrupted resume files, SAM bridge lying or dying mid-operation. Fuzzing (cargo-fuzz) for every parser from M2.
- Chaos tests (router kill/restart, disk-full, SIGKILL during state write) run in CI, not just manually.
- **Regress runnable by anyone (OpenBSD regress doctrine):** `make test` from a clean checkout runs tier 1 with zero infrastructure; tier 2 requires nothing beyond a local i2pd. If contributors can't run the tests, the tests decay into ours alone, then nobody's.

### Releases & project hygiene
- Few, boring, well-tested releases over frequent ones. Signed tags, reproducible-build attempt documented. No release with a failing interop matrix.
- **Culture of deletion (OpenSSH doctrine):** every feature must justify its continued existence at each release; removals are announced proudly in release notes, not buried. The LOC metric above is allowed — encouraged — to go down.
- **SECURITY.md before release 0.1:** reporting contact, disclosure expectations, and the release-signing key published before anyone needs them — never retrofitted after the first report.

## 10. Out of Scope Forever (unless explicitly re-scoped)

Clearnet peers/trackers, mixed-network mode, outproxy usage of any kind, telemetry of any kind.
