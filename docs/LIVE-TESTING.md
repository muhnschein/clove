# Live-Router Testing Plan — closing M1 and M3

**Status:** Rev 2 — Bucket 1 (the router-free half) has landed; Bucket 2 (the
live sign-off) is the operator's to run. This is the agreed approach for paying
down the biggest outstanding risk in the project — that nothing has run against
a real I2P router. It operationalizes the [open] items in `PROTOCOL.i2p-bt`
(§2.5 inbound topology, §2.6 R2 concurrency, §5.1 and §5.4 announce quirks) and
the exit criteria for M1 and M3 in `SCOPE.md` §8.

**What's implemented (Bucket 1):** the inbound SAM path (`i2pnet::sam`
`SamListener`/`ForwardedStream`), the R2 stress harness (`i2pnet` bin
`sam-stress`), the router-gated loopback download test
(`clove-core` `torrent::tests::two_instances_download_over_sam`, `#[ignore]`d),
and the environment (`contrib/podman/i2pd.container` + `Makefile`). Run it:

```
make routers                       # the three routers and where their SAM lands
make router-up ROUTER=i2pd         # start one (rootless podman quadlet)
                                   # …give a cold router a few minutes for tunnels…
make sam-stress N=64               # R2 harness: 64 concurrent streams, one session
make test-live                     # the router-gated loopback download
make matrix                        # …or all three routers in turn

make report ARGS=--up              # …or all of the above, into one file (§5.3)
```

All three routers in `SCOPE.md` §6 — i2pd, Java I2P and emissary — have a
quadlet and run side by side on different SAM ports (§5.1); 0.1 needs the
sign-off on all of them, and §6.3 is the table to record it in.

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

## 4. Bucket 1 — buildable now, no router (landed)

All four items below have shipped; file pointers and the run commands are in the
status block at the top. They are verifiable in CI (compile, unit-tests, and the
harness runs and reports "no router" cleanly when SAM is absent). One deliberate
scope call: item 3's **kill-router-mid-transfer** chaos check stays a *manual*
procedure (§6.1) rather than an automated test — orchestrating a router restart
from inside `cargo test` is brittle, the supervisor's reconnect logic is already
unit-tested against a fake factory, and the live restart is a nightly-runner
candidate (§7). Ordered by leverage:

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

### 5.1 Three routers, three quadlets, one command each

The interop matrix (`SCOPE.md` §6) names three routers, and all three are
first-class here rather than "i2pd plus a TODO". They publish SAM on different
host ports, so **all three can run at once** and the matrix is a loop, not a
teardown dance:

| ROUTER | Implementation | Image | SAM (host) | Console |
|---|---|---|---|---|
| `i2pd` | C++ (deployment target, P0) | `docker.io/purplei2p/i2pd` | 127.0.0.1:**7656** | 127.0.0.1:7070 |
| `java` | Java I2P (the reference, P1) | `docker.io/geti2p/i2p` | 127.0.0.1:**7666** | 127.0.0.1:7657 |
| `emissary` | Rust (young SAM, P0) | built here, no registry image | 127.0.0.1:**7676** | none; read the log |

Inside every container SAM is on 7656; only the published host port differs, so
nothing but `CLOVE_SAM_PORT` changes between routers — which is exactly what
the harness was designed for.

```
make routers                          # what each is called and where SAM lands
make router-build ROUTER=emissary     # only emissary needs building
make router-up    ROUTER=i2pd
make router-sam-enable ROUTER=java    # not needed for i2pd; see below
make test-live    ROUTER=java
make matrix                           # the live tier against all three in turn
```

`make matrix` keeps going after a failure and prints which routers passed —
one router being down must not hide the results of the two that are up.

#### Why two of them need `router-sam-enable`

Not laziness on their part; SAM is an optional bridge and each hides it
differently. The step exists because **the setting lives in a file the router
writes on its first boot**, so there is nothing for a quadlet to configure
until it has booted once.

- **i2pd** — nothing to do. `--sam.enabled=true` on the command line, in the
  quadlet, and it is done.
