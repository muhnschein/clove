# Phase H — Daily-drivable: many torrents, and a view worth watching

**Status:** Complete except `add --to` (§8). H0, H4, H1, H5, H2, H3, H6 and the TUI (§9) have all landed.
This pinned the design before the wiring, the way `PHASE-F.md` did, so the
architecture was argued on paper where arguing is cheap. Maps to no new milestone: this is what M4's *"a CLI pleasant
enough for daily use"* (`SCOPE.md` §1) turns out to mean once you run more than
one torrent at a time.

The reference clients are the ones an operator would otherwise be running:
rtorrent and TransmissionBT. Neither is a feature list to copy. What they
supply is a **question**: of everything they do, what does clove not do, and of
that, what actually stops someone using clove all day?

Nothing here relaxes `SCOPE.md` §9. The dependency budget is the binding
constraint on every item, and §10 states what each costs. The answer is zero
new crates, including the TUI.

---

## 1. Multi-torrent already exists. The budget does not.

`cloved`'s registry is a `BTreeMap<[u8; 20], Hosted>` and has been since
`PHASE-F.md` §7 3b. Add ten torrents today and ten torrents are hosted, listed,
paused, resumed, verified and persisted independently. The plural is not what
is missing.

What is missing is that **each of those ten torrents helps itself to whatever
it wants**. Every live torrent independently starts:

| Per live torrent | Count | Source |
|---|---|---|
| dial-sweep thread | 1 | `Swarm::dial_only` |
| tracker announce thread | 1 | `Announcer::spawn` |
| maintenance thread (keep-alives, choke rounds) | 1 | `Torrent::spawn_maintenance` |
| in-flight dial threads | up to 8 | `SwarmConfig::dial_concurrency` |
| attached peers × 2 threads (reader + writer) | up to 100 | `SwarmConfig::max_peers` = 50 |

There is no global cap on any line of that table. `max_peers` is per torrent,
and the inbound demux enforces it per torrent as well
(`swarm.rs`, `InboundDemux::max_peers`). Ten torrents is therefore up to **500
concurrent I2P streams on one SAM session and ~1030 threads**; forty torrents
is 2000 streams, and the arithmetic does not care that no operator would
choose it deliberately — a watch directory or a bulk add would.

This is not a "clove gets slow" problem. It is a **session** problem, and we
have already measured where the edge is:

- `SCOPE.md` R2 is closed *negative* up to **200 concurrent streams** on i2pd
  2.61.0 (`PROTOCOL.i2p-bt` §2.6e): connect latency was uncorrelated with
  concurrency across 30 runs. That is the ceiling we have evidence for, and it
  is the number below which we should stay until someone measures higher.
- `PROTOCOL.i2p-bt` §2.12 records a session wedging under a stream failure on
  an unexpected path. Every extra concurrent stream is another draw on the
  thing already known to be the fragile part.

One wedged session takes **every** torrent down, not the one that overspent.
That asymmetry is the argument for doing the budget first and the comforts
second: a queue that keeps you under the limit is worth more than any number
of new subcommands, and no subcommand is pleasant on a daemon that has
detached from its router.

So the order below is not a wish list in priority order. H1–H3 are the ones
that make "many torrents" mean anything; H4–H6 are what make it pleasant; §9
is the view.

---

## 2. H0 — A torrent name is not a control sequence — **landed**

