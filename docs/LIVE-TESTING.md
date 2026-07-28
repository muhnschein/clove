# Live-Router Testing Plan — closing M1 and M3

**Status:** Rev 3 — the order of the tiers is inverted from Rev 2. The
loopback tier is no longer the gate; a live swarm download is. Rev 2's
apparatus is intact and still runs, but it has stopped being the first thing
tried, for the reasons in §0.

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
quadlet and run side by side on different SAM ports (§5.1); 0.1 needs the
sign-off on all of them, and §6.3 is the table to record it in.

## 0. Why the order changed (Rev 3, 2026-07-27)

Three live sessions have been spent and no result from any of them says
anything about clove. Reading the failures back (`PROTOCOL.i2p-bt` §2.6b–e,
§2.8, §2.9, §2.10) they fall into three piles, and none of the piles is a bug
in this codebase:

1. **Harness arithmetic**, twice — a probe that killed its own retry loop
   a third of the way in, then the same nested-budget mistake reintroduced one
   layer up in the shell. Both fixed, both with tests, and neither taught us
   anything about I2P.
2. **The routers were not fit to be measured against.** Every recorded run
   used containers between fifteen minutes and an hour old, firewalled, with
   `LeaseSets 0` and a 33% tunnel build rate. A router in that state cannot
   carry a stream for anyone, clove included.
3. **The loopback topology itself** — and this is the one worth acting on.

Every tier-2 test puts both destinations on **one** router: `router-ready`,
`sam-stress` by default, and `two_instances_download_over_sam`. That asks the
router to resolve a leaseSet for a destination it published seconds ago, on
behalf of a sibling session it is already hosting. It is the most fragile
netDb operation a young router performs; emissary cannot do it at all, with
the router demonstrably healthy at the time (§2.8); and it is the first thing
every run tried and the thing every run died on.

**The mission is easier than the gate.** A swarm peer never does what the
loopback test does. It resolves destinations that have been published for
months, by routers that want to be found. And the download half never needs
*our* leaseSet resolved by anybody: I2P bundles the sender's leaseSet with the
stream's opening message, so the far side replies without a lookup at all
(`PROTOCOL.i2p-bt` §2.11). We built a gate strictly harder than the thing it
was gating, then spent three sessions failing it.

So the question "rewrite the report harness, or go straight to real
torrenting?" has a third answer, which is the one taken here: **the harness is
fine, its ordering was wrong.** `ci/live-report.sh` is well-built — bounded
steps, verdict table, container logs, staleness detection — and most of what
looks like scar tissue in it (unique session ids, derived deadlines,
host-router awareness) was paid for in exactly these failed runs. Rewriting it
would reproduce its design minus the lessons. What it needed was a tier above
it that does not depend on the fragile case, and a reason to run that tier
first.

`ci/live-swarm.sh` is that tier. It also proves considerably more per run: a
completed download exercises the tracker announce, BEP 9, the picker, the
choker, storage, verification and the resume writer against i2psnark — the
client `SCOPE.md` §6 calls normative — over real tunnels. Bytes served prove
the wire in the other direction. A peer that dials *us* proves `STREAM
FORWARD` and that our leaseSet reached the wider netDb, which is the half of
§2.5 no router-free test can reach and a stronger result than the loopback
test was ever going to give.

The loopback tier keeps its place. It is a fine diagnostic and `sam-stress`
is still the only instrument that answers R2. It is just not the gate any
more, and a failure in it no longer skips everything behind it.

**On fitness of subject (pile 2), which no reordering fixes.** A freshly
reseeded container is not a peer anyone wants to route through. Before
spending a session, give the router hours rather than minutes, forward its
transport port (§6.6), and check it is not firewalled. A long-running
host-installed router is usually the better subject than anything this repo
can start for you — `ci/live-report.sh` says so when it finds one, and that
is a recommendation, not an apology.

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

**Run `make swarm TORRENT=…` (§5.5).** Five of these six boxes are milestones
in its table; tick them from the run rather than from impressions.

- [ ] Join a well-seeded public i2psnark swarm: full download completes.
      → `download-complete`, plus the script's closing `verify` pass, which
      re-hashes every byte on disk against the metainfo rather than trusting
      our own counters.
