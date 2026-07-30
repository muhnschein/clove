# Fuzzing clove's parsers

`docs/SCOPE.md` §9 asks for fuzzing of every parser. There are two layers, and
they are complementary:

| | where | toolchain | when |
|---|---|---|---|
| **Sweep** | `crates/clove-core/tests/hostile.rs` | stable | every push, seconds |
| **Fuzz** | this directory | nightly + cargo-fuzz | manual / scheduled, hours |

The sweep is a deterministic mutation pass that runs in normal CI, so parsers
get adversarial input on every commit with no special setup and no flakes. The
targets here are coverage-guided and go far deeper, but need a nightly
toolchain, so they run out of band.

This crate is **excluded from the workspace** (see the root `Cargo.toml`).
`libfuzzer-sys` and its dependencies therefore never enter the shipped build,
count against the dependency budget, or appear in the workspace `Cargo.lock`
that `ci/check-net-deps.sh` gates.

Being outside the workspace, these targets are not compiled by
`cargo test --workspace`, so CI runs `cargo check --manifest-path
fuzz/Cargo.toml` on every push. Without it a changed signature in `clove-core`
breaks a target silently and the nightly job is the first to notice, a day
later. Building the targets needs no nightly toolchain; only *running* them
does.

## Running

```sh
cargo install cargo-fuzz            # once
rustup toolchain install nightly    # once

make fuzz-all                       # every target, ~20 min, writes a report
make fuzz-all QUICK=1               # ~2 min, "does it still build and run"
make fuzz-all SCALE=8               # a long hunt
make fuzz-all SEED=1                # and keep what it finds (see Corpus)
make fuzz TARGET=metainfo SECS=600  # one target, straight through
```

The budgets below add up to 3360 seconds of fuzzing, which is about twenty
minutes of wall clock on a four-core machine: `ci/fuzz.sh` keeps `nproc - 1`
targets running and starts the longest first, refilling a slot as soon as one
frees rather than waiting for a batch.

`make fuzz-all` runs `ci/fuzz.sh`, which writes `fuzz/report-<stamp>.txt`. The
report is the deliverable: it carries the commit and tree it ran against, the
toolchain, per-target executions and coverage, and — if anything crashed — the
input **base64-encoded** along with the commands to reproduce and minimise it.
That is deliberate. "It crashed on metainfo" is not something anyone can act
on from somewhere else; a report that contains the failing bytes is.

`ci/fuzz.sh` also reports, per target, how many edges the run reached that its
corpus did not already reach — `cov 498 (+191 this run)` — so the budgets below
can be revised from evidence rather than taste.

That number is the coverage on libFuzzer's `INITED` line subtracted from the
coverage at the end, and it is deliberately not `new_units_added`, which an
earlier version of this script used. A unit is kept for hitting a new execution
*count bucket*, not a new edge, so the two can disagree completely: in the
2026-07-30 sweep `json` added 3262 units and did not reach one edge it had not
already reached thirty seconds in. Budgeting on unit counts would have bought
`json` more time and `magnet` less, which is backwards.

A crash is written to `fuzz/artifacts/<target>/`. Reproduce and minimise it:

```sh
cargo +nightly fuzz run bencode fuzz/artifacts/bencode/crash-<hash>
cargo +nightly fuzz tmin bencode fuzz/artifacts/bencode/crash-<hash>
```

Then add the minimised input as a regression case in the module's own tests —
that is where it belongs permanently, so the bug stays dead whether or not
anyone runs the fuzzer again.

## Targets

Each target covers one hostile-input surface, and most assert a property
beyond "did not panic":

| Target | Surface | Extra property asserted |
|---|---|---|
| `bencode` | every torrent, resume file, tracker reply | decode/encode round trip agrees |
| `metainfo` | `.torrent` files from anyone | no path traversal or NUL in file paths; file lengths sum to the total; surviving trackers are I2P URLs |
| `resume` | on-disk state, possibly tampered with | bitfield lengths match the piece count; priorities in range; round trip agrees |
| `json` | daemon replies read by the CLI | round trip agrees |
| `http` | tracker responses and API requests | — |
| `wire` | peer messages and handshakes | — |
| `tracker` | announce responses | a hostile `interval` cannot make us announce inside the local floor |
| `extensions` | `i2p_pex`, `ut_metadata`, BEP 10 handshake | the PEX peer cap holds |
| `magnet` | magnet URIs | non-I2P trackers are filtered out |

The properties matter as much as the crashes: a parser that accepts a torrent
whose paths escape the download directory has not crashed, and is still a
serious bug.

## Dictionaries

`fuzz/dicts/<target>.dict` lists the tokens the parser actually looks for —
bencode keys with their length prefixes, `magnet:?xt=urn:btih:`, HTTP framing,
wire message headers. `ci/fuzz.sh` and CI pass the file for a target whenever
one exists, and the mutator can then *insert* a token rather than having to
arrive at it byte by byte.

libFuzzer already recovers some of these unaided: a `strip_prefix("magnet:?")`
compiles to a `memcmp` that the sanitizer intercepts, and the literal goes
into an auto-dictionary. A bencode key lookup walks bytes in a loop, and the
interception never sees it. So the win is uneven and worth measuring rather
than assuming — 120s per arm from the committed seed, three RNG seeds each,
2026-07-30:

| Target | Edges gained, no dict | Edges gained, dict |
|---|---:|---:|
| `magnet` | +1, +1, +1 | +191, +187, +191 |
| `extensions` | +21, +19, +0 | +113, +114, +102 |