`metainfo::check_component` rejects the empty string, `.`, `..`, `/`, `\` and
NUL. It does not reject `ESC`. A torrent whose `info.name` is
`"\x1b[2J\x1b[1;31mhello"` parses, is hosted, becomes a directory name, and is
printed **verbatim** by `clove list`, `clove show` and `clove watch`, which
render `name` straight into the table. (`clove watch` was removed by
`DECISIONS.md` S3; the sanitising this section installed did not move — it is
in the renderers, which is why it survived the removal untouched.)

The bytes are attacker-supplied: they arrive in a `.torrent` from anywhere, or
in a magnet's `dn=`. `SECURITY.md` already puts *"path traversal from torrent
file names"* in scope; this is the same input reaching a different interpreter.
The API is **not** affected — `json::write_string` escapes everything below
`0x20` as `\u00XX`, so `--json` and any future frontend already get inert text.
The hole is exactly the human renderers in `clove(1)`, plus `cloved`'s own
`eprintln!` lines that name a torrent.

Fix, and it is the smallest item in this document:

- A `display` helper in `clove`: replace C0 and C1 control characters and
  `DEL` with `.`, and truncate to the column width with a trailing `…`.
  Applied to every attacker-controlled string that reaches a rendered cell —
  name, file paths, tracker URLs, `last_error`.
- The same for the daemon's log lines, since a log is read in a terminal too.
- Keep the *JSON* faithful. The wire format is data; sanitising is a property
  of rendering, and doing it in the daemon would lie to `--json` consumers
  about what the torrent is actually called.

This is a prerequisite for §9 rather than a nicety: a full-screen view renders
strictly more attacker-controlled text than a one-shot table does, and doing
that on top of an unsanitised renderer is how a torrent name repaints your
screen.

**Cost:** ~40 lines, plus tests. No new dependency.

---

## 3. H1 — One budget, shared — **landed**

A `PeerBudget`: an atomic count of attached peers across the whole daemon, and
a slot guard that releases on drop. The dial sweep takes slots before it dials;
the demux takes one before it accepts; a dropped peer returns one. Both callers
already have the shape for it — the sweep computes
`max_peers.saturating_sub(connected.len())` today, and the demux already
refuses past its cap.

- **`peer_limit`** (config, default **200**) — the global ceiling, set to the
  concurrency R2 actually measured rather than to a round number. It is a
  *stream* budget as much as a peer budget, which is the reason it is global:
  the SAM session is the shared resource, not the CPU.
- **`torrent_peer_limit`** (config, default 50) — the existing `max_peers`,
  now explicitly a sub-cap. A single torrent still cannot take the whole
  budget.
- **Fairness — corrected on contact with the code.** This section originally
  called for sweeping the registry's torrents in rotation, so a torrent whose
  info-hash sorts first could not monopolise the budget. There is no such
  sweep to rotate: each live torrent owns its *own* `Swarm` thread and its own
  `sweep_interval` timer, started whenever that torrent went live, so the
  order they reach the budget in is already arbitrary and carries no bias
  toward a low info-hash. What actually bounds one torrent is
  `torrent_peer_limit`, which is why it stays. *Trigger for revisiting:* at
  the defaults, four torrents can hold the whole 200 while a fifth gets
  nothing until peers churn. If a live run shows a new add starved for more
  than an announce interval, the fix is a fair share — `peer_limit` divided
  by the live-torrent count as a floor — not a rotation.
- **No preemption.** A torrent that holds slots keeps them until its peers
  drop on their own (idle timeout, choke churn, completion). Preemption means
  deciding *which* healthy connection to sever, which needs a policy we have
  no evidence for. Same trigger as above.

Both keys are tunables in the R5 sense — the defaults are the numbers we can
defend today, and live measurement is expected to move them.

**Cost:** ~150 lines in `clove-core::swarm` plus the two call sites and the two
config keys. No new dependency. Mock-provable: N torrents, a budget of K,
assert the attached total never exceeds K and that every torrent eventually
gets slots.

**As built.** `clove-core::budget` — a `PeerBudget` whose `claim()` is one
compare-exchange returning a `PeerSlot` guard. The slot is held *by the peer's
own entry in its torrent's table*, so it returns on every path that removes a
peer — idle timeout, protocol violation, pause, session teardown, or a reader
thread that panicked — without any of those paths knowing the budget exists.

The claim sits in `Torrent`'s attach path and is the only authoritative check.
The dial sweep and the inbound demux read `available()` first, but purely to
avoid spending a `dial_timeout` reaching for room that is not there: that read
is advisory and documented as such, because two torrents can reach the claim
believing the same slot is free and exactly one of them can be right.

A `Torrent` built the old way gets `PeerBudget::unlimited()`, so every existing
test and any standalone use behaves as before; the daemon uses
`Torrent::with_budget` and one budget owned by the registry — which outlives
any single session, since a router restart returns every slot on its own.

---

## 4. H2 — The queue — **landed**

The budget stops the daemon hurting itself. The queue is what an operator
actually wants: twenty torrents added, three downloading, the rest waiting
their turn without costing a tunnel.

- **`max_active_downloads`** (default 3) and **`max_active_seeds`** (default
  5). Torrents past the limit enter a new state, **`queued`**: hosted,
  persisted, listed, and costing nothing — no engine, no announcer, no
  streams.
- **Promotion** happens on the events that free a slot: a download completes
  (it moves to the seed budget, freeing a download slot), a torrent is paused,
  removed, or hits a stop rule (§5). The registry already has a periodic tick;
  this is a pass in it, not a new thread.
- **Order** is the queue order (§7), and `queued` is distinct from `paused`:
  paused is an operator decision and survives everything, queued is the
  daemon's own bookkeeping. Conflating them is how Transmission's queue
  confuses people, and the resume file has room for both.
- **`clove start <ref>`** forces a torrent to the front — the one manual
  override, because "download *this* one now" is the request a queue must be
  able to answer.

Note the interaction worth stating: the queue and the budget solve overlapping
problems at different grains, and both are wanted. The queue keeps the
*steady state* sane; the budget is what holds when twelve torrents are all
inside their announce interval at once.

**Cost:** ~200 lines in `registry.rs`, one new state string, two config keys.
**No resume field**: queue position is derived from H5's `added` rather than
stored, so there is nothing to migrate and nothing that can disagree with the
add order.

**As built.** One `rebalance()` is the only thing that decides what runs;
every path that could change the answer — add, pause, resume, remove,
completion noticed by the periodic tick, a session coming up — calls it rather
than starting or stopping a torrent itself. It is a pure function of the
torrents' states and add order, so a rebalance that changes nothing stops
nothing, and a torrent already running keeps its peers.

`paused` and `queued` became one `Wanted` enum with an exhaustive transition
table, because clippy's `struct_excessive_bools` fired and it was right: two
booleans for three mutually exclusive answers has a fourth combination that
means nothing, which is exactly the "implicit states smeared across booleans"
`SCOPE.md` §9 forbids. `scanning` stays a separate flag — a hash pass runs over
a *paused* torrent, which is what `clove verify` does, so it is orthogonal
rather than a fourth variant.

**The cost worth naming:** queue position is add order, so resuming a paused
torrent returns it to *its* place, which can displace a newer torrent promoted
in its absence. That torrent keeps its data and loses its peers and an
announce. The alternative — letting an incumbent keep its slot — makes position
depend on unobservable history and lets the first-added torrent starve behind
newer ones. `clove start` exists for when the order is not what you want.

---

## 5. H3 — Knowing when to stop — **landed**

Without this, "daily drivable" means manually pausing finished torrents
forever, and an unattended `cloved` seeds everything it has ever seen until the
disk or the router says otherwise.

- **`seed_ratio`** (default `0.0` = unlimited) — stop seeding at
  uploaded/downloaded ≥ ratio. Per-torrent override via
  `clove seed-ratio <ref> <n>`.
- **`seed_idle_minutes`** (default 0 = never) — stop after this long with no
  peer attached.
- Stopping means entering `paused` with a visible reason (`clove show` says
  *"paused: seed ratio 2.0 reached"*), never silently. A torrent that stops
  for a reason nobody can read is a bug report about a torrent that "stopped
  working".

The counters exist already: `Hosted` carries lifetime `uploaded`/`downloaded`
and the registry persists them. This is a comparison in the periodic tick.

Deliberately **not** included: scheduling by time of day. It is the one
rtorrent/Transmission feature that is genuinely better served by the thing
already on the operator's machine — `systemd` timers against `clove pause
--all` / `clove resume --all`, which H4 makes a one-liner.

**Cost:** ~120 lines, two config keys, one subcommand, and resume **v5** for
the per-torrent override and the stop reason.

**As built.** The reason is carried *by* the stopped state rather than beside
it — `Wanted::Paused(Why)`, where `Why` is `Operator`, `SeedRatio` or
`SeedIdle` — so a stopped torrent cannot exist without an answer to the
question its operator will ask. It is persisted, because that question is
usually asked days later, and surfaces as `paused_because` in `clove show`.

Ratios are stored and parsed in **thousandths**, never as floats. The config
format is exact `key value` text and the resume file is bencode, which has
integers and no floats at all; a ratio that round-trips through an `f64` can
come back as 1.499 where the operator wrote 1.5. One hand-rolled parser serves
both `clove.conf` and the API, so `seed_ratio 1.5` and a `1.5` over the socket
are the same number.

Two rules that keep it from firing when it should not: a torrent that
downloaded nothing has no ratio and is never stopped by one (it was added
complete), and the idle clock runs only while a torrent is *complete and has
nobody attached*, resetting the moment a peer arrives — so an incomplete
download in a slow swarm is never stopped for being quiet. Raising a limit on a
torrent the daemon stopped starts it again; an operator's own pause is not
undone by it.

---

## 6. H4 — Naming a torrent without typing forty characters — **landed**

This is the cheapest item here and the one that most changes how the client
feels. Every per-torrent command today takes a full 40-character lowercase hex
info-hash — `parse_info_hash` rejects anything of a different length, rejects
uppercase, and there is no other way to name a torrent:

```
clove pause 3f2a91c0d4e5b6a7889900112233445566778899
```

With one torrent you paste it once. With twenty it is unusable, and no amount
of queueing or budgeting fixes that.

- **Unique-prefix resolution.** `clove pause 3f2a`. This is git's doctrine and
  git is in the reference class §9 names. An ambiguous prefix is an **error**
  that lists the candidates with their names — never a guess.
- **Resolution happens in the daemon**, not the CLI. The CLI is a thin
  one-request client; resolving client-side would mean a `GET /v1/torrents`
  before every action, two round trips and a window in which the answer
  changes. `GET/POST/PUT/DELETE /v1/torrents/{ref}` accepts a prefix of ≥4 hex
  characters; `409` with a `candidates` array on ambiguity, `404` on no match.
  A full 40-character hash keeps its exact-match fast path, so nothing that
  works today changes behaviour.
- **Multiple operands and `--all`.** `clove pause 3f2a 9b1c`, `clove resume
  --all`. Each operand is one request — the CLI loops, the daemon stays
  simple — and the exit code is the worst of them.
- **Not** name-substring matching. It reads as friendlier and it is: right up
  to the day `clove remove --data ubuntu` matches two torrents and picks the
  wrong one. Prefixes are unambiguous or they are an error.

**Cost:** ~80 lines split between `registry.rs` (the resolver) and
`clove/src/main.rs` (operand loops). No new dependency.

**What landing it turned up.** `--all` expands against the listing, and the
listing includes magnets still fetching their metadata — which have no engine,
so `clove resume --all` failed for the whole run because one entry was never
resumable. Two fixes, both narrower than they first looked:

- Those operations answered **`404 no such torrent`** about an entry
  `clove list` was showing at that moment. That is simply false, and it sends
  an operator hunting for a torrent they can see. They now answer `400` naming
  the actual state. Removing one still works, because removing is the one
  thing a half-added magnet can do.
- `--all` means *every torrent that has become one*: the CLI filters on the
  `fetching-metadata` state, which is the existing marker for "this is an add
  in progress, not yet a torrent". One rule in one place, rather than a list of
  which commands tolerate which states.

---

## 7. H5 — Rates, and an order that means something — **landed**

Two gaps that only appear with more than one torrent, and that §9 needs before
it is worth building:

- **Rates.** `Hosted` reports lifetime `uploaded`/`downloaded` totals and
  nothing else. Neither `clove list` nor `clove watch` can answer *"is
  anything moving right now"* — the first question anybody asks a torrent
  client. Compute up/down rates **in the daemon** as an EWMA over the existing
  refresh tick, expose `up_rate`/`down_rate` in the list and detail JSON, and
  add a daemon-total pair to `GET /v1/status`. Daemon-side rather than
  client-side deltas: one implementation, and every `--json` consumer gets it
  without reimplementing differencing.
- **Order.** The registry is a `BTreeMap` keyed by info-hash, so `clove list`
  is ordered by a hash — which is to say, shuffled, and reshuffled the moment
  you add one. Add an `added` timestamp to the resume format (a `v3` field)
  and order by it. That also gives the queue (§4) its natural order, and gives
  `clove list` a stable one: the row for a torrent does not move under the
  cursor while you are looking at it, which §9 depends on.
- A `#` column carrying the list position, so `clove pause 3` works the way
  rtorrent users expect. Positions are **display indices resolved by the CLI
  against the listing it just fetched**, never a daemon-side identity — an
  index that means something different after an add is a footgun, and the
  daemon must not hold one.

