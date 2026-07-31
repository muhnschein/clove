# Decisions

The scope draft deferred Q1–Q7 to M0 spike memos but stated a lean for each.
These memos lock the leans in as **documented, reversible defaults**: the
`i2pnet` trait boundary and per-module error/enum discipline keep every one
of them swappable, and any that fails in practice gets revisited *with
evidence* rather than re-litigated up front. Revisiting one is a normal PR,
not a scope change — but the burden of proof is on the change.

## Q1 — Tracker traffic session: shared with peer traffic

Tracker announces use the same stream subsession and destination as peer
traffic. This is what i2psnark does and what trackers expect (announced
identity must match peer identity), and one subsession is less supervision
state. Revisit only if live-swarm testing shows tracker streams starving
under peer load (QoS) — a separate subsession *on the same destination*
remains possible without engine changes.

## Q2 — Resume format: bencode

We hand-roll a bencode codec anyway (torrent files require it, and §9 wants
hostile-input control over it). Reusing it for resume data means zero new
dependencies, no serde, and exactly one hostile-input parser to harden and
fuzz instead of two. Every resume file carries an integer `version` key from
day one; the format spec lives in `STATE-FORMAT.md` (written with the
implementation at M4). Policy per SCOPE §3: newer clove reads older state;
older clove refuses newer state cleanly.

## Q3 — Fast extension (BEP 6): yes, in v1

i2psnark supports it, it measurably improves swarm behavior (allowed-fast
pieces during choke, precise have-all/have-none), and it is cheap next to
BEP 10 which we need regardless. Wire-codec work, no architectural cost.

## Q4 — Identity: single client identity + global ephemeral flag

One persisted destination keypair per client (stable identity across
restarts), plus one global `ephemeral` config flag that skips persistence.
Per-torrent transient identities are v2: they multiply session topology
(one PRIMARY session each) and supervision state for a niche benefit.

## Q5 — Concurrency: synchronous thread-per-peer

Blocking I/O, one thread per peer connection, dedicated worker threads for
disk and hashing, bounded channels between them. Most auditable option and
entirely viable at I2P scale (50–200 peers; tunnel latency dwarfs thread
cost). **De-risked externally:** yosemite 0.7.0 ships a first-class `sync`
cargo feature (alongside `tokio`/`smol`), so no async runtime enters the
dependency tree at all. The planned M0 concurrency spike is therefore
dropped. Fallback if a concrete wall is hit: smol via yosemite's `smol`
feature, behind the same `i2pnet` traits. The R2 stress harness (i2pd SAM
under many concurrent streams) stays — it tests router behavior, not our
runtime choice — and runs in Phase D.

## Q6 — HTTP API server: hand-rolled HTTP/1.1

We control both ends (our CLI, local socket), need a tiny subset (GET/POST,
JSON bodies, token header, unix socket first), and the opentracker precedent
says a few hundred careful lines beat a framework's transitive closure.
Same reasoning covers the *client* side (tracker announces over I2P
streams): one shared minimal HTTP/1.1 implementation in `clove-core`.

## Q7 — Wire identity: peer-ID prefix `-CV0001-`, client string `clove/0.1`

Azureus-style prefix `CV`, which does not collide with anything in the
informal BEP 20 registry (CT/CD/CB etc. are taken; CV is free as of this
writing). Version digits track releases. **Checkpoint:** re-verify against
the registry *and* observed I2P-swarm peer IDs before M3 — first live
announce is the wire-permanent moment. Until then this is a candidate, after
that it never changes.

---

# Scope amendments

Unlike the Q memos above, these change `SCOPE.md` rather than fill in a blank
it left. Each states what moved, the evidence that moved it, and the condition
under which it moves back — a deferral with a trigger is a decision; one
without is a quiet drop.

## S1 — emissary: tracked, not a 0.1 gate (2026-07-28)

`SCOPE.md` §6 required the live sign-off on all three routers for 0.1. It now
requires i2pd and Java I2P. emissary keeps its quadlet, its address-book
helper and its column in `LIVE-TESTING.md` §6.3; what it loses is the power to
block a release.

**The evidence.** Across every live session, emissary 0.4.0 has never reached
a swarm, and the reason is not clove. Its SAM bridge has been fine since the
first run — sessions come up in 15–60s. Naming is what fails, in two
independent places:

- **Hostnames**, from an address book its subscription has not fetched
  (`PROTOCOL.i2p-bt` §5.5). This is the failure `make router-addressbook`
  exists to sidestep.
- **Its own freshly created b32 destinations**, via `NAMING LOOKUP`, which
  involves no address book at all — a b32 is a hash resolved through the
  netDb. `sam-stress` reports it plainly: *"neither b32 resolves, including
  the dialer's own"*. Seeding an address book does not and cannot fix this
  one, which is why an operator who ran `router-addressbook` before a sweep
  still saw `KeyNotFound`.

Same-router leaseSet resolution was already documented as broken against a
demonstrably healthy router (§2.8). Three naming failures, no clove component
implicated in any of them.

