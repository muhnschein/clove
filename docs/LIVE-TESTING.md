# Live-Router Testing Plan — closing M1 and M3

**Status:** Plan (rev 1). No code in this document has landed yet; this is the
agreed approach for paying down the biggest outstanding risk in the project —
that nothing has run against a real I2P router. It operationalizes the [open]
items in `PROTOCOL.i2p-bt` (§2.5 inbound topology, §2.6 R2 concurrency, §5.1
and §5.4 announce quirks) and the exit criteria for M1 and M3 in `SCOPE.md` §8.

## 1. The debt, stated plainly

The engine downloads a torrent between two instances *over the mock network*
(M2, done). Everything above `i2pnet::sam` is exercised. But the SAM backend's
inbound path is unwritten, the R2 stress harness the plan calls for does not
exist, and **not one line has touched a live router.** The three hardest,
reputation-defining unknowns — the ones `SCOPE.md` names as Suspect #1 for
XD-style flakiness — are exactly the parts still marked [open] or [assumed]:

- inbound stream topology (`PROTOCOL.i2p-bt` §2.5),
- one-session concurrency ceiling under load (§2.6, R2),
- supervision surviving a real router restart (`supervisor` is unit-tested
  against a fake factory only).

The longer these sit, the more engine code accretes on top of assumptions that
a live router may not honor. This plan converts them from "unverified" to
"verified, and re-verified on every change" without waiting for the whole
product to exist.

## 2. Why CI cannot do this, and what that forces

The web/CI container has no real I2P connectivity — peer traffic (NTCP2/SSU2 to
arbitrary hosts) does not traverse the sandboxed egress, and I2P has no
same-router shortcut: two destinations on one router still route dest-to-dest
through tunnels built across the live network. So even the "loopback" test
needs a router that genuinely participates. **Live testing runs on an operator
machine, not in CI.** That splits the work into two buckets: what we can write
and verify without a router (Bucket 1, below), and the live sign-off itself
(Bucket 2). Tier-1 CI stays router-free and green throughout.

## 3. The topology decision, settled in code first

The single biggest structural gap is inbound streams. yosemite 0.7.0 offers two
shapes (`PROTOCOL.i2p-bt` §2.5):

- **(a) `Session::accept()`** — one accept per inbound stream, takes `&mut
  self`, blocks. Only one accept can be outstanding at a time across the whole
  session: a hard serialization point for a swarm of inbound peers, and a
  plausible slice of the R2 flakiness.
- **(b) `Session::forward(port)`** — the router forwards each inbound stream as
  a fresh TCP connection to a loopback listener we run; the peer's full base64
  destination arrives on the first line. Inbound concurrency is then bounded by
  a plain `TcpListener` accept loop, not the `&mut self` lock.

**Decision: implement (b).** It matches the "one PRIMARY session + forwarded
listener" topology in `SCOPE.md` §4, it is the R2-safe answer, and the loopback
listener it needs is an allowed IP-socket construction site *inside* `i2pnet`
(the same loopback-validating helper `lib.rs` already promises for the HTTP
API). This is written and typechecked against the yosemite API with no router
present; only its runtime behavior is deferred to Bucket 2. Deriving the
`DestHash` from the forwarded destination reuses `addr` (`PROTOCOL.i2p-bt`
§1.3), which is already tested against RFC 4648 vectors.

When this lands, `PROTOCOL.i2p-bt` §2.5 moves from [open] to [assumed] (written,
pending a live run), and flips to [decided] the first time Bucket 2 confirms a
forwarded peer's dest-hash reconciles with its dialed hash.

## 4. Bucket 1 — buildable now, no router

Ordered by leverage. All of this is verifiable in CI (compiles, unit-tests, the
harness runs and reports "no router" cleanly when SAM is absent).

1. **Inbound path** (`i2pnet::sam`): implement `I2pListener` via `forward` +
   the loopback listener helper, per §3.
2. **R2 stress harness** (`i2pnet` bin target, e.g. `src/bin/sam-stress.rs`):
   bring up one PRIMARY session, `forward` a listener, then dial our own
   destination N times concurrently and pump bytes. Reports the connect-latency
   distribution, failure/timeout counts, and sustained throughput as N climbs
   (16 → 32 → 64 → 128 → 200+). This is the instrument that answers R2. It reads
   the SAM port from `CLOVE_SAM_PORT` and exits with a clear "no router at
   127.0.0.1:<port>" message when unset/unreachable — never hangs.
3. **Gated tier-2 tests**: a two-instance loopback download and a
   kill-router-mid-transfer chaos test, both `#[ignore]`d and keyed on
   `CLOVE_SAM_PORT` so `cargo test` (tier-1) stays router-free and
   `cargo test -- --ignored` (tier-2) runs them against a local router.
4. **The environment** (§5): the podman quadlets and a `make test-live` target
   so tier-2 is a documented one-liner, honoring the SCOPE §9 regress doctrine
   ("tier 2 requires nothing beyond a local i2pd").

## 5. The environment — podman + quadlets, local-first

Target: a Debian-stable operator box running **rootless podman with quadlet**
support (podman ≥ 4.4; Debian 13/trixie ships 5.x). No docker, no compose. The
router is a container managed by a systemd user unit generated from a quadlet.

### 5.1 i2pd quadlet

`contrib/podman/i2pd.container` (a `.container` quadlet dropped in
`~/.config/containers/systemd/` for rootless, or `/etc/containers/systemd/` for
system-wide):