**Cost:** ~120 lines. One resume-format version bump — **v4**, not v3: the
format was already at v3 (`sequential`) when this was written. It gets its
`STATE-FORMAT.md` entry and is forward-compatible in the usual direction, a
file without `added` reading as 0 and sorting first.

**Two things the tests found.** `added` is stored in **milliseconds**, not
seconds: at one-second resolution every torrent of a bulk add shares a
timestamp, falls through to the info-hash tie-break, and comes out in hash
order — the exact shuffle the field exists to remove, and what a watch
directory (H6) would hit every time. And a listing position is **one to three
digits**, which makes positions and references disjoint by construction, since
the shortest accepted prefix is four characters. Without that bound an
all-digit info-hash — `0000…0000` is legal — was read as position zero.

---

## 8. H6 — Adding torrents the way people actually add them — **landed, less `--to`**

- **`add --paused`**, **`add --sequential`**, **`add --to <subdir>`.** The
  first two are trivially small and remove a two-step dance. The third has a
  constraint worth naming: after init, `cloved` confines itself with Landlock
  to the data directory (`SCOPE.md` §5, Layer 2), so an arbitrary destination
  path *cannot work* — the kernel refuses it, correctly, and no amount of
  configuration inside the daemon changes that. `--to` therefore takes a
  **subdirectory of the downloads root**, which covers the real need (keep
  media apart from archives) without punching a hole in the layer that exists
  to stop exactly this. An operator who genuinely wants a second root
  configures it as a config key so it is unveiled *before* self-restriction —
  a possible later key, not part of this phase.