**The ecosystem's own position.** The I2P project lists emissary among
alternative clients as experimental. Gating our release on a live sign-off
from a router its own project does not call stable held us to a harder bar
than upstream holds itself to.

**Consistency.** §2 cut `clove fetch` on exactly this reasoning: *every hour
spent there is an hour not spent on the live-router sign-off that actually
gates 0.1*. The same sentence applies here and reaches the same answer.

**Why this is not a deletion.** The infrastructure stays — quadlet,
`Containerfile.emissary`, `seed-addressbook.sh`, the §6.3 row. So does the
work emissary paid for: the metadata fetch that names its failing stage, the
negative-cache countdown in the error text, `sam-stress`'s "this router does
not answer NAMING LOOKUP for its own destinations" caveat. Every one of those
came from chasing an emissary failure, all of them make *every* router's
results legible, and none of them is going anywhere.

**Reversal condition.** emissary reaches a stable release. Re-run `make swarm
TORRENT=… ROUTER=emissary`; if it carries a download, it returns to the gate
and this memo is struck. Until then its column is recorded, not required.

## S2 — A TUI, but no TUI framework (2026-07-30)

`PHASE-F.md` §6 rejected a TUI and invited this memo in the same breath: *"if a
full curses-style UI is ever genuinely wanted, it is a separate,
budget-spending decision — not smuggled in with M4."* It is wanted, this is the
decision, and it spends nothing.

**What §6 got right, and how far it reaches.** §6 rejected `ratatui` +
`crossterm` on `SCOPE.md` §9 grounds — roughly doubling the tree, breaking the
human-reviewable `cargo vendor` goal, a large raw-mode/input/resize surface to
audit. Measured rather than estimated, against crates.io on 2026-07-30, with
`cargo add` into an empty crate:

| Candidate | Locked crates |
|---|---|
| clove today | **48** |
| `iocraft` 0.8.4 | 68 |
| `ratatui` 0.30.2, `default-features = false, features = ["crossterm"]` | 91 |
| `ratatui` 0.30.2, default features | 181 |

§6 undersold it. The full `ratatui` is not double the tree, it is **just under
four times** it, and it arrives with `syn` at three major versions, `serde`,
`mio`, `signal-hook`, `strum` and `regex`. Stripping it to no default features
still lands at 91 — nearly double clove's whole closure for the parts of a
framework we would use least. `iocraft` is smaller at 68 and still out: it
pulls `crossterm` anyway, adds `futures`/`parking_lot`/`regex` and its own
proc-macro pair, and its async-React model fights Q5's synchronous
thread-per-peer discipline rather than fitting it. **All three stay rejected,
and the rejection is now quantified.**

**What §6 did not distinguish.** It treated "a TUI" and "a TUI framework" as
one thing. They are not. A full-screen view needs raw mode, window size,
keypress decoding, cursor addressing and a repaint discipline. Four of those
five are ANSI escape sequences and a byte-level state machine — which is
precisely the "300 focused lines over 30,000 imported" clove already chose for
bencode, HTTP/1.1 at both ends, JSON at both ends, and argument parsing. Only
raw mode and window size need a syscall, and `unsafe_code = "forbid"` is what
stops us calling them directly.

**The finding that closes it.** clove already depends on `rustix` (entered
2026-07-29 for `openat`-based path handling). Enabling `termios`, `event` and
`stdio` on it resolves to exactly `{rustix, bitflags, errno, libc,
linux-raw-sys, windows-link, windows-sys}` — **seven crates, every one already
in `Cargo.lock` at the same version**. `tcgetattr`/`tcsetattr` give raw mode and
`tcgetwinsize` gives the window size, both without `unsafe`, at a dependency
cost of **zero new crates**. The reason §6 could not reach this conclusion is
that it was written in Phase F and `rustix` did not enter the tree until Phase
G — the argument was sound on the evidence it had.

**The decision.** No TUI framework, unchanged and now with numbers. A
hand-rolled full-screen view is permitted, specified as `clove top` in
`PHASE-H.md` §9, on these conditions:

- It lives entirely in `clove(1)`. **No new API endpoint**, no engine state,
  no daemon change. Every keystroke is the same `/v1/` call the equivalent
  subcommand already makes, so a bug in it cannot reach `cloved`.
- It reuses the existing table renderers rather than growing a second set.
- `clove watch` stays exactly as it is — no raw mode, pipe-safe, nothing to
  restore. `clove top` is an additional command, not a replacement, and
  nothing chooses between them by sniffing `isatty`.
- The costs are documented before they are paid, in `PHASE-H.md` §9: a
  `SIGTERM`-killed `clove top` leaves a terminal wanting `stty sane` (a signal
  handler needs `unsafe` or a crate, and we take neither), and columns are
  padded by character count so wide glyphs misalign — which is what
  `clove list` already does.