- **Java I2P** — ships the SAM bridge as a client app with
  `startOnLoad=false`. The image's entrypoint fixes SAM's *bind address* but
  leaves it switched off, so a stock container answers on 7657 and refuses
  7656. `router-sam-enable` flips `startOnLoad` in the persisted
  `clients.config.d` entry and restarts. (Equivalent: tick "SAM application
  bridge" under `/configclients` in the console.)
- **emissary** — writes `router.toml` on first boot with `[sam]` bound to
  loopback *inside* the container, which `PublishPort` cannot forward to.
  `router-sam-enable` sets `host = "0.0.0.0"` under `[sam]` only and restarts.

Both edits are idempotent, so re-run after any config reset, or when unsure.

#### Notes per router

- **i2pd** runs as container-root (`User=0`). Under rootless podman that maps
  to *your* user — the owner of the named volume — so i2pd can write its
  reseed certificates. Left as the image's non-root user it hits
  `certificates: Permission denied`, reseed fails, the router gets no peers,
  and every dial is `CantReachPeer`. That symptom is indistinguishable from a
  clove bug from the outside, which is why the quadlet pins it.
- **Java I2P** needs `IP_ADDR=0.0.0.0` so the entrypoint binds local services
  to all interfaces inside the container; without it the entrypoint derives
  the container's private IP, which `PublishPort` cannot reach either. It also
  wants ~1 GiB — a JVM plus a full netDb is not a 256 MiB proposition — and a
  long `TimeoutStartSec`, since a cold Java router reseeds, unpacks webapps
  and builds tunnels before it is any use.
- **emissary** has no published image, so `contrib/podman/Containerfile.emissary`
  builds one with `cargo install emissary-cli`, upstream's own install path.
  It drops the `ui` feature, which would pull dioxus/desktop and with it GTK
  and WebKit — a headless test router has no use for a desktop window. CA
  certificates *are* installed: reseeding is an HTTPS fetch, and without them
  the router starts, finds no peers, and every dial fails with `CantReachPeer`
  — the same misleading symptom as the i2pd volume problem above.

Every named volume persists netDb and the router's own keys, so a **restart**
(the M1 chaos criterion) resurrects a warm router instead of a cold reseed:
the test measures our supervisor, not I2P bootstrap time.

> **Status of this section:** the image names, config file locations, defaults
> and entrypoint behaviour are read from each project's own sources, and the
> config edits are unit-tested against sample files. What has *not* happened is
> a boot of all three on a real machine — that is the operator's first pass,
> and §6.3 is where its results go. Expect to fix a detail or two here on the
> first run; correct them in place rather than working around them.

### 5.2 Why one router is enough for the loopback download

The two-instance download test needs two *destinations*, not two routers. Each
clove instance opens its own SAM session against the same i2pd and gets its own
destination; they reach each other dest-to-dest through that router's tunnels —
exactly the "two instances over one local router" of `SCOPE.md` §6, tier 2. One
quadlet covers M1's loopback criterion.

### 5.3 One command, one file: `make report`

Running the tiers one at a time and pasting each result somewhere is how a
live session turns into an afternoon. `ci/live-report.sh` runs everything that
applies on the machine and writes a single report:

```
make report              # test whatever routers are already answering
make report ARGS=--up    # bring the routers up first, then test
./ci/live-report.sh --help
```

It runs tier 1 (build, unit tests, smoke, chaos, the no-clearnet gate, man
pages) once, then for each router: context, the live tier, and `sam-stress` at
16/32/64/128. Nothing aborts on failure — a router that is down, a test that
fails, a stress level that collapses are all recorded and stepped past, because
a run that stops at the first problem wastes the other twenty minutes.

Two files come out:

- `live-report-<timestamp>.txt` — everything.
- `…​.txt.short` — environment, the verdict table, and only the sections that
  did **not** pass. Usually a few hundred lines; this is the one to send first.

What it collects beyond raw output is the part that saves a round trip: router
version and image, container restart count, whether SAM and the console answer,
i2pd's netDb/tunnel counts, and the container log for any router that failed.
Those are what separate "clove cannot dial" from "this router knows nobody to
dial through" — a distinction that cost us a debugging session once already.

API tokens are redacted. I2P destinations are not, since they are what makes a
dial traceable and the ones in a test run are transient; `--redact-dests`
removes them if you would rather.

### 5.4 `make test-live`

```
make test-live                    # waits for SAM, runs cargo test -- --ignored
make test-live ROUTER=java        # …against a different router
make sam-stress N=128             # the R2 harness at a given concurrency
make sam-stress N=128 ROUTER=emissary
make matrix                       # the live tier against all three, in turn
```

Readiness is a TCP probe of `127.0.0.1:$CLOVE_SAM_PORT` plus a trial transient
session (SAM answering ≠ tunnels built); the target polls with a timeout and
fails loudly rather than running tests against a half-up router.

These are the individual targets; `make report` (§5.3) drives all of them and
is the better starting point for a session whose results you intend to share.

## 6. Bucket 2 — the live sign-off (operator machine)

Run against routers in `SCOPE.md` §6 priority order. Each run records its
findings straight into `PROTOCOL.i2p-bt`, flipping [assumed]/[open] entries to
[decided] (or filing a new observation when the router surprises us).

**Router order:** i2pd (P0, deployment target) → emissary (P0, young SAM, expect
bugs on both sides, coordinate upstream) → Java I2P (P1, reference).

The order is about where to spend attention, not what to skip: **the checklists
below are per-router and 0.1 needs all three.** A finding on one router is not
a finding until you know whether the other two agree — that is the whole value
of having a reference implementation (Java I2P) in the matrix. When they
disagree, Java I2P is presumed right and the deviation goes in
`PROTOCOL.i2p-bt` naming the router.

Run `make matrix` to sweep all three; the per-router detail below is for when
something fails and you need to know what it was supposed to prove.

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
- [ ] Layer 2 is actually on: `cloved`'s startup line reads
      `sandbox: landlock enforced; seccomp filter installed`, and everything
      above still passes with it enforced. This one belongs here rather than in
      Bucket 1 because container CI does not expose Landlock — the unit test in
      `crates/cloved/src/sandbox.rs` reports it as unenforced and exercises only
      seccomp there, so a bad path set would not surface until an operator with
      a real kernel runs the daemon. `contrib/systemd`'s `ReadWritePaths` and
      the sandbox's read-write set must agree; a mismatch shows up as an I/O
      error on a torrent that worked before.

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

### 6.3 Results matrix

Fill this in as runs happen — one row per router, dated. An empty cell is
"not yet run", which is different from a failure and should not be blurred
into one. `—` means not applicable.

| Check | i2pd | Java I2P | emissary |
|---|---|---|---|
| Router boots, SAM answers | | | |
| `sam-stress` 16/32/64 | | | |
| `sam-stress` 128/200 | | | |
| Inbound dest-hash reconciles (§2.5) | | | |
| Two-instance loopback download, both directions | | | |
| Survives router restart mid-transfer | | | |
| Public i2psnark swarm: full download | | | |
| PEX acquisition observed | | | |
| Announce quirks confirmed (§5.1, §5.4) | | | |
| Multi-hour seed soak | | | |

Alongside each result, note the router **version** — "i2pd 2.58" not "i2pd" —
because a behaviour that changes between releases is exactly the kind of thing
this table exists to catch, and an undated "works" is worth very little a year
from now.

M1 closes when the first six rows are green on all three. M3 closes when the
rest are. A row that fails on one router and passes on the other two is a
finding to file, not a reason to stop: record it, open the issue, and carry on
with the other checks.

## 7. Keeping them closed (anti-decay)

A once-run manual pass rots. Two commitments keep M1/M3 *closed*:

1. **Runnable by anyone** (SCOPE §9 regress doctrine): `make test-live` against
   the quadlet is the whole setup. If a contributor with a local i2pd can run
   tier-2, it stays honest. Tier 1 needs nothing at all: `make test` (units and
   the hostile-input sweep), `make smoke` (the daemon end to end), and
   `make chaos` (SIGKILL storms and failed state writes).
2. **Nightly, later**: once the operator box is stable, a `systemd` timer (or a
   self-hosted runner on that box) runs `make test-live` + `sam-stress`
   nightly, so a regression surfaces in a day, not a release. Deferred but
   designed for — nothing in Bucket 1 blocks adding it, since the harness and
   tests are already env-driven and non-interactive.

## 7a. Troubleshooting (live-run findings)

**`CantReachPeer` on every dial, immediately after `router-up`.** Two causes,
distinguish them at the i2pd console (`http://127.0.0.1:7070`, published by the
quadlet):

- **Cold router (setup problem).** If the console's **Routers** count is ~0–10,
  reseed never completed and the router has no peers to build tunnels through —
  so *nothing* is reachable. The usual cause under rootless podman is a data
  volume i2pd can't write: `podman logs systemd-i2pd` shows
  `certificates/i2pd_certificates: Permission denied`, reseed's TLS certs are
  missing, and reseed fails. The quadlet runs the container as `User=0` (which
  under rootless podman maps to *your* user, the volume's owner) to fix exactly
  this. If you started a router before this fix, wipe the half-initialized
  volume and restart: `make router-down && podman volume rm clove-i2pd-data &&
  make router-up`, then wait for **Routers** to climb into the hundreds.
- **Warmup (normal).** On a healthy router, a *freshly created* destination is
  unreachable for the few seconds it takes to build tunnels and publish its
  leaseSet (`PROTOCOL.i2p-bt` §2.6b). The live test and `sam-stress` retry the
  dial through this window, so a transient `CantReachPeer` that clears on retry
  is expected, not a failure.

**`sam-stress` seems to hang at "driving N concurrent streams".** Dial
*initiation* serializes on the one session (`PROTOCOL.i2p-bt` §2.6a), so on a
cold router N attempts run back-to-back at ~60s each. Confirm reachability with
`make sam-stress N=1` first, then scale N up once the router is warm.

## 8. Sequencing

Bucket 1 lands in-tree in the order of §4 (inbound path → harness → gated tests
→ quadlets/make), each a normal reviewable change with tier-1 CI green. Bucket 2
is executed by the operator against the quadlet and reported into
`PROTOCOL.i2p-bt`; M1 is signed off when §6.1 is fully checked on i2pd + emissary,
M3 when §6.2 is checked on a live swarm. This maps onto `PLAN.md` Phase D (M1)
and Phase E (M3) — it is the "needs a live router" tail those phases defer.
