# Live-router testing — the tiers, the environment, and the interop matrix

**Status:** M1 and M3 are met on i2pd and Java I2P — the two routers 0.1 gates
on. emissary is tracked but no longer gating (`DECISIONS.md` S1).
The results, per router and dated, are the matrix in §6.3 — that table is the
point of this document, and everything else here exists to fill it in.

**Start here:**

```
make swarm TORRENT='magnet:?xt=urn:btih:…'   # the real binaries, a real swarm
```

That is the whole tier-3 entry point: build, run `cloved` against your router,
download a torrent you name from live i2psnark peers, seed it back, and print
a milestone table saying exactly how far it got. Everything else on this page
is diagnosis for when it does not get far enough.

The rest, when you want it:

```
make routers                       # the three routers and where their SAM lands
make router-up ROUTER=i2pd         # start one (rootless podman quadlet)
                                   # …give a cold router a few minutes for tunnels…
make sam-stress N=64               # R2 harness: 64 concurrent streams, one session
make cross                         # every ordered pair of routers, one dial each
make test-live                     # the router-gated loopback download
make matrix                        # …or all three routers in turn

make report ARGS="--up --swarm magnet:?…"   # all of it, into one file (§5.3)
```

All three routers in `SCOPE.md` §6 — i2pd, Java I2P and emissary — have a
quadlet and run side by side on different SAM ports (§5.1). **0.1 needs the
sign-off on i2pd and Java I2P**; emissary is recorded but not gating, per
`DECISIONS.md` S1. §6.3 is the table all of it goes in.

## 0. Why the swarm tier comes first

Every tier below the swarm one puts **both** destinations on a single router and
asks it to resolve a leaseSet for a destination it published seconds ago, on
behalf of a sibling session it is already hosting. That is the most fragile
netDb operation a young router performs; emissary cannot do it at all with the
router demonstrably healthy (`PROTOCOL.i2p-bt` §2.8). For three sessions it was
also the *gate*, so nothing behind it ever ran.

**The mission is easier than the gate.** A swarm peer resolves destinations
published for months by routers that want to be found, and the download half
never needs *our* leaseSet resolved by anyone — I2P bundles the sender's
leaseSet with the stream's opening message, so the far side replies without a
lookup (§2.11). The gate was strictly harder than the thing it gated.

So `make swarm` runs first, and a loopback failure alongside swarm success is a
finding about the router. The loopback tier keeps its place as a diagnostic, and
`sam-stress` is still the only instrument that answers R2 — it is simply not the
gate, and a failure in it no longer skips everything behind it.

One thing no reordering fixes: **a freshly reseeded container is not a peer
anyone wants to route through.** Give a router hours rather than minutes, and
forward its transport port (§6.6). A long-running host-installed router is
usually the better subject than anything this repo can start for you.

## 1. What is proven, and what is not

The results matrix is §6.3; this is the summary it rolls up to.

**Proven live, on i2pd and Java I2P:** a full download from a public i2psnark
swarm — twice each, at 20.4 MiB and at 286 MiB — peers acquired over PEX beyond
the tracker's set, and payload served back to real leechers. On i2pd, also a
remote peer dialing our destination: `STREAM FORWARD` and our leaseSet reaching
the wider netDb, the half of `PROTOCOL.i2p-bt` §2.5 that no router-free test
can reach. M1 and M3 are met on those two routers.

**Also proven, on i2pd:** `sam-stress` to 200 concurrent streams on one
session, twice over — the R2 question, answered in the negative and closed in
`SCOPE.md` §7. The dial path does not degrade with concurrency (§2.6e).

**Not proven:** the multi-hour seed soak, and a router restart mid-transfer.
Those are what stands between here and the 0.1 interop sign-off.

**Not reachable in this environment:** the inbound half against a containerized
router (§3.1), and with it every tier that needs a listener of ours — the
loopback download, `sam-stress`, and the cross pairs whose listener is a
container. These need a host-installed router or clove inside the router's
namespace; they are blocked on the test rig, not on clove.

**No longer gating:** emissary end to end. Its 0.4.0 fails naming in two
independent ways (§6.3), it is experimental upstream, and 0.1 now signs off on
i2pd and Java I2P — `DECISIONS.md` S1, with the condition for putting it back.

Worth keeping in view: every defect that has mattered was found *here*, not in
CI, and the unit suite was green through all of them. That is the argument for
this document existing, and the reason the tiers are cheap to run.


## 2. Why CI cannot do this, and what that forces

The web/CI container has no real I2P connectivity — peer traffic (NTCP2/SSU2 to
arbitrary hosts) does not traverse the sandboxed egress, and I2P has no
same-router shortcut: two destinations on one router still route dest-to-dest
through tunnels built across the live network. So even the "loopback" test
needs a router that genuinely participates. **Live testing runs on an operator
machine, not in CI.** That splits the work into two buckets: what we can write
and verify without a router (Bucket 1, below), and the live sign-off itself
(Bucket 2). Tier-1 CI stays router-free and green throughout.


## 3. Inbound topology: `STREAM FORWARD`, not `STREAM ACCEPT`