- The escape-sequence input decoder gets a `cargo-fuzz` target like every
  other parser here. **Done:** `fuzz/fuzz_targets/keys.rs`, which is why
  `clove` is a library as well as a binary. For a decoder the failure that
  matters is a stall rather than an attacker, so it asserts that every decode
  makes progress, over-reads nothing, and follows no sequence past its bound.

**Reversal condition.** Two, in opposite directions. If the hand-rolled view
exceeds roughly 800 lines in `clove`, or if terminal restoration needs a signal
handler after all, the "write it ourselves" premise has failed and `ratatui` is
re-costed honestly against that failure rather than against an estimate. *As
built it is ~640 lines across `term` and `top`, and `Cargo.lock` holds the same
52 entries it did before — so the premise held, on both counts, and the bound
stays live for whatever the view grows into next.* And if
`clove top` lands and nobody uses it over `clove watch`, §9 of `SCOPE.md`
applies in its usual direction — *removals are announced proudly* — and it
goes.

## S3 — The Transmission RPC shim, built (2026-07-31)

`SCOPE.md` §3 put a compatibility shim in v2: *"Explicitly not compatible with
the Transmission/qBittorrent APIs in v1 (compat shim is a v2 candidate — worth
it for \*arr-style tooling, not worth the constraint now)."* `PHASE-H.md` §11
repeated the deferral unchanged. It is now built, for Transmission only, and
`docs/PHASE-I.md` is the design.

**The evidence.** The deferral rested on a cost estimate — "the constraint" —
made before Phase F existed. What Phase F and H actually built removed most of
that cost:

- a hand-rolled HTTP/1.1 **server** (Q6), which did not exist when §3 was
  written;
- a hostile-input-hardened JSON **parser** as well as an encoder, added in
  Phase F §7 3 for the CLI and reusable here;
- rates, queue states, add ordering, seed limits and a global peer budget
  (`PHASE-H.md` H1–H5) — which are, between them, most of the fields
  `torrent-get` and `session-get` are asked for. Without H5's rates the shim
  would have had to lie about speed or omit it.

So the shim is one file over parts already paid for. `Cargo.lock` is unchanged
by it, which was the constraint §9 actually cares about.

**What it buys, weighed against the alternative.** `SCOPE.md` §2 defers a web
UI indefinitely, and §9's budget is why. Transmission's RPC is a protocol
rather than a library: implementing it costs a file, and it buys every frontend
that already speaks it — tremc, transgui, Transdroid, Flood, Torrent Control,
and the \*arr download-client interface — none of which clove has to build,
ship, or keep working. There is no other item in the backlog with that ratio.

**Why this does not weaken §5.** The surface is a presentation layer that emits
strings and constants and never constructs a type, so `clippy.toml`'s
`disallowed_types` and `ci/check-net-deps.sh` both pass **unchanged**. That they
needed no edit is the test of it: an implementation that required relaxing
either would have meant IP vocabulary reaching the engine, and would have been
the wrong implementation. The one new capability — a loopback TCP listener,
since no Transmission client can speak to a unix socket — uses the
loopback-validating helper `i2pnet` has had since Phase F, and gets **no**
override for a wider bind. `i_know_sam_is_remote` exists because a remote SAM
bridge is a real deployment; a remote API bind is not, and §5's answer to
reaching the daemon from elsewhere is a forwarded port.

**Conditions this was accepted on.**

- **Off by default** (`transmission_rpc no`), so a daemon nobody asked to
  expose one does not carry a second authentication scheme.
- **No engine change.** It reads the JSON `/v1/` already emits and calls the
  registry operations `/v1/` already calls. The two additions it did make to
  the registry — `piece_length` in the detail object, and one listing pass that
  renders every torrent in full — serve `clove show` and `clove top` equally.
- **No field invented that a client would act on.** Where clove has no answer
  the field is a documented constant that is true of every I2P torrent, or it
  is absent. `peers` is absent because `registry.rs` already asserts a peer's
  address never reaches the API, and that policy outranks a populated peer tab.
- **The one property traded is written down**, not discovered later:
  `PHASE-F.md` §2's "the daemon never parses JSON" stops being true whenever
  this is enabled. It parses with the already-fuzzed parser, under the existing
  body cap.
- **A real client in CI.** `ci/transmission.sh` installs and drives
  `transmission-remote` rather than trusting curl. This was not a formality:
  it found two defects in the first minute — a route that missed the trailing
  slash every client sends, and a tracker section that rendered as nothing —
  and every unit test was green through both (`PHASE-I.md` §7).

**Reversal condition.** Two. If the surface starts pulling engine changes
toward Transmission's model rather than restating clove's — a stored queue
position, a peer table the daemon would not otherwise keep, per-file byte
accounting — then the "presentation layer" premise has failed and it is
re-costed as a feature rather than a file. And if `SCOPE.md` §9's usual
direction applies — nobody drives clove this way — it goes, proudly, in one
commit, because that is what one file with one job and no engine state is for.

qBittorrent's API is **not** covered by this and stays a v2 candidate on §3's
original terms. *Trigger:* a frontend somebody actually wants that speaks only
qBittorrent; most speak both.