The `magnet` row is the one to look at twice. Without a dictionary that target
had, in effect, stopped: one edge in two minutes, every time, on a corpus of
561 files that between them never produced a valid `magnet:?` prefix under
mutation. The dictionary is not a marginal speedup there, it is the difference
between fuzzing the parser and idling in front of it.

The other seven dictionaries were written on the same reasoning rather than on
per-target evidence, and six of them moved a target that the pre-dictionary
reports had called saturated — see the coverage column below. `wire` is the
one that did not.

## Budgets

Not flat, and set from what the last report measured rather than from taste.
The `--- results ---` section of any report is the input to this table; the
figure that matters is edges gained per 100 seconds, because it is the only one
that says whether more time would buy anything.

| Target | Budget | Cov before | Cov now | Gained | Edges/100s |
|---|---:|---:|---:|---:|---:|
| `wire` | 60s | 168 | 168 | 0 | 0.0 |
| `bencode` | 180s | 387 | 389 | +2 | 0.0 |
| `json` | 240s | 732 | 735 | +3 | 0.0 |
| `metainfo` | 240s | 448 | 454 | +6 | 0.3 |
| `extensions` | 420s | 322 | 416 | +94 | 2.0 |
| `http` | 420s | 469 | 559 | +90 | 3.9 |
| `tracker` | 480s | 595 | 642 | +47 | 9.0 |
| `resume` | 480s | 283 | 336 | +53 | 21.7 |
| `magnet` | 840s | 402 | 498 | +96 | 21.1 |

"Cov before" is the 2026-07-30 pre-dictionary report; "cov now" is after the
sweep of the same date that introduced them. The per-100s column is from that
sweep alone, which is why `extensions` and `http` read low despite large
absolute gains: both banked almost all of it in the first thirty seconds and
then flattened.

The ordering this implies is nothing like the allocation it replaced, which had
`magnet` and `resume` — the two fastest climbers — on the lowest tier, and
`metainfo` at 0.3 edges/100s on the joint highest. The total is unchanged at
3360s: a reallocation, not a bigger bill.

The table lives in exactly one place, `budget_for` in `ci/fuzz.sh`. CI asks for
it — `./ci/fuzz.sh --budget magnet` — rather than keeping a second copy in the
workflow matrix, which is what it used to do under a comment saying the two
must agree. Revising a budget is now one edit.

A plateau means "saturated given this corpus and these mutators", not "fully
explored", and this table is the demonstration. Every target the previous
reports had flat was flat against a fuzzer with no dictionary; six of them
moved as soon as it had one. So treat these budgets as current rather than
settled — this is the first dictionary-enabled sweep, some of the gain above is
newly-opened ground being consumed once, and the next report will say so.

`wire` is the exception that stayed put: 168 edges, no new units, across five
independent runs and 190M executions. Sixty seconds is the right budget, but
the reading is not "the wire codec is exhausted" — it is that a target which
parses one message in isolation has run out of things to say about a protocol
whose bugs live in *sequences*. `docs/CODE-REVIEW-2026-07.md` §C already
carries the fix, a stateful target driving `Message::parse` → `on_message` over
a real `Torrent`; these runs are the evidence for it.

## Corpus

A seed corpus is committed as `fuzz/seed-corpus.tar.gz`, unpacked over
`fuzz/corpus/` before a run by both `ci/fuzz.sh` and CI. Loose corpus files
stay git-ignored; only the tarball is tracked.

One tarball rather than thousands of files on purpose. The content is about a
megabyte either way, but as loose files it is thousands of git objects in
every clone, for inputs nobody is going to review individually. As a tarball
it is one line in a diff. `ci/fuzz-seed.sh` packs it deterministically — sorted
names, zeroed mtimes, `gzip -n` — so it shows up in `git status` when its
contents changed and not merely because it was rebuilt.

Starting cold is the thing this avoids. An unseeded run spends much of its
budget rediscovering that input has to be bencode at all — which is fuzzing
the question "is this a torrent" rather than the parser behind it.

**A corpus is an asset only while it is minimised.** libFuzzer keeps an input
for reaching a new execution-count *bucket*, not a new edge, so a corpus grows
without the coverage growing with it. The 2026-07-30 sweep ended with 19 310
files; `cmin` took them to 4 943 at the same coverage:

```
cmin: bencode        833 -> 388     cmin: wire            40 -> 40
cmin: metainfo       578 -> 380     cmin: tracker       1699 -> 757
cmin: resume         743 -> 304     cmin: extensions     637 -> 300
cmin: json          4491 -> 1300    cmin: magnet        7191 -> 770
cmin: http          3098 -> 704
```

Nine files in ten were carrying nothing, and `magnet` alone had accumulated
7191 to hold what 770 hold. That bloat is charged to the next run twice —
startup replays all of it, and mutation energy is then split across all of it.
On `magnet` the cost had already grown larger than the benefit of seeding at
all: 120s from the 561-file seed reached 308 edges, where the same 120s from an
*empty* directory reached 498. Seeding is not free, and `cmin` is not
housekeeping.

So fold runs back in, rather than letting `fuzz/corpus/` accumulate until it
dies with the working tree:

```sh
make fuzz-all SEED=1   # sweep, then minimise and repack in one go
make fuzz-seed         # or fold in whatever the corpus has grown since
```

A report from a run that grew the corpus says so, and says this, in its
`--- corpus ---` section — the numbers in a report are no use if the inputs
behind them are thrown away.

In CI the scheduled job caches each target's corpus and restores it next time,
so the corpus compounds; it `cmin`s *before* the cache is written, so the
compounding is in coverage rather than in file count. Each run also uploads its
minimised corpus as an artifact, kept 30 days, which is what somebody folds
into the committed seed with `make fuzz-seed`.