- [ ] PEX acquisition observed — peers learned via `i2p_pex` beyond the
      tracker's set (confirms §4.3; resolve the flags [open]).
      → `pex-acquisition`, i.e. `pex_peers > 0` in `clove show --json`.
- [ ] Announce quirks confirmed against a live tracker: the `ip=<base64 dest>`
      value form (§5.1) and `event=started`/numwant behavior (§5.4) → [decided].
      → `peers-known` proves the announce was *accepted*; the exact value form
      still wants a read of the daemon log next to the tracker's reply, so this
      box is the one the script cannot tick for you.
- [ ] Sustained seed to i2psnark peers over a multi-hour soak; no session
      starvation under peer load (revisit Q1 only if it appears).
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

| Check | i2pd | Java I2P | emissary |
|---|---|---|---|
| Router boots, SAM answers | ok — 2.61.0, 2026-07-28 | ok — 2026-07-28 † | ok — 2026-07-28 † |
| **Public i2psnark swarm: full download** (`download-complete`) | **ok** — 2.61.0, 226s | **ok** — 286s, 2026-07-28 † | blocked before the swarm — see below |
| **PEX acquisition observed** (`pex_peers > 0`) | **ok** — 2.61.0, 68 peers | not seen in 1086s | |
| **Bytes served to a swarm peer** (`bytes-served`) | not yet — see below | not yet — see below | |
| **A remote peer dialed us** (`inbound-peer`, §2.5) | **ok** — 2.61.0, 11 peers | not seen in 1086s | |
| Announce quirks confirmed (§5.1, §5.4) | | | |
| Multi-hour seed soak | | | |
| Survives router restart mid-transfer | | | |
| Cross-router dial (`make cross`) | | | |
| Two-instance loopback download, both directions | | | |
| `sam-stress` 16/32/64 | | | |
| `sam-stress` 128/200 | | | |

† **Version not recorded.** These runs predate `--router-version`; the cells
are honest about what is known rather than guessing. Pass it on any run whose
result goes in this table.

**Two of three routers carry a full download, 2026-07-28.** Same magnet
(`i2pupdate-2.13.0.su3`, 20.4 MiB, 82 pieces), same commit, back to back:

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

**`bytes-served` is still empty on all three, and the evidence now points away
from clove.** The Java run announced twice — `announces_ok 2`, so the
`completed` announce of §5.6 did go out — and still served nothing across 900s
of seeding. With the tracker correctly told there is a new seed, what remains
is the swarm: an I2P router update is close to all-seeds, and seeds do not
request from seeds. Peers decayed toward zero after completion on both routers,
which is what a swarm of seeds does with a peer that has just said it wants
nothing. **Test this row against a torrent with real leechers**; until then it
is untested, not failing.

Also worth watching, not yet a finding: the second i2pd run saw no PEX and no
inbound peer where the first saw 68 and 11. One run each is not a trend, the
second was cut short at 603s, and both numbers depend on the swarm rather than
on us — but if a third run is also empty, that is worth chasing.

**First completed download from a live swarm — i2pd 2.61.0, 2026-07-28.**
`i2pupdate-2.13.0.su3`, 20.4 MiB in 82 pieces, from postman's tracker:

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
loopback, either settles it — and `sam-stress` at 16/32/64. **M3** closes on
the four swarm rows plus the announce quirks and the soak. `sam-stress`
128/200 is R2's ceiling and informs tuning; it is not a release gate.

A row that fails on one router and passes on the other two is a finding to
file, not a reason to stop: record it, open the issue, and carry on with the
other checks. A loopback row that fails while the swarm rows are green is a
finding about the *router*, not about clove — §0 and `PROTOCOL.i2p-bt` §2.8
are why.

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

## 8. Sequencing

Bucket 1 lands in-tree in the order of §4 (inbound path → harness → gated tests
→ quadlets/make), each a normal reviewable change with tier-1 CI green. Bucket 2
is executed by the operator against the quadlet and reported into
`PROTOCOL.i2p-bt`; M1 is signed off when §6.1 is fully checked on i2pd + emissary,
M3 when §6.2 is checked on a live swarm. This maps onto `PLAN.md` Phase D (M1)
and Phase E (M3) — it is the "needs a live router" tail those phases defer.