- **`watch_dir`** (config, unset by default). Poll a directory every few
  seconds; a `.torrent` that appears is added and the file is moved aside to
  `.added`. Polling, not inotify: it is a directory check on a timer against
  zero new dependencies, and the latency nobody notices. This is the single
  feature that lets external tooling drive clove without an API compatibility
  shim, and it is how most people already wire a torrent client to anything
  else.
- **`clove stats`.** `PHASE-F.md` §4 lists it in the command surface and it
  was never built; with H5's rates there is finally something to put in it —
  session and lifetime totals, active/queued/seeding counts, the peer budget's
  current draw. *(Built here, then folded into `clove status` by
  `DECISIONS.md` S3: one command answers both "is the daemon alright" and
  "what is my client doing", which is how they were being read anyway.)*

**Cost:** ~180 lines total, one config key.

**`--to` is deferred, and this is the reason.** Everything else here landed;
the per-torrent destination did not. It needs a resume field (a third format
bump in one phase), and more importantly it needs every storage path to be
resolved beneath a *configurable* root rather than the one root there is now.
That is the same surface the M-01 path-traversal finding lives on — the one
`rustix` and `openat` entered the tree for — and adding a second root at the
end of a long phase, beside a TUI, is how the bug that work exists to prevent
gets introduced. It wants its own change with its own review, not a corner of
this one.

