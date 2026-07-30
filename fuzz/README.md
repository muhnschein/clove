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
| `wire` | a peer's whole byte stream: framing, messages, handshake | a message survives its own encoder unchanged |
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
one that did not, and the cause turned out to be the target rather than the
dictionary. Most of `wire.dict` is length-prefixed framing —
`"\x00\x00\x00\x0d\x06"` is a request — and a target that handed its input
straight to `Message::parse` read that as a *body*, where a five-byte request
header is nothing but a wrong-length message. The vocabulary was written for a
reader that did not exist yet. It does now; see Budgets.

## Budgets

Not flat, and set from what the reports measured rather than from taste.

The two sweeps of 2026-07-30 are what the current table rests on, and together
they are a controlled experiment rather than two readings. Both started from
the same corpus — the `INITED` coverage libFuzzer prints after replaying the
seeds was identical in all nine targets — and the second ran at `--scale 8`.
The only difference between them is time, which is the only thing a budget
buys.

| Target | Budget | +edges at 1x | +edges at 8x | What 7x more time bought |
|---|---:|---:|---:|---:|
| `tracker` | 480s → **900s** | +47 | +94 | +47 |
| `resume` | 480s → **600s** | +53 | +70 | +17 |
| `extensions` | 420s | +94 | +103 | +9 |
| `metainfo` | 240s | +6 | +8 | +2 |
| `http` | 420s | +90 | +91 | +1 |
| `bencode` | 180s | +2 | +2 | 0 |
| `json` | 240s | +3 | +3 | 0 |
| `magnet` | 840s → **240s** | +96 | +96 | 0 |
| `wire` | 60s → **120s** | 0 | 0 | 0 |

`magnet` is what moved the table. It held the largest budget in the file on the
strength of +96 edges in 840 seconds — and returned exactly +96 in 6720. Seven
eighths of the biggest allocation here was buying nothing, and the dictionary
A/B had already said so from the other end: 120 seconds from an *empty*
directory reached 498, the same ceiling both sweeps stopped at. `tracker` was
the only target still clearly paying for time at the far end, and takes most of
what `magnet` gives up.

Nothing else moves. Cutting `http` or `json` below the budget they were
measured at would be extrapolation from a pair of runs that only ever tested
them upwards. The report now measures that directly instead, so the next one
settles them without guessing. The total is unchanged at 3360s: a reallocation,
not a bigger bill.

The table lives in exactly one place, `budget_for` in `ci/fuzz.sh`. CI asks for
it — `./ci/fuzz.sh --budget magnet` — rather than keeping a second copy in the
workflow matrix, which is what it used to do under a comment saying the two
must agree. Revising a budget is now one edit.

### Reading a run for what it says about time

A previous version of this table carried an "edges per 100s" column and set the
ordering from it. It does not reproduce from its own inputs — `extensions` is
listed at 2.0 where +94 edges in 420 seconds is 22.4, `http` at 3.9 where +90
in 420 is 21.4 — so the ordering it justified could not be checked. It is gone,
replaced by something measured rather than derived: `ci/fuzz.sh` now reports
the point in the run at which coverage stopped growing.

```
      note: +313 edges, all of them by 12% of the run — the rest bought nothing
```

libFuzzer prints `cov:` on every progress line, so the first line carrying the
run's final figure is the moment it stopped learning; everything after it is
budget that demonstrably bought nothing. Past the halfway mark the note reads
the other way instead — still gaining at *n*% of the run, worth more time — so
a budget that is genuinely too small says so in the same sentence.

A total on its own can say neither, and got it exactly backwards here: this
script called `magnet` "still climbing, worth more time" on +96 edges, and
eight times the budget returned +96 again.

A plateau still means "saturated given this corpus and these mutators", not
"fully explored". `wire` is the standing proof of that.

### `wire`

`wire` sat at 168 edges through the scale-8 sweep's 239M executions, having not
moved since it was written. The reading was never "the codec is exhausted": a
peer connection is a handshake and then a *stream*, and the target parsed one
body in isolation. `read_frame` — the length prefix, the oversize ceiling, the
short read — was not fuzzed at all, and `wire.dict` was mostly framing tokens
that a body parser can only reject.

It now reads a stream of frames and asserts that a message survives its own
encoder. Same toolchain, same 40-file corpus, same dictionary, 120s per arm,
three RNG seeds each:

| | Edges reached | Corpus after |
|---|---:|---:|
| body parser | 168, 168, 168 | 40, 40, 40 |
| frame stream | 337, 337, 337 | 302, 272, 316 |

Sixty seconds was the right budget for a target that could not use more. 120s
is provisional for one that can — all three arms reached 337 inside the first
1% of the run, so this is a floor to re-derive from the next report rather than
a settled figure.

What it does not yet cover is a *session*: `Message::parse` feeding
`on_message` against a real `Torrent`, where the state machine's bugs live.
This target reaches the codec, not the protocol.

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
without the coverage growing with it. The scale-8 sweep of 2026-07-30 ended
with 25 003 files; `cmin` took them to 5 173 at the same coverage:

```
cmin: bencode        953 -> 395     cmin: wire            40 -> 40
cmin: metainfo       571 -> 393     cmin: tracker       3491 -> 833
cmin: resume        1089 -> 315     cmin: extensions     879 -> 319
cmin: json          7859 -> 1329    cmin: magnet        5884 -> 848
cmin: http          4237 -> 701
```

Four files in five were carrying nothing, and `json` alone had accumulated 7859
to hold what 1329 hold. That bloat is charged to the next run twice — startup
replays all of it, and mutation energy is then split across all of it. On
`magnet` the cost had already grown larger than the benefit of seeding at all:
120s from the 561-file seed reached 308 edges, where the same 120s from an
*empty* directory reached 498. Seeding is not free, and `cmin` is not
housekeeping.

**What is committed is checked, not assumed.** A seed is only worth its size if
it actually reaches what the run that produced it reached, so the coverage of
the packed tarball is measured — replay each target over it with `-runs=0` and
read the `INITED` line:

| | `bencode` | `metainfo` | `resume` | `json` | `http` | `wire` | `tracker` | `extensions` | `magnet` |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Sweep reached | 389 | 456 | 353 | 735 | 560 | — | 689 | 425 | 498 |
| Committed seed reaches | 389 | 456 | 353 | 735 | 560 | 337 | 689 | 425 | 498 |

That check is worth running because the alternative is silent. The two sweeps
of 2026-07-30 began from *identical* coverage in all nine targets — the second
never received the first's findings, so 26 880 seconds of fuzzing went into
re-deriving ground that had already been covered once. A seed corpus that does
not compound is a seed corpus that is not doing its job, and the only way to
know which one you have is to measure it.

The same check is what decided what to keep here. The previously committed seed
was merged with the sweep's and re-minimised, on the theory that the older
corpus might hold edges the sweep had dropped: it did not. The union reached
exactly the same coverage in all nine targets and cost 389 extra files, so the
sweep's own corpus is what is committed — plus a fresh `wire` corpus, since
that target now reads its input as a frame stream and the old 40 files reach 35
edges of the 337 available. One more `cmin` pass over the result took 5426
files to 5375, which is what the tarball holds.

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