```ini
[Unit]
Description=i2pd router for clove live tests

[Container]
Image=docker.io/purplei2p/i2pd:latest
# SAM on loopback only — matches Layer 1's loopback rule.
PublishPort=127.0.0.1:7656:7656
Volume=clove-i2pd-data:/home/i2pd/data:Z
# Enable the SAM bridge (off by default in the stock image).
Exec=--sam.enabled=true --sam.address=0.0.0.0 --sam.port=7656

[Install]
WantedBy=default.target
```

Notes:
- `sam.address=0.0.0.0` binds SAM *inside the container*; `PublishPort` pins the
  host side to `127.0.0.1`, so the router is never reachable off-box. yosemite
  connects to `127.0.0.1:7656` (it hardcodes the host — `PROTOCOL.i2p-bt` §2.1),
  which this satisfies.
- The named volume persists netDb and the router's own keys, so a **restart**
  (the chaos test) resurrects a warm router instead of a cold reseed — the test
  measures our supervisor, not I2P bootstrap time.
- First bring-up needs a few minutes to reseed and build tunnels before SAM
  sessions will succeed. `make test-live` waits for readiness (§5.3) rather than
  assuming it.

A second quadlet (`i2pd-emissary.container`, `emissary` image on a different
published port) is added when the matrix reaches emissary; the harness and tests
take the port as input, so only `CLOVE_SAM_PORT` changes between routers.

### 5.2 Why one router is enough for the loopback download

The two-instance download test needs two *destinations*, not two routers. Each
clove instance opens its own SAM session against the same i2pd and gets its own
destination; they reach each other dest-to-dest through that router's tunnels —
exactly the "two instances over one local router" of `SCOPE.md` §6, tier 2. One
quadlet covers M1's loopback criterion.

### 5.3 `make test-live`

```
make test-live          # brings the quadlet up if needed, waits for SAM,
                        # runs cargo test -- --ignored, prints the harness summary
make sam-stress N=128   # runs the R2 harness at a given concurrency
```

Readiness is a TCP probe of `127.0.0.1:$CLOVE_SAM_PORT` plus a trial transient
session (SAM answering ≠ tunnels built); the target polls with a timeout and
fails loudly rather than running tests against a half-up router.

## 6. Bucket 2 — the live sign-off (operator machine)

Run against routers in `SCOPE.md` §6 priority order. Each run records its
findings straight into `PROTOCOL.i2p-bt`, flipping [assumed]/[open] entries to
[decided] (or filing a new observation when the router surprises us).

**Router order:** i2pd (P0, deployment target) → emissary (P0, young SAM, expect
bugs on both sides, coordinate upstream) → Java I2P (P1, reference).

### 6.1 M1 exit checklist

- [ ] `sam-stress` completes at 16/32/64/128/200 concurrent streams on one
      session; latency distribution and any failure cliff recorded (R2, §2.6).
- [ ] Inbound: a forwarded peer's derived `DestHash` reconciles with the hash it
      was dialed at (confirms §2.5, §1.3); §2.5 → [decided].
- [ ] Two-instance loopback download completes **both directions** on i2pd.
- [ ] Kill-router-mid-transfer: `systemctl --user restart i2pd` (or `podman
      restart`) during a transfer; torrents show "waiting for router", the
      supervisor rebuilds the session tree on backoff, and the transfer
      **completes** without a restart of clove. Confirms `supervisor` §3.1/§3.2
      against a real router.
- [ ] The above pass on emissary; deltas from i2pd noted. Java I2P sanity pass.

### 6.2 M3 exit checklist

- [ ] Join a well-seeded public i2psnark swarm: full download completes.
- [ ] PEX acquisition observed — peers learned via `i2p_pex` beyond the
      tracker's set (confirms §4.3; resolve the flags [open]).
- [ ] Announce quirks confirmed against a live tracker: the `ip=<base64 dest>`
      value form (§5.1) and `event=started`/numwant behavior (§5.4) → [decided].
- [ ] Sustained seed to i2psnark peers over a multi-hour soak; no session
      starvation under peer load (revisit Q1 only if it appears).
- [ ] Wire identity `-CV0001-` re-checked against observed swarm peer IDs before
      the first announce (Q7 checkpoint — the wire-permanent moment).

## 7. Keeping them closed (anti-decay)

A once-run manual pass rots. Two commitments keep M1/M3 *closed*:

1. **Runnable by anyone** (SCOPE §9 regress doctrine): `make test-live` against
   the quadlet is the whole setup. If a contributor with a local i2pd can run
   tier-2, it stays honest.
2. **Nightly, later**: once the operator box is stable, a `systemd` timer (or a
   self-hosted runner on that box) runs `make test-live` + `sam-stress`
   nightly, so a regression surfaces in a day, not a release. Deferred but
   designed for — nothing in Bucket 1 blocks adding it, since the harness and
   tests are already env-driven and non-interactive.

## 8. Sequencing

Bucket 1 lands in-tree in the order of §4 (inbound path → harness → gated tests
→ quadlets/make), each a normal reviewable change with tier-1 CI green. Bucket 2
is executed by the operator against the quadlet and reported into
`PROTOCOL.i2p-bt`; M1 is signed off when §6.1 is fully checked on i2pd + emissary,
M3 when §6.2 is checked on a live swarm. This maps onto `PLAN.md` Phase D (M1)
and Phase E (M3) — it is the "needs a live router" tail those phases defer.