*Trigger:* it is wanted as soon as somebody keeps media and archives on
different disks, which is most operators. The design above stands — a
subdirectory of the downloads root, because Landlock will refuse anything else
— and nothing that landed forecloses it.

---

## 9. The TUI — **landed**

**The framework is rejected; the TUI is not.** The full argument, the measured
dependency closures, and the reversal condition are `DECISIONS.md` **S2**,
which amends `PHASE-F.md` §6. In short: §6 was right that `ratatui` +
`crossterm` cannot be paid for, and it was right for a reason that turns out
not to generalise to *"a full-screen view"* — only to *"a framework"*.

What Phase H proposes is **`clove top`**: a full-screen, keyboard-driven view,
hand-rolled, **zero new crates**, built on `rustix` features clove already
pays for (§10).

> **Built as specified, then removed — `DECISIONS.md` S3 (2026-07-31).** Under
> S2's own reversal condition, and `clove watch` went with it. What follows is
> the specification it was built to, kept because S3's reasoning only makes
> sense against it; it is not a description of the CLI today. The parts that
> outlived it are in `clove list`: the header bar, and the rule that there is
> one table implementation.

- **`clove watch` does not change.** It stays the dumb repaint loop —
  no raw mode, works over a pipe, works in a terminal that cannot do better,
  leaves nothing to restore if you kill it. `clove top` is the other one. Two
  commands that are honest about what they are, rather than one command that
  silently changes interaction model based on `isatty`.