yosemite 0.7.0 offered two shapes for inbound streams (`PROTOCOL.i2p-bt` §2.5):
`accept()` takes `&mut self` and blocks, so only one accept can be outstanding
across the whole session — a hard serialization point for a swarm of inbound
peers, and a plausible slice of the R2 flakiness. `forward(port)` has the router
push each inbound stream to a loopback listener we run, with the peer's full
base64 destination on the first line, so inbound concurrency is a plain accept
loop.

**(b) was implemented**, and confirmed live: a forwarded peer's derived
dest-hash reconciles with its dialed hash, and remote peers have dialed us on
i2pd. The loopback listener is an allowed IP-socket construction site inside
`i2pnet`, and the `DestHash` derivation reuses `addr`, already tested against
RFC 4648 vectors.

Only on i2pd, and §3.1 is why: the host-installed i2pd is the only router in
this environment that *can* forward to us. An earlier revision of this section
claimed both i2pd and Java I2P, which its own §6.3 row contradicted.

### 3.1 The inbound half needs a shared network namespace

`SamListener::forward` binds its loopback listener to `127.0.0.1` and issues:

```
STREAM FORWARD ID=<nick> PORT=<port> SILENT=false
```

with **no `HOST=`**. Per SAMv3 the router then connects back to the address it
sees our SAM control connection arriving from. That is our loopback only when
the router shares our network namespace. **A router in a podman container does
not.** It dials peers correctly, accepts the inbound stream, and then cannot
hand it over. emissary says so in its own log, once per attempt:

```
emissary::streaming: inbound stream accepted local=… remote=… payload_len=0
emissary::streaming: failed to open tcp stream to forwarded listener
```

This is a deployment constraint, not a passing bug, and it is deliberate:
binding `0.0.0.0` and naming a routable `HOST=` would put a listening socket on
a real interface, which is exactly what `SCOPE.md` §5 Layer 1 constrains. The
supported shapes are a **host-installed router**, or **clove inside the
router's namespace**:

```
podman run --network=container:systemd-i2p-java …
```

What it costs, and where it shows up:

- **`make swarm` is unaffected in the main.** A download needs only the
  outbound half — dial peers, leech, serve on connections we opened. The
  inbound half is a bonus, and its absence shows up as one blank
  `inbound-peer` milestone. This is why the swarm tier passes against
  containerized routers that every tier below it fails.
- **`router-ready`, `test-live`, `sam-stress` and the cross pairs need both
  halves.** Each dials a destination whose listener is ours, so a containerized
  router fails all of them — with symptoms (`failed to fill whole buffer`,
  streams that never finish) that look like router or clove faults and are
  neither. `ci/live-report.sh` now detects this and skips those steps with the
  reason instead of filing red rows; `sam-stress` says so directly when not one
  forwarded stream arrives.
