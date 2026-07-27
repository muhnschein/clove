# Implementation Plan

Dependency-ordered phases; each is testable without revisiting the previous
one, and the cheapest-to-verify code (pure parsers) lands first. Milestone
mapping to `SCOPE.md` §8: A ≈ parser groundwork for M2, B+D = M1, C = M2,
E = M3, F+G = M4; M5 (hardening) runs across the tail of D–G.

Rules that apply to every phase: workspace lints stay green (`forbid
unsafe`, pedantic, no unwrap/expect outside tests, `std::net` banned outside
`i2pnet`), `ci/check-net-deps.sh` allowlist changes ship in the same commit
as the `DEPENDENCIES.md` entry, and every parser gets hostile-input tests
when it lands — not later.

## Phase A — Pure code, no I/O (`clove-core`)

- `bencode`: hand-rolled codec, ~200 lines, strict (canonical-order checks
  where the spec demands, depth/size limits). First fuzz target.
- `metainfo`: .torrent parsing; multi-tracker (BEP 12) with the I2P-only
  announce filter — non-I2P URLs skipped, never resolved, never logged
  beyond "skipped non-I2P tracker".
- `config`: flat `key value` parser; unknown key = fatal; `-C` validation
  entry point; empty config = working defaults.
- `resume`: versioned bencode state per Q2 (encode/decode + version policy;
  the on-disk atomic-rename writer arrives with storage in Phase C).

Exit: `cargo test` covers round-trips and a malformed-input corpus; fuzz
targets build.

## Phase B — `i2pnet` traits + mock

- Finalize the trait surface sketched at bootstrap (`I2pDialer`,
  `I2pListener`, `I2pNamingLookup`; stream type is blocking `Read + Write`
  per Q5).
- In-memory mock network: process-local destinations, piped streams, fault
  injection (drop session, stall stream, fail lookup) — the substrate for
  every engine test and later chaos tests.

Exit: two mock endpoints exchange bytes; fault injection demonstrably fires.

## Phase C — Engine core against the mock (`clove-core`)

- `wire`: BEP 3 message codec + keep-alives, BEP 10 extension handshake,
  BEP 6 fast extension (Q3). Fuzz target.
- `peer`: connection state machine — explicit enum, exhaustive transition
  table in docs (SCOPE §9).
- `storage`: file-backed pieces, preallocation option, SHA-1 verify on read
  and download, full recheck; dedicated worker threads, bounded queues only;
  atomic write-temp-rename for state files.
- `picker`: rarest-first + endgame; per-torrent sequential flag.
- `choker`: choke/interest state machines; all timeouts config-tunable (R5).

Exit (M2 demo): full torrent download between two engine instances over the
mock network, including mid-transfer fault injection.

## Phase D — yosemite backend + supervision (`i2pnet`)

- yosemite (`sync` feature) implementations of the traits; SAM PRIMARY
  session + stream subsession topology per Q1/Q4.
- Session-tree supervision: explicit reconnect state machine, exponential
  backoff, "waiting for router" surfaced to the engine, no thundering-herd
  re-announce. Suspect #1 for XD flakiness — designed and chaos-tested
  explicitly (router kill/restart mid-transfer).
- **R2 stress harness** (bin target, run manually against local i2pd and
  emissary): many concurrent streams on one session; findings recorded in
  `PROTOCOL.i2p-bt` notes.
- Inbound topology settled as SAM `FORWARD` to a loopback listener, not
  `ACCEPT` (avoids the `&mut self` serialization point) — see `LIVE-TESTING.md`.
- Outbound streams speak SAM directly rather than through yosemite, after the
  library's shared session controller proved to poison itself on any
  unexpected stream reply (`PROTOCOL.i2p-bt` §2.12). Inbound and outbound are
  now the same socket type, with the same timeouts and the same close-on-drop.

Exit (M1): a stream between two instances over a real router — a swarm peer
dialing us counts, and is the stronger result; survives router restart. Live
sign-off checklist and the podman-quadlet test environment are in
`LIVE-TESTING.md` §6.1, and §0 there explains why the loopback form of this
criterion stopped being the one to chase first.

## Phase E — Swarm citizen (`clove-core`)

- `tracker`: HTTP announces over I2P streams (shared minimal HTTP/1.1
  client per Q6); compact response = concatenated 32-byte hashes; announce
  state machine with backoff.
- `pex`: i2p_pex; i2psnark behavior is normative (R4) — conformance notes
  go straight into `PROTOCOL.i2p-bt`.
- `magnet`: BEP 9 metadata exchange, i2p-style magnet handling.
- Naming cache with negative backoff (R6) wired into peer acquisition.

Exit (M3 demo): download from and sustained seed to a live i2psnark swarm —
`make swarm TORRENT=…`, whose milestone table ticks most of the checklist.
Live sign-off checklist in `LIVE-TESTING.md` §6.2.

## Phase F — Operable (`cloved`, `clove`)

- `cloved`: config load, engine host, hand-rolled HTTP/1.1 API (Q6) under
  `/v1/` on a unix socket; localhost-TCP opt-in via `i2pnet`'s
  loopback-validating listener helper; token auth even on TCP.
- `clove`: hand-rolled arg parsing; add/remove/list/status/pause/resume/
  verify/priorities/re-announce/stats; aligned-table output, `--json` on
  every read command; sensible exit codes; shell completions.
- `STATE-FORMAT.md` written alongside the resume writer's final form.
- `clove sequential` and `clove announce` close the last two §3 CLI
  commitments. `clove fetch` (daemon-less one-shot) is **cut from v1**, 2026-07;
  see SCOPE §2 for the reasoning. It is a v2 candidate, not a rejection.

Exit (M4 demo): daily-drivable daemon+CLI against a local router.

## Phase G — Hardening & packaging

- Layer 2: Landlock + seccomp post-init self-restriction, ABI-probed,
  graceful single-log-line fallback; "pledge-shaped" phase hooks. *Landed:*
  `crates/cloved/src/sandbox.rs`, one `enter_post_init` hook called after the
  control socket binds and before the first thread starts.
- Man pages: `cloved(8)`, `clove(1)`, `clove.conf(5)`, `clove-api(7)` — each
  with real EXAMPLES. `PROTOCOL.i2p-bt` consolidated. `SECURITY.md`.
- systemd unit (Layer 3) + network-namespace recipe; chaos suite in CI
  (SIGKILL during state write, disk-full); long seed soak; interop matrix
  sign-off (i2pd/emissary/Java I2P × i2psnark/BiglyBT/XD).

Exit: 0.1 release criteria per SCOPE §9 (signed tag, green interop matrix).

## Dependency schedule

| Phase | New direct deps |
|---|---|
| A | `sha1` (info-hash at parse time) |
| B–C | none |
| D | `yosemite` (features = `sync`, allowlisted in `ci/check-net-deps.sh`) + `sha2` (dest-hash) |
| E | none |
| F | `getrandom` (API token randomness) |
| G | `landlock`, `seccompiler`, `libc` (constants for the seccomp filter) |

Total: 7 of the ≤15 budget (`DEPENDENCIES.md` is authoritative).