- **What it is:** the same renderers the one-shot commands use (this is not
  negotiable — one table implementation, as `watch` already established), plus
  a selection cursor over the torrent list, plus keys. `p` pause/resume, `v`
  verify, `a` announce, `s` sequential, `d` remove (confirm line, `D` for
  `--data`), `Enter` for the detail pane, `q` to quit.
- **Every key is the API call the equivalent subcommand makes.** The TUI holds
  no engine state, adds **no endpoint**, and can be killed mid-anything. A bug
  in it cannot reach the daemon. That containment is what makes the audit
  surface small enough to accept.
- **The genuinely hard part** is terminal restoration, and it is worth stating
  plainly rather than discovering later. A `Drop` guard restores the termios
  and leaves the alternate screen on normal exit and on panic-unwind; a panic
  hook keeps the message readable. Ctrl-C is not a signal problem here —
  in raw mode it arrives as byte `0x03` and is handled in band, which is the
  one way raw mode makes this *easier*. What remains is `SIGTERM`/`SIGHUP`:
  installing a handler needs `unsafe` or a crate, and we will take neither, so
  `kill`ing `clove top` leaves a terminal wanting `stty sane`. That is the
  documented cost, it goes in `clove(1)`, and if it proves intolerable the
  smallest possible fix gets its own memo with evidence.
- **Width.** No `unicode-width` crate. Columns are padded by character count,
  which is what `clove list` does today, so wide (CJK) and combining
  characters misalign by a column. Known, unchanged from current behaviour,
  and not worth a dependency.
- **Input parsing is a parser**, so it is treated as one: a bounded state
  machine over CSI sequences, no allocation, a `cargo-fuzz` target alongside
  the existing ones. The input is a keyboard rather than a hostile network,
  but `fuzz/README.md` already makes this cheap and the discipline is the
  point.

**Cost:** ~600 lines in `clove`, the largest item in this document — and the
reason it is last.

**As built, and the promise kept.** `clove` became a library as well as a
binary so the escape-sequence decoder could carry a `cargo-fuzz` target like
every other parser here — S2 made that a condition, and
`fuzz/fuzz_targets/keys.rs` asserts the properties that matter for a decoder
rather than for an attacker: every decode makes progress, none over-reads, and
none follows a sequence past its documented bound.

The dependency claim was checkable and is checked: `Cargo.lock` holds **52
entries before and after**, because `rustix`'s `termios`/`stdio` features
resolve entirely to crates already in the tree. `ci/check-net-deps.sh` covers
the new manifest without changes — its `net`-feature check is per-manifest.

Driven through a pty end to end during development: raw mode, the alternate
screen, the cursor, `p`/`D`, the confirm-then-act path, and restoration on
exit. Two defects showed up only there and in no unit test — a state column too
narrow for `waiting-for-router`, which stepped every column after it, and
"1 torrents". It is worth building *after* H0 (or it renders hostile text
full-screen), H4 (or its actions have nothing to name) and H5 (or it is a
screen full of totals that never move).

---

## 10. What this costs the dependency budget

**Nothing.** Every item above, the TUI included, is zero new crates.

`clove` today locks **48** dependencies (`DEPENDENCIES.md`, `grep -c '^name = '
Cargo.lock` less the four workspace members). Measured with `cargo add` against
crates.io on 2026-07-30:

| Candidate | Locked crates | Verdict |
|---|---|---|
| clove today | 48 | — |
| `rustix` + `termios`,`event`,`stdio` | **7** | already in the tree, all of them |
| `iocraft` 0.8 | 68 | rejected (S2) |
| `ratatui` 0.30, `default-features = false` | 91 | rejected (S2) |
| `ratatui` 0.30, default features | 181 | rejected (S2) |