- **Java I2P's empty `inbound-peer` row in §6.3 is this**, not the
  `EXT_PORT`/firewalled note in §6.6. Its container cannot receive a forward.
  Judge that row again from a Java router that shares clove's namespace.


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
   `ci/sam-stress-sweep.sh` (`make sam-sweep`) drives the ladder — every level,
   repeated, into one file with a summary table — because the answer is a
   curve and a curve needs more than one point per level. Each run also emits
   one machine-readable `sam-stress-result` line, which is what the sweep reads
   rather than parsing the human table.
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
| `java` | Java I2P (the reference, P0) | `docker.io/geti2p/i2p` | 127.0.0.1:**7666** | 127.0.0.1:7657 |
| `emissary` | Rust (young SAM, P2, not gating) | built here, no registry image | 127.0.0.1:**7676** | none; read the log |

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
make sam-sweep                    # the whole R2 ladder, ×3, into one file
make matrix                       # the live tier against all three, in turn
```

`make sam-sweep` is the one to reach for when the question is R2 rather than
"does this router work at all". It runs `sam-stress` at 1/16/32/64/128/200,
three times each, keeps every run's output and prints one table at the end:

```
make sam-sweep
make sam-sweep ROUTER=java SWEEP_ARGS='--levels "16 64" --repeats 5'
./ci/sam-stress-sweep.sh --help
```

It defaults to a 4 KiB echo rather than `sam-stress`'s 64 KiB, deliberately.
R2 asks whether the *dial path* degrades under concurrency; at 64 KiB a busy
router spent a median of 78 seconds per echo, so the ladder measured tunnel
bandwidth instead, took hours, and buried the answer. `--payload` puts it back
when throughput is the question.

Read the `dialed` column against N first. If that holds as N climbs, the
session's dial path is fine whatever the echo columns do — and on a
same-router run the echo columns are mostly the router carrying every byte
twice (§2.6c). Nothing aborts on a failing level: the rungs above a bad one
are the question, so the ladder runs to the top regardless.

Readiness is a TCP probe of `127.0.0.1:$CLOVE_SAM_PORT` plus a trial transient
session (SAM answering ≠ tunnels built); the target polls with a timeout and
fails loudly rather than running tests against a half-up router.

These are the individual targets; `make report` (§5.3) drives all of them and
is the better starting point for a session whose results you intend to share.

### 5.5 `make swarm` — the tier that proves the product

```
make swarm TORRENT='magnet:?xt=urn:btih:…'
make swarm TORRENT=~/thing.torrent ROUTER=java SWARM_ARGS='--deadline 7200'
./ci/live-swarm.sh --help
```

It builds the release binaries, writes a throwaway config pointed at your
router, starts `cloved`, adds the torrent through `clove`, and samples the
daemon until the download completes or the budget runs out — then keeps
seeding, because "it downloaded" and "it can serve" are separate claims. No
router-side setup beyond a router that works: no quadlet, no second
destination, no loopback.

**It reports milestones, not a verdict.** Each is stamped with the second it
was reached, and an unreached one is left blank on purpose:

| Milestone | What it settles |
|---|---|
| `daemon-up` | the daemon runs and answers |
| `router-connected` | SAM session up, `STREAM FORWARD` accepted (§2.7, §2.5) |
| `torrent-added` | the add path, magnet or file |
| `metadata` | BEP 9 metadata fetched from a live peer |
| `peers-known` | the tracker answered with destinations (§5.1, §5.4) |
| `peer-connected` | we dialed a real swarm peer and handshaked (§1.2) |
| `first-bytes` / `first-piece` | the wire, storage and SHA-1 verification over real tunnels |
| `download-complete` | **M3, first row** |
| `pex-acquisition` | **M3**: peers learned via `i2p_pex` (§4.3) |
| `bytes-served` | **M3**: we served a swarm peer |
| `inbound-peer` | a remote peer dialed *us* — `STREAM FORWARD` end to end, and our leaseSet resolvable from the wider netDb (§2.5) |

A run that reaches `peer-connected` and stops has told you something specific:
peers are reachable and the transfer is not moving, which is clove's problem
to answer. A run that never reaches `peers-known` has told you the announce is
where to look. That is the whole point of not collapsing this into pass/fail.

The last three milestones are readable because the engine now counts them:
`Torrent::pex_learned` and `Torrent::inbound_peers`, surfaced by the daemon as
`pex_peers` and `inbound_peers` in `clove show --json`. Before that, "PEX
acquisition observed" was a checklist line with no way to observe it.

**You supply the torrent, deliberately.** A magnet committed to this repo
would be dead within months, and every failure after that would be blamed on
clove — the exact confusion this tier exists to end. Take a well-seeded one
from a tracker index or i2psnark's own list, and prefer something small enough
to finish inside an hour: this is a correctness test, not a benchmark.

## 6. Bucket 2 — the live sign-off (operator machine)

Run against routers in `SCOPE.md` §6 priority order. Each run records its
findings straight into `PROTOCOL.i2p-bt`, flipping [assumed]/[open] entries to
[decided] (or filing a new observation when the router surprises us).

**Router order:** i2pd (P0, deployment target) → Java I2P (P0, reference) →
emissary (P2, tracked, expect bugs on both sides, coordinate upstream).

The order is about where to spend attention, not what to skip: **the checklists
below are per-router, and 0.1 needs both P0 routers.** A finding on one router
is not a finding until you know whether the other agrees — that is the whole
value of having a reference implementation (Java I2P) in the matrix. When they
disagree, Java I2P is presumed right and the deviation goes in
`PROTOCOL.i2p-bt` naming the router. emissary's results are recorded on the
same terms and gate nothing (`DECISIONS.md` S1).

Run `make matrix` to sweep all three; the per-router detail below is for when
something fails and you need to know what it was supposed to prove.

### 6.1 M1 exit checklist

- [ ] `sam-stress` completes at 16/32/64/128/200 concurrent streams on one
      session; latency distribution and any failure cliff recorded (R2, §2.6).
      **Run it as `make sam-sweep`** (§5.4) — repeats per level, one file, one
      table. The first hand-run ladder produced a success rate of 3/16, 30/32,
      11/64, 31/128, which is noise wearing a cliff's clothes, and no single
      run per level can tell those apart.
      **Needs a router in clove's own network namespace** — §3.1. Against the
      containerized routers here it fails on the listening side and measures
      nothing; `ci/live-report.sh` now skips it there rather than filing a red
      row.
      **Answered, i2pd 2.61.0, 2026-07-28** (`PROTOCOL.i2p-bt` §2.6e): two
      sweeps, 31 runs with results, **every one dialled N of N** — ~1,580
      streams from N=1 to N=200, no dial failures. Connect p50 ranges overlap
      completely across levels and do not order by N (the fastest run of either
      sweep was an N=128 one), and p99 sits within ~10 ms of p50 across up to
      200 dials in a single run. One sweep ran against a machine with another
      I2P client at full load and is indistinguishable from the quiet one.
      The dial path does not degrade with concurrency.
      What remains open is not R2 but §2.6f: i2pd's SAM bridge stopped
      answering at the top of both ladders, which is why 200 has one run of six
      rather than six.
- [x] Inbound: a forwarded peer's derived `DestHash` reconciles with the hash it
      was dialed at (confirms §2.5, §1.3); §2.5 → [decided]. **i2pd, 2026-07-28**
      — `inbound-peer` on three runs (11, 2 and 2 remote peers). Only on the
      host-installed router, and §3.1 is why.
- [ ] Two-instance loopback download completes **both directions** on i2pd.
      Same §3.1 constraint: both instances need the router to forward to them.
- [ ] Kill-router-mid-transfer: `systemctl --user restart i2pd` (or `podman
      restart`) during a transfer; torrents show "waiting for router", the
      supervisor rebuilds the session tree on backoff, and the transfer
      **completes** without a restart of clove. Confirms `supervisor` §3.1/§3.2
      against a real router.
- [x] **Java I2P pass: done, 2026-07-28** — two full swarm downloads, faster to
      a session than either other router, PEX and bytes served. Its one blank
      is `inbound-peer`, which §3.1 accounts for and which no amount of clove
      work would change while the router sits in a container.
      *(emissary is no longer on this list: `DECISIONS.md` S1. Its deltas are
      still worth recording in §6.3 when a run happens.)*
- [x] Layer 2 is actually on: `cloved`'s startup line reads
      `sandbox: landlock enforced; seccomp filter installed`, and everything
      above still passes with it enforced. **Confirmed on every live run** —
      `ci/live-swarm.sh` captures that line into each report for this reason. This one belongs here rather than in
      Bucket 1 because container CI does not expose Landlock — the unit test in
      `crates/cloved/src/sandbox.rs` reports it as unenforced and exercises only
      seccomp there, so a bad path set would not surface until an operator with
      a real kernel runs the daemon. `contrib/systemd`'s `ReadWritePaths` and
      the sandbox's read-write set must agree; a mismatch shows up as an I/O
      error on a torrent that worked before.

### 6.2 M3 exit checklist

**Run `make swarm TORRENT=…` (§5.5).** Five of these six boxes are milestones
in its table; tick them from the run rather than from impressions.

- [x] Join a well-seeded public i2psnark swarm: full download completes.
      **i2pd and Java I2P, 2026-07-28** — 20.4 MiB in 226s and 286s, 82/82
      pieces re-verified.
      → `download-complete`, plus the script's closing `verify` pass, which
      re-hashes every byte on disk against the metainfo rather than trusting
      our own counters.
- [x] PEX acquisition observed — peers learned via `i2p_pex` beyond the
      tracker's set (confirms §4.3; the flags stay [open]).
      **Both routers, 2026-07-28.**
      → `pex-acquisition`, i.e. `pex_peers > 0` in `clove show --json`.
- [ ] Announce quirks confirmed against a live tracker: the `ip=<base64 dest>`
      value form (§5.1) and `event=started`/numwant behavior (§5.4) → [decided].
      → `peers-known` proves the announce was *accepted*; the exact value form
      still wants a read of the daemon log next to the tracker's reply, so this
      box is the one the script cannot tick for you.
- [ ] Sustained seed to i2psnark peers over a multi-hour soak; no session
      starvation under peer load (revisit Q1 only if it appears). *Serving
      itself is confirmed* — 71 MiB and 69 MiB back on 2026-07-28, on a
      torrent with real leechers — but not yet over hours.
      → `make swarm … SWARM_ARGS='--seed-for 21600'` after a completed
      download; `bytes-served` and `inbound-peer` are the signals that peers
      are actually taking data from us rather than us merely being up.
- [ ] Wire identity `-CV0001-` re-checked against observed swarm peer IDs before
      the first announce (Q7 checkpoint — the wire-permanent moment).

### 6.3 Results matrix

Fill this in as runs happen — one row per router, dated. An empty cell is
"not yet run", which is different from a failure and should not be blurred
into one. `—` means not applicable.

Rows are ordered by what they prove, not by which tier they came from. The
swarm rows are first because they are the ones a user would recognise as "the
client works", and because they no longer depend on the loopback rows passing.

| Check | i2pd | Java I2P | emissary (not gating) |
|---|---|---|---|
| Router boots, SAM answers | ok — 2.61.0, 2026-07-28 | ok — 2026-07-28 ‡ | ok — 0.4.0, 2026-07-28 |
| **Public i2psnark swarm: full download** (`download-complete`) | **ok** — 2.61.0, 226s and 4450s | **ok** — 286s and 4345s, 2026-07-28 ‡ | blocked before the swarm — see below |
| **PEX acquisition observed** (`pex_peers > 0`) | **ok** — 2.61.0, 68 peers | **ok** — 2 peers at 76s, 2026-07-28 ‡ | |
| **Bytes served to a swarm peer** (`bytes-served`) | **ok** — 2.61.0, 95.0 MiB | **ok** — 54.0 MiB, 2026-07-28 ‡ | |
| **A remote peer dialed us** (`inbound-peer`, §2.5) | **ok** — 2.61.0, 11 and 2 peers | not reachable in this environment — §3.1 | |
| Announce quirks confirmed (§5.1, §5.4) | | | |
| Multi-hour seed soak | | | |
| Survives router restart mid-transfer | | | |
| Cross-router dial (`make cross`) | unfinished at 240s — 2026-07-28 | n/a as listener — §3.1 | n/a as listener — §3.1 |
| Two-instance loopback download, both directions | | n/a — §3.1 | n/a — §3.1 |
| `sam-stress` 16/32/64 | **ok** — 2.61.0, 2026-07-28, 12/12 runs dialled N of N | n/a — §3.1 | n/a — §3.1 |
| `sam-stress` 128/200 | **128 ok** — 6/6 runs dialled 128 of 128. 200: one run of six, the other five blocked by the SAM bridge (§2.6f) | n/a — §3.1 | n/a — §3.1 |

† **Version not recorded at all.** The 20.4 MiB runs further down predate
`--router-version`. Left as they are: an undated, unversioned "works" is worth
little, and rewriting it to look better would be worth less.

‡ **Version approximate.** The Java rows come from a container whose image tag
is `:latest` (digest `b06b16a5fc4ea3d1853`, log line `Starting I2P
2.12.0-17-rc`); the cells stay honest about the difference between a digest and
a version. `ci/router-version.sh` now also asks a *host-installed* binary, so
i2pd names itself without `--router-version`.

**`n/a — §3.1`** is not a failure and not "not yet run". Those rows need the
inbound half, and every containerized router in this environment is
structurally unable to provide it. They become runnable against a
host-installed router, or clove inside the router's namespace.

**A 286 MiB torrent, three routers, one sweep — 2026-07-28.** Same magnet
(286.1 MiB, 1145 pieces, a swarm with active leechers), same commit, all three started
within 80 seconds of each other:

| | router-connected | metadata | complete | served | inbound | PEX |
|---|---|---|---|---|---|---|
| i2pd 2.61.0 (host) | 30s | 45s | **4450s** | **95.0 MiB** | **2** | 1 |
| Java I2P ‡ (container) | 16s | 31s | **4345s** | **54.0 MiB** | 0 — §3.1 | 2 |
| emissary 0.4.0 (container) | 15s | — | — | — | — | — |

Both completions re-verified 1145/1145 pieces against the metainfo.

**Do not read the two completion times against each other.** Three routers and
three daemons shared one box and one swarm for the whole run: ~66 KiB/s each,
against ~140 KiB/s for the same client alone on the 20.4 MiB run below. The
number this table wants from a sweep is *whether* it completed; throughput
needs a run that is not competing with itself.

**`bytes-served` closed, and the swarm was the culprit all along.** That row
was empty on all three routers, and the note below argued the cause was the
torrent — an I2P router update is close to all-seeds, and seeds do not request
from seeds — recording it as *untested, not failing* and asking for a re-run
against real leechers. This is that re-run: bytes moved at **105s on i2pd and
46s on Java**, both long before completion, i.e. ordinary reciprocation while
still leeching. The diagnosis held.

**Java I2P's PEX row closed with it** (2 peers at 76s, against "not seen in
1086s" previously), which also answers the note below about the second i2pd run
seeing neither PEX nor an inbound peer: a third run saw both. Swarm-dependent,
as suspected, not a trend.

**Transfer overhead at scale:** 286.2 MiB moved for 286.1 MiB of torrent —
about 0.04%, against 2.5% on the 20.4 MiB run and 45% before the request
deadline (§4.7). The picker holds at fourteen times the size.

**What the seeding windows did *not* show.** Upload flatlined at completion on
both routers — i2pd sat at 95.0 MiB for the whole 890s window, Java climbed to
54.0 MiB in the first 90s and then stopped — while attached peers decayed from
7 toward 0–3 and `known_peers` stayed at 13–14. postman's interval is half an
hour, so at most one announce falls inside a 15-minute window and no fresh
peers arrive; that is the innocent reading and probably the right one. It is
still worth settling before the multi-hour soak, because a soak against a swarm
that drains in fifteen minutes measures nothing: check whether the seeding path
re-dials from the known/PEX set as connections drop.

**Two of three routers carry a full download, 2026-07-28.** Same magnet
(20.4 MiB, 82 pieces, a near-all-seeds swarm), same commit, back to back:

| | router-connected | metadata | download-complete |
|---|---|---|---|
| i2pd 2.61.0 | 30s | 75s | **226s** |
| Java I2P † | 15s | 45s | **286s** |
| emissary † | 61s | — | — |

**Java I2P completed a download for the first time.** Every previous attempt
died at the session, and §2.13 says why: Java's `SAMv3Handler` pings the client
and ends the session with `SESSION_ERROR "PONG timeout"` when nobody answers,
and nothing in clove read the control connection at all. It answers now, and
the router that had never worked was the *fastest* of the three to a session
(15s) and reached metadata in 45s.

**emissary never reached the swarm, and not because of clove.** Its SAM session
came up fine at 61s; the tracker's *name* could not be resolved:

```
resolving tracker tracker2.postman.i2p: protocol error: `router error: KeyNotFound`
… negative-cached after 2 failed lookup(s); not asking the router again for 29s
```

That is §5.5 exactly — an address-book name on a router whose subscription has
not been fetched — and the diagnostic behaved as designed: it named the stage,
the host, the router's own word, and how long the negative cache would hold.
Fix it router-side (subscribe the address book) or sidestep it with a `b32`
tracker URL; see the troubleshooting section above. Re-run before reading this
row as anything about clove.

*Updated 2026-07-28: an operator ran `make router-addressbook` before a sweep
and hit the identical `KeyNotFound`, because the address book is only half of
it.* The same sweep showed emissary 0.4.0 failing `NAMING LOOKUP` for its **own
freshly created b32 destinations** — no address book involved, since a b32 is a
hash resolved through the netDb — which `sam-stress` reported as *"neither b32
resolves, including the dialer's own"*. Two independent naming failures, plus
the same-router leaseSet failure of §2.8, none of them clove's. That is the
evidence behind `DECISIONS.md` S1, which moves emissary out of the 0.1 gate
until it reaches a stable release; its column stays, and stays recorded.

**`bytes-served` is still empty on all three, and the evidence now points away
from clove.** *(Superseded 2026-07-28 by the 286 MiB sweep above, which closed
this row on both routers. Kept because the reasoning was the useful part and it
turned out to be right.)* The Java run announced twice — `announces_ok 2`, so
the `completed` announce of §5.6 did go out — and still served nothing across
900s of seeding. With the tracker correctly told there is a new seed, what
remains is the swarm: an I2P router update is close to all-seeds, and seeds do
not request from seeds. Peers decayed toward zero after completion on both
routers, which is what a swarm of seeds does with a peer that has just said it
wants nothing. **Test this row against a torrent with real leechers**; until
then it is untested, not failing.

Also worth watching, not yet a finding: the second i2pd run saw no PEX and no
inbound peer where the first saw 68 and 11. One run each is not a trend, the
second was cut short at 603s, and both numbers depend on the swarm rather than
on us — but if a third run is also empty, that is worth chasing. *(Answered
2026-07-28: the third run saw both. Swarm-dependent, not a trend.)*

**First completed download from a live swarm — i2pd 2.61.0, 2026-07-28.**
20.4 MiB in 82 pieces, from postman's tracker:

```
router-connected    30s
metadata            76s   (BEP 9 from a live peer)
peer-connected      76s
first-piece         91s
download-complete  227s   20.9 MiB transferred for 20.4 MiB of torrent
pex-acquisition    317s
inbound-peer       317s
```

~140 KiB/s sustained, 38–39 peers, and 82/82 pieces re-verified against the
metainfo after the fact. The 2.5% transfer overhead is the honest cost of
duplicate blocks near the end; it was 45% before the request deadline (§4.7).

`bytes-served` did **not** happen, across a 900-second seeding window with
7–40 peers attached. Two causes, and it is worth separating them because only
one is ours:

  - **clove's**, now fixed: no `completed` announce went out. The run shows
    `announces_ok 1` — one announce, sent at 76s as a leecher holding nothing,
    and postman's interval is half an hour. The tracker therefore spent the
    entire seeding window handing our destination to peers as a leecher, and
    no peer had a reason to ask us for anything. See §5.6.
  - **the swarm's**, and not a defect: an I2P router update is close to
    all-seeds, and seeds do not request from seeds. Peers fell from 38 to 7
    shortly after completion, which is what a swarm of seeds correctly does
    with a peer that has just told them it wants nothing.

Re-run against a torrent with real leechers before reading anything into a
second empty result. Until then this row is *untested*, not *failing*.

Alongside each result, note the router **version** — "i2pd 2.58" not "i2pd" —
because a behaviour that changes between releases is exactly the kind of thing
this table exists to catch, and an undated "works" is worth very little a year
from now.

**M1** closes on: router boots, a remote peer dialed us (the inbound half of
§2.5, whose dest-hash reconciliation is what accepting the peer *is*),
survives a router restart mid-transfer, a dial succeeds — cross-router or
loopback, either settles it — and `sam-stress` at 16/32/64, **which is done on
i2pd** (§2.6e). **M3** closes on the four swarm rows plus the announce quirks
and the soak. `sam-stress` 128/200 is R2's ceiling and informs tuning; it is
not a release gate, and 128 is done on i2pd too.

A row that fails on one router and passes on the others is a finding to file,
not a reason to stop: record it, open the issue, and carry on with the other
checks. A loopback row that fails while the swarm rows are green is a finding
about the *router* — or about the test rig, if the router is containerized
(§3.1) — not about clove. §0 and `PROTOCOL.i2p-bt` §2.8 are why.

### 6.4 Before spending 500 seconds on *tier 2*: `make router-ready`

```
make router-ready ROUTER=emissary
```

**Scope, as of Rev 3:** this gates the loopback tier and nothing else. It is a
same-router dial, so it inherits everything §0 says about that topology — a
router can fail this and still carry a swarm perfectly well, and `make swarm`
does not wait for it. Read a failure here as "do not spend twenty minutes on
tier 2 yet", never as "clove is broken".

Two sessions on the router, one dial between them, bounded by
`READY_DEADLINE` (240s). It is the smallest thing that exercises what the
loopback tier needs, and its verdicts are the ones worth acting on:

- **succeeded 1/1** — the router can carry a stream between its own
  destinations. Run the real tier.
- **unfinished 1** — the router accepted the sessions but never completed the
  dial. Almost always a netDb too thin to resolve a leaseSet: wait, do not
  debug clove. Check again in an hour; a first-boot router can need several.
- **failed 1** with an error — that error is the finding. Record it.

Run this after any router restart and before a matrix sweep. A tier-2 run that
fails after eight minutes of retries tells you nothing this does not tell you
in four.

### 6.5 Router readiness has four gates, not one

Learned the expensive way (`PROTOCOL.i2p-bt` §2.10, §2.11). Each is necessary
for what sits above it and none of the earlier ones implies the later:

1. **The SAM port answers.** `make router-wait`. Proves a process is
   listening. Java I2P's bridge binds early in startup, so this passes while
   the router behind it is still coming up.
2. **It speaks SAM.** clove opens the session with a real `HELLO VERSION`
   exchange of its own, bounded and length-capped (§2.7). Proves it is a SAM
   bridge and not something else on that port. Still says nothing about
   tunnels — and since §2.13 the `SESSION CREATE` that follows on the same
   socket is bounded too, so a bridge that passes this and then stalls is an
   error rather than a hang.
3. **It can reach the network.** `make swarm` getting as far as
   `peer-connected`: a tracker resolved, a swarm peer dialed, a handshake
   exchanged. This is the gate that matters for the product, and it is the
   cheapest real one — everything above it in the milestone table is clove's
   own behaviour rather than the router's.
4. **It can resolve its own fresh leaseSets.** `make router-ready`. Only
   tier 2 needs this, and it is the strictest of the four: emissary 0.4.0
   fails it with a demonstrably healthy router (§2.8).

A router that passes 1–3 and fails 4 is a perfectly good router for clove; it
just cannot host the loopback test. A router that fails 3 is warming up,
firewalled, or has no peers — and in none of those cases is clove the subject.

**Gate 0, which is about the rig rather than the router:** can it reach a
listener of ours at all? A containerized router cannot (§3.1), and it fails
gate 4 for that reason alone — no matter how healthy it is. Check this first,
because it is the one gate a better router will never pass. `ci/live-report.sh`
checks it for you and skips gates 4 and up with the reason.

### 6.6 Do not test a firewalled router if you can avoid it

Both routers in the first three-router run reported themselves firewalled:

```
emissary: router is firewalled, publishing U  ipv4_status=Firewalled
java:     *** EXT_PORT is unset.
          *** I2P router will resolve to a "Firewalled" state
```

A firewalled router still works — it builds tunnels and clients function —
but it participates as a second-class peer: fewer usable peers, slower tunnel
builds, worse netDb reachability. That is a poor baseline to judge clove
against, and it muddies every result with "was that us or the router?".

The quadlets now publish each router's transport port (i2pd 12346, Java
12345, emissary 8888 — NTCP2/SSU2, deliberately *not* loopback-bound, unlike
SAM). If the machine is behind NAT, forward those ports too. If you cannot,
note it in the §6.3 results table: a firewalled run is still worth recording,
it just is not a clean sign-off.

## 7. Keeping them closed (anti-decay)

A once-run manual pass rots. Two commitments keep M1/M3 *closed*:

1. **Runnable by anyone** (SCOPE §9 regress doctrine): `make swarm
   TORRENT=…` against any working router is the whole setup — no quadlet, no
   second destination, nothing this repo has to install. That is a lower bar
   than tier 2 ever was, and it is the one that matters: a contributor who
   already runs i2pd can check clove works before reading a line of this page.
   Tier 1 needs nothing at all: `make test` (units and the hostile-input
   sweep), `make smoke` (the daemon end to end), and `make chaos` (SIGKILL
   storms and failed state writes).
2. **Nightly, later**: once the operator box is stable, a `systemd` timer (or a
   self-hosted runner on that box) runs `make swarm` against a long-lived
   torrent plus `make test-live` and `sam-stress`, so a regression surfaces in
   a day, not a release. Deferred but
   designed for — nothing in Bucket 1 blocks adding it, since the harness and
   tests are already env-driven and non-interactive.

## 7a. Troubleshooting (live-run findings)

**`state` flapping between `downloading` and `waiting-for-router` every
60–120s, with peers and known-peers dropping to 0.** That was the session
wedge, and it is fixed (`PROTOCOL.i2p-bt` §2.12): dials no longer go through
the library whose shared state machine caused it. Each flap cost a new
destination, every known peer, and a fresh announce — which is why a run could
be downloading and still unusably slow.

If you see the flap now it is a genuine lost router: the control connection
died, and the daemon says exactly that.

**The socket count against 127.0.0.1:7656 climbs steadily during a run**
(`ss -tn 'dport = :7656' | wc -l`). Two causes, both fixed: a goodbye announce
fired on every teardown including wedge-caused ones, and — the larger one —
every dialled peer that went silent parked a thread holding a stream clove
could not close (§2.7a). Outbound streams are ordinary sockets now, with
timeouts and close-on-drop. Worth re-measuring rather than assuming: if it
still climbs, there is a third source.

**A `.torrent` sits at `downloading` with 0 peers and says nothing.** Fixed:
the running announcer used to discard its errors — the magnet path had been
repaired for exactly this and the path a real torrent takes had not. `clove
show` now reports `announces_ok`, `announces_failed` and
`last_announce_error`, and the daemon logs each failure with the destination
the tracker's host resolved to.

**Every announce fails and no peers are ever learned.** Read the reason, which
now carries the response: `tracker: response is not bencode; it begins "…"`.
If it begins with a hex number and a CRLF, that is chunked framing on a client
too old to decode it (fixed; `PROTOCOL.i2p-bt` §5.1a). If it begins `<html`,
the tracker returned an error page and the status line is the thing to read.
If it says `<empty body>`, the stream closed before the tracker said anything.

**A magnet that sits in `fetching-metadata` and never moves.** Found by the
first live swarm run, 2026-07-27: nine minutes at `fetching-metadata` with no
output of any kind. The cause was not the fetch failing — it was the fetch
being *silent*. `try_fetch_round` discarded the error from every stage, so
"this router has never heard of the tracker's hostname", "the tracker returned
no peers" and "thirty peers were dialed and none served the metadata" were one
indistinguishable state, and the daemon's log said nothing at all.

Each stage now names itself, in the daemon's log and in `clove list`:

```
clove list --json | grep -o '"last_error":"[^"]*"'
grep 'metadata fetch' <data-dir>/cloved.log | tail -20
```

The usual cause is the first stage. A magnet's `tr=` is typically a hostname
(`tracker2.postman.i2p`), which the router resolves from its **address book** —
and a router that has not yet fetched its subscriptions has never heard of it.
Worse, a failed lookup is negative-cached with a doubling hold up to 30 minutes
(`i2pnet::naming`, R6), so the symptom is not a stream of failing lookups but
silence: retries get *rarer*. The error text now says how long the hold has
left, so a log with no lookups in it is distinguishable from a router that is
not being asked.

Fixes, cheapest first: check the router knows the host (i2pd's console has an
address book page); wait for the subscription fetch on a young router; or
sidestep naming altogether by using a tracker URL in `b32` form, which resolves
through the netDb rather than the address book.

**On emissary, do this instead** — it starts with an empty address book and
fetches its subscription over I2P, which takes longer than a live run:

```
make router-addressbook          # resolves with i2pd, writes into emissary
make swarm TORRENT='magnet:…' ROUTER=emissary
```

Note the `make swarm` form: the torrent is `TORRENT=`, the router is `ROUTER=`,
and anything else goes through `SWARM_ARGS='…'`. `make swarm --router emissary`
is not it — make takes those as goals, not arguments.

`contrib/podman/seed-addressbook.sh` asks a router that already knows the name
(i2pd by default) over SAM and writes the answer into **both** of emissary's
address books, because it has two and they serve different callers (§5.5a):

| file | holds | read by |
|---|---|---|
| `addressbook/addresses` | `hostname=<b32 label>` | `resolve_base32` — `STREAM CONNECT` by hostname |
| `addressbook/destinations/<host>.txt` | the base64 destination | `resolve_base64` — `NAMING LOOKUP` |

**clove's path is the second one.** It does a `NAMING LOOKUP` and dials the
result, so `destinations/` is what decides whether it works; writing only
`addresses` leaves the entry present, correct, and invisible. The script also
restarts the router, which `addresses` needs (it is read once, at startup).

Nothing is hardcoded: a destination baked into a script would be a lie the day
postman rotates keys, and there is no way to check it offline. `--dry-run`
resolves and prints without touching anything, which is also how the script is
tested (§7b).

### 7b. Checking the b32 derivation without a router

`seed-addressbook.sh` computes a b32 label in shell — `base32(SHA-256(dest))`,
lowercase and unpadded — and it has to agree exactly with
`i2pnet::addr::to_b32`, because an address book entry that resolves to the
*wrong* destination is worse than one that is missing. Both are checkable
offline against a fake SAM bridge:

```
# a bridge that answers HELLO, then NAMING REPLY with a known destination
./contrib/podman/seed-addressbook.sh --dry-run --sam-port <fake-port> some.i2p
```

Two things this caught that review did not, and that are worth keeping in mind
if the script is ever changed:

  - **`RESULT=OK` must be matched against the `NAMING REPLY` line, not the
    whole conversation.** The handshake before it says `HELLO REPLY
    RESULT=OK`, so a check against everything received calls every lookup a
    success — `KEY_NOT_FOUND` included — and writes a hash of the string
    `KEY_NOT_FOUND` into the address book as if it were the tracker.
  - **Validate the destination, not the label.** SHA-256 is always 32 bytes,
    so the label is always 52 base32 characters; a hash of the word "nonsense"
    is exactly as well-formed as a hash of postman's destination. The gate is
    that the decoded destination is at least 387 bytes.

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

## 8. What is left

Bucket 1 has landed in full. What stands between here and the 0.1 interop
sign-off, cheapest first:

1. **A router that shares clove's network namespace** — a host-installed Java
   I2P, or clove run inside a container's netns (§3.1). Nothing below this is
   measurable without one, and it is configuration rather than code.
2. **A router restart mid-transfer** (§6.1), which is the one M1 box no swarm
   run can tick for you.
3. **The multi-hour seed soak** (§6.2) — worth settling the post-completion
   peer decay noted in §6.3 first, or the soak will measure an empty swarm.
4. **§2.6f, if it recurs**: i2pd's SAM bridge stopped answering at the top of
   both stress ladders. Watch `systemctl --user status i2pd` across the next
   sweep — whether the router exited or only its bridge stopped decides
   between an upstream bug report and a local limit. Not a release gate, and
   not something a torrent client's usage pattern provokes (§2.6f), but it is
   the one open question the sweeps raised.

*`sam-stress` at 16/32/64/128/200 has left this list: R2 is answered (§2.6e),
and the ladder is a regression instrument now rather than an open question.*

emissary is off this list (`DECISIONS.md` S1). It is welcome back when it
reaches a stable release: `make router-addressbook`, then `make swarm
TORRENT=… ROUTER=emissary`, and if it carries a download it returns to the
gate.

Findings from any of these go into `PROTOCOL.i2p-bt`, results into §6.3 with
the router version.