The `rustix` row is the finding that makes §9 possible. Enabling `termios`,
`event` and `stdio` on the `rustix` clove already depends on resolves to
exactly `{rustix, bitflags, errno, libc, linux-raw-sys, windows-link,
windows-sys}` — **every one already in `Cargo.lock` at the same version**. Raw
mode (`tcgetattr`/`tcsetattr`) and window size (`tcgetwinsize`) are the only
syscalls a full-screen view needs that are not an ANSI escape sequence, and
`unsafe_code = "forbid"` is what rules out reaching for them directly — the
same reasoning that admitted `landlock` and `rustix` itself.

`DEPENDENCIES.md` gets one paragraph when this lands: the feature list grows,
the crate count does not, and `ci/check-net-deps.sh` keeps its existing
`rustix` special case unchanged (the `net` feature stays off, and the script
already fails if any manifest turns it on).

---

## 11. What we are not building, and what would change that

A deferral with a trigger is a decision; one without is a quiet drop
(`DECISIONS.md`).

- **Download rate limiting.** A token bucket in the read path touches
  thread-per-peer blocking I/O and interacts with the choker, and on I2P the
  tunnels are usually the limit anyway — the ceiling you would configure is
  mostly already enforced by the network. **Upload** limiting is the half with
  a real argument (a client saturating its own tunnels is antisocial) and is
  the cheaper half, living in `spawn_writer` alone. *Trigger:* a live run
  where clove's seeding measurably degrades its own download or the router's
  other traffic. Upload first, download only if that is not enough.
- **Labels, categories, per-torrent tracker editing.** Transmission has them;
  they are state to persist, migrate and render for a benefit that ordering
  and prefixes mostly cover. *Trigger:* an operator running enough torrents
  that H5's ordering stops being sufficient.
- **Transmission/qBittorrent RPC compatibility.** Already a v2 candidate in
  `SCOPE.md` §3, and H6's `watch_dir` covers most of what *arr-style tooling
  actually needs. Unchanged by this phase.
- **Web UI.** Still `SCOPE.md` §2, still deferred, and nothing here moves it.
  Worth noting the opposite of the usual direction, though: `clove top` was
  evidence *for* the deferral, because it was the same JSON a web UI would
  consume, proving the API is sufficient without one. Its removal (S3) does
  not weaken that: the JSON it proved sufficient is unchanged.
- **Per-torrent identities.** Q4 put them in v2 and this phase does not
  disturb that — H1's budget is per *session*, and per-torrent destinations
  would mean N sessions, which is the supervision cost Q4 declined.

---

## 12. Build order

Each step is tier-1 green on its own and each is useful before the next lands.

1. **H0** — sanitise rendered names. A defect, not a feature; goes first
   regardless of what follows.
2. **H4** — prefix resolution, multiple operands, `--all`. Smallest change with
   the largest effect on daily use.
3. **H1** — the shared peer budget. The first thing that makes many torrents
   safe rather than merely possible.
4. **H5** — rates and the `added` ordering (resume `v3`).
5. **H2** — the queue.
6. **H3** — seed ratio and idle stopping.
7. **H6** — add flags, `watch_dir`, `clove stats`.
8. **§9** — `clove top`.

*(Both of 7's and 8's view commands were later removed or folded — `clove
stats` into `clove status`, `clove top` and `clove watch` into `clove list`
plus `watch -n 2 clove list`. See `DECISIONS.md` S3.)*

Steps 1–4 are the ones that would make the difference between "works" and
"pleasant" if the phase stopped early; 5–6 are what let it run unattended; 7–8
are the comforts. Man pages (`clove.1`, `clove.conf.5`, `clove-api.7`) are
updated **in the commit that lands each item**, not afterwards — they are the
primary user documentation (§9 of `SCOPE.md`) and a phase that documents itself
at the end documents itself wrongly.

Total: roughly **1500 lines** across eight steps, no new dependencies, and the
LOC metric goes up by a number this document is willing to state in advance so
that it can be checked against.
