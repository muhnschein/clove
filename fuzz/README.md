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
| `dest` | SAM base64 destinations and b32 labels | both codecs round trip; a truncated destination is still a whole destination |

The properties matter as much as the crashes: a parser that accepts a torrent
whose paths escape the download directory has not crashed, and is still a
serious bug.

### Is this list still the whole surface?

Worth re-asking whenever the code has moved, because a target set is only
current until someone adds a parser. Checked against `main` on 2026-07-31, the
public parse entry points across all four crates are:

`bencode::{decode, decode_prefix}`, `config::{parse, parse_seed_ratio}`,
`extension::Handshake::parse`, `http::{read_response, read_request}`,
`json::parse`, `magnet::parse`, `metadata::MetadataMessage::parse`,
`metainfo::MetaInfo::{parse, from_info_dict}`, `pex::PexMessage::parse`,
`resume::Resume::decode`, `tracker::parse_response`,
`wire::{Message::parse, Handshake::parse, read_frame}`, and in `i2pnet`,
`addr::{destination_bytes, destination_len, base32_decode, base32_encode,
i2p_base64_decode, i2p_base64_encode}` with `DestHash::{from_b32,
from_b64_destination}`.

The `clove-core` half has kept up on its own — `cargo check --manifest-path
fuzz/Cargo.toml` runs on every push, so a changed signature breaks the build
rather than rotting a target, and the `metainfo` target has grown alongside the
code it covers. What had *not* kept up was the crate list: the fuzz crate
depended on `clove-core` alone, so `i2pnet` had no coverage at all. That is
what `dest` closes, and it is not a hypothetical surface — see below.

Two gaps are known and left open:

- **`config::parse`.** A parser, and uncovered. Its input is a file the
  operator writes, which puts it below everything above on the list of things
  an attacker reaches.
- **The SAM line protocol** — `read_sam_line`, `parse_session_status`,
  `read_dest_line` in `crates/i2pnet/src/sam.rs`. These read from the router's
  socket and are genuinely untrusted, but they are private, so a target needs a
  seam in production code. `dest` covers the parsing they delegate to; the
  line framing around it is still uncovered.

### `dest`

The one parser here whose mis-reading has already cost something. A destination
and a *private key blob* are both one long base64 run; what separates them is a
length-prefixed certificate header 384 bytes in. Before `destination_len`
existed nobody looked, and every announce carried our private keys in the `ip`
parameter until postman's tracker refused them — `crates/i2pnet/src/addr.rs`
carries the full account, dated 2026-07-27.

The arithmetic that tells the two apart reads an attacker-supplied 16-bit
length and slices with it, which is the shape a fuzzer exists for. The target
asserts what that incident violated: a blob truncated to its destination is
itself a whole destination, the length never runs past the input, and
`destination_bytes` and `from_b64_destination` agree about whether the input
was a destination at all. Around it sit the two codecs, asserted as round
trips in the direction we control — whatever bytes we hold, the text we emit
must decode back to exactly them, because a codec that loses a byte corrupts a
peer identity rather than failing.

It reached 366 edges from a 494-file seed, on a 120s budget that was
provisional — the same footing `wire` was given when it was rewritten, a floor
to re-derive from the first report with something to say about it. That report
is the 2026-08-16 sweep: +70 edges, all of them by ~1506s, and a seed that now
starts the next run at all seventy. The budget stays at 120s, on evidence
rather than for want of any; see Budgets.

There was one more, `keys`, over the terminal escape-sequence decoder behind
`clove top` — the one target whose input was a keyboard rather than an
attacker, and which asserted progress rather than safety, because the failure
that matters for a decoder is a stall. It went when the full-screen view did
(`docs/DECISIONS.md` S3), along with the decoder, the `clove` library target
that existed to expose it, and this project's only raw-mode surface. A parser
removed is a fuzz target that never has to run again.

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

The 2026-07-31T08:38Z sweep is what the current table rests on, and it ran at
`--scale 80` — a budget nobody would schedule, which is exactly the point. It
is the far end of the curve, and what a target does with eighty times its
budget settles whether that budget is short.

| Target | Budget | Seed cov | Final cov | Gained | Last new edge |
|---|---:|---:|---:|---:|---:|
| `resume` | 600s → **720s** | 357 | 368 | +11 | ~42717s of 48000s |
| `extensions` | 240s → **900s** | 425 | 433 | +8 | ~4214s of 19200s |
| `tracker` | 1500s → **900s** | 882 | 883 | +1 | ~14242s of 120000s |
| `http` | 420s → **120s** | 562 | 562 | 0 | — |
| `bencode` | 120s | 389 | 389 | 0 | — |
| `metainfo` | 120s | 456 | 456 | 0 | — |
| `magnet` | 120s | 498 | 498 | 0 | — |
| `json` | 120s | 735 | 735 | 0 | — |
| `wire` | 120s | 337 | 337 | 0 | — |
| `dest` | **120s** | — | — | — | new target, see below |

Two rows overturn the previous table, and one of them was this file's mistake.
`extensions` had just been cut to 240s on the strength of a flat scale-8 run,
and its gains here land at ~4214s — seventeen times that. The cut was made
where the evidence ran out rather than where the target stopped, which is the
one thing the floor rule was written to avoid. It goes up.

`tracker` is the other. It took most of the last reallocation on +193 edges and
returned **+1 edge for 2.2 billion executions**. That is the `magnet` lesson
again, one revision later and from the opposite direction: a target that was
genuinely still climbing can be finished by the next sweep, and a budget set on
a gain rather than on a margin will always be a sweep behind.

So the three that still convert seconds into edges take the budget, and
everything flat at 8x *and* 80x sits at the floor — `http` joins them.

That floor is a choice, and worth being plain about: it is not a measured
plateau. Once the seed reaches a target's ceiling no budget buys another edge,
so no report can say how few seconds would do — what the seconds still buy
there is crash search, which coverage cannot price. 120s is small enough to be
cheap and long enough to be a real hunt. `wire` is the standing warning against
reading a floor as a finished target: it sat flat for three sweeps because the
target was too narrow, not because the codec was clean.

None of the three raised budgets reaches its plateau, and none is meant to.
What carries a gain between runs is the seed corpus, which round-trips exactly;
a budget only has to keep finding *some* of the ground each run for the corpus
to accumulate the rest.

The total is unchanged at 3360s, now across ten targets: a reallocation, not a
bigger bill.

### The 2026-08-16 sweep, and why a plateau time is not a budget

`--scale 100`: 4.57 billion executions across the ten targets, about 93
target-hours, **no crashes**. It revises none of the numbers above, and why it
does not is the part worth keeping.

| Target | Budget | Seed cov | Final cov | Gained | Last new edge |
|---|---:|---:|---:|---:|---:|
| `dest` | 120s | 366 | 436 | +70 | ~1506s of 12000s |
| `tracker` | 900s | 898 | 926 | +28 | still gaining at ~50783s |
| `resume` | 720s | 357 | 369 | +12 | still gaining at ~59872s |
| `extensions` | 900s | 425 | 432 | +7 | ~36540s of 90000s |
| `http` | 120s | 566 | 566 | 0 | — |
| `bencode` | 120s | 389 | 389 | 0 | — |
| `metainfo` | 120s | 456 | 456 | 0 | — |
| `json` | 120s | 735 | 735 | 0 | — |
| `wire` | 120s | 337 | 337 | 0 | — |
| `magnet` | 120s | 498 | 498 | 0 | — |

Read straight off the table, `dest` is a budget twelve times too small: it
plateaued at ~1506s against the 120s it is given. That reading is wrong, and it
is worth naming because the "1x budget" convention below is what invites it.

**A plateau time is the cost of *discovering* those edges against the corpus
that run began from — and the same run's `--seed` pass then banks them.** `dest`
started at 366 edges; the seed that run packed replays at 436. So ~1506s says a
120s budget could not have found all seventy in one sitting. It does not say
the next run needs 1506s, because the next run does not start where that one
did. **The figure expires with the corpus that produced it.**

The two that never plateaued make the same point from the other side. `resume`
was still gaining at 83x its budget and `tracker` at 56x. Those are not table
revisions, they are a different activity — nobody schedules fourteen hours per
target per night. What compounds for them is the corpus, which is exactly the
arrangement this file already runs on: a budget only has to keep finding *some*
of the ground each run.

So the table stands, and six targets flat at 100x is the strongest confirmation
the floor rows have had. What the sweep actually bought is the corpus it packed
— and two things nobody was looking for, which are written up under Input
length above and `wire` below rather than here, because neither is about time.

#### The `tracker` verdict was wrong

The previous revision recorded `tracker` returning **+1 edge for 2.2 billion
executions**, and drew a lesson from it: a target that is genuinely still
climbing can be finished by the next sweep. The first half of that stands. The
second half was a verdict on the target, and this sweep refutes it — `tracker`
gained +28 and was still gaining at the end of 90 000 seconds. Across four
sweeps it has gone 689 → 883 → 898 → 926 and has never once stopped.

The 900s it was given is still the right number, for a reason this file did not
say. Not "there is nothing left" but "what is left arrives slower than any
budget anybody would schedule". A plateau always meant saturated *given this
corpus and these mutators*, and the corpus changes every run by design — so
"finished" was never a claim the evidence could support, about this target or
any other. `wire` is the standing warning for the same sentence read the other
way.

### The first crash, and what it was not

The 08:38Z sweep produced this fuzzer's first failure: `resume`, an
`oom-` artifact, 264 bytes, with the run reported as `FAIL`. It was not a bug
in `Resume::decode`, and the chain that establishes that is worth keeping
because the next OOM will look identical:

- The named input **replays in 1 ms** on a fresh process. Nothing grows.
- No allocation anywhere in the target exceeds 64 MiB, under
  `-malloc_limit_mb=64` across the whole corpus and a 300s run.
- The same target built with `--sanitizer none` does **12M executions flat at
  31 MiB**.
- Under ASan the process grows monotonically with executions — 53 MiB at
  `INITED`, 553 at 2M, 702 at 5.4M — and crosses 2048 MiB somewhere near 90M.

So the growth is the sanitizer's allocator, which does not return freed pages,
and it will cross *any* fixed ceiling given a long enough run. `resume` reached
it first because it decodes, re-encodes and re-decodes on every execution,
which is the most allocation per unit of any target here.

Two things follow. The RSS limit now scales with `--scale`, so a long campaign
is not a guaranteed false crash while a genuine runaway — which blows any of
these limits in a single execution — is still caught. And the report now names
the artifact's *kind*: an `oom-` carries a note saying to check that it
reproduces standalone before treating it as a parser bug, because libFuzzer's
RSS watchdog names whichever unit was executing when the ceiling was crossed,
not the unit that grew anything.

libFuzzer's own answer to this is `-fork=1`, which runs batches in child
processes so RSS resets per job. It was measured here and it works. It also
replaces every progress line with a format carrying no `INITED` line and no
`stat::` block — which is the entire input to the report's results section — so
adopting it means rewriting that parsing. Worth doing; not worth doing
silently.

Worth doing *sooner* than that phrasing suggests, because the mitigation has a
ceiling and the ceiling has now been seen. `RSS_LIMIT` is `2048 * SCALE` capped
at 16 GiB, so it stops scaling at `--scale 8`; past there a longer campaign
gets no more headroom. `resume` peaked at **7087 MiB** in the 2026-08-16 sweep
at scale 100 — 43% of a cap that cannot rise. Extrapolating the growth figures
above through that run's 431M executions puts the crossing somewhere near a
billion, which is roughly `--scale 250`. The false OOM is not fixed, it is
deferred, and `-fork=1` is the thing that retires it.

The same run exposed a second thing, which is that the report had never
actually been read on a failure. It printed `FAIL resume (exit ?)`: the target
runs in a subshell that writes its exit status to a file afterwards, and under
`set -e` a non-zero exit aborted that subshell before the write. The one line
that had to survive a failure was the line the failure skipped, and it took the
first real failure in the fuzzer's history to notice. It records the code now.

### The seed round-tripped exactly

The other thing this sweep settles is whether the corpus survives being packed.
Its `INITED` coverage — what libFuzzer reports after replaying the seeds, before
mutating anything — was the previous sweep's *final* figure in all nine targets:
389, 456, 353, 735, 560, 337, 689, 425, 498. Nothing was lost to `cmin`, the
repack, or the round trip through git, and the two-sweeps-from-identical-ground
problem that prompted the last revision is closed.

It is worth checking every time rather than assuming, because `cmin` rewrites
the corpus rather than trimming it: this one dropped between 17% and 57% of the
previous seed's files per target and replaced them with new ones. So
`ci/fuzz-seed.sh` now replays the packed seed with `-runs=0` and prints the
coverage each target starts from, and `ci/fuzz.sh` reports coverage as
`cov 689 -> 882` so the same check on the next sweep is a glance rather than
arithmetic across two reports.

One caveat on reading those figures across time: an edge count is a property of
the corpus **and** the code, so a target's seed coverage moves when either
does. `resume` sat at 358 when the corpus was packed and reads 357 against
today's `main`, having lost an edge to a change in `resume.rs` rather than to
anything about the corpus. A figure that drops without the corpus changing is a
question for `git log`, not a corpus regression.

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
      note: +2 edges, all of them by ~300s of 3360s (1x budget: 420s) — the rest bought nothing
```

libFuzzer prints `cov:` on every progress line, so the first line carrying the
run's final figure is the moment it stopped learning; everything after it is
budget that demonstrably bought nothing. Past the halfway mark the note reads
the other way instead — still gaining at ~*n*s, worth more time — so a budget
that is genuinely too small says so in the same sentence.

A total on its own can say neither, and got it exactly backwards once: the
script called `magnet` "still climbing, worth more time" on +96 edges, and
eight times the budget returned +96 again.

In seconds, and against the 1x budget, because the first version of this note
said "by 12% of the run" and that cannot be transcribed into a table written in
seconds — least of all from a scaled run, where it is 12% of eight times the
budget. `tracker` at "still gaining at 76% of the run" was asking for somewhere
between 901 and 7200 seconds. The same reading as *~5470s, six times the 900s
budget* is the number `budget_for` wants, and revising the table is now
transcription rather than arithmetic.

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
was provisional for one that can, pending a report; the report came, and the
rewritten target reached 337 from the seed and stayed there through 14.5M
executions at eight times that budget. So 120s is now a measured floor, on the
same footing as the other five — and carrying the same caveat, which this
target is the reason for.

What it does not yet cover is a *session*: `Message::parse` feeding
`on_message` against a real `Torrent`, where the state machine's bugs live.
This target reaches the codec, not the protocol.

### `extensions`, and what a second is worth

Budgets are written in seconds, but what a target does with a second is not a
constant, and one target is a long way off the rest. In the 2026-07-30T11:37Z
sweep:

| Target | Execs/s | Mean input | Execs for 3360s |
|---|---:|---:|---:|
| `json` | 59 627 | 131 B | 114.5M in 1920s |
| `tracker` | 40 585 | 116 B | 292.3M in 7200s |
| `magnet` | 14 219 | 391 B | 27.3M in 1920s |
| `extensions` | **1 914** | 322 B | **6.4M in 3360s** |

`extensions` runs at a thirtieth of `json`'s rate on inputs of comparable size,
so its seconds buy a fortieth of the crash search — which is the *only* thing
they buy once its coverage has plateaued, as it now has. It parses the same
input three ways (`pex::PexMessage::parse`, `metadata::MetadataMessage::parse`,
`extension::Handshake::parse`), and none of the three reads like ~500µs of work
on 322 bytes.

That was left as "wants measuring". It has now been measured, over both the
committed seed and the corpus the 2026-08-16 sweep packed, release build,
per-parser, no sanitizer:

| | Execs/s | Per exec |
|---|---:|---:|
| `extensions`, all three parsers | 39 045 | 25.6 µs |
| `pex::PexMessage::parse` alone | 105 826 | 9.4 µs |
| `metadata::MetadataMessage::parse` alone | 104 914 | 9.5 µs |
| `extension::Handshake::parse` alone | 101 984 | 9.8 µs |
| **`bencode::decode` alone, same corpus** | **105 168** | **9.5 µs** |
| `bencode` target, its own corpus | 88 816 | 11.3 µs |
| `json` target, its own corpus | 358 134 | 2.8 µs |

The answer is dull, which is why guessing would not have found it. **Each of
the three parsers costs exactly one `bencode::decode` of the input, and the
extension-specific work on top does not register** — under 5% of the total, at
the edge of the measurement. There is no hot spot. Per byte the decode costs
what `bencode`'s own corpus costs; what differs is that this target decodes the
same input three times, on a corpus whose mean input is around three times the
size of `bencode`'s or `json`'s.

That accounts for a factor of nine. The sweeps show thirty to thirty-six, so
the rest is ASan amplifying allocation volume — three `Value` trees of
`BTreeMap`s and `Vec`s per execution, on an allocator that does not give pages
back.

The useful conclusion is not about the budget. It is that **`extensions` spends
over 95% of its time inside a parser that already has its own target, saturated
at 389 edges since three sweeps ago.** Its seconds are mostly re-fuzzing
`bencode`. Making them buy extension-specific coverage is a question about how
often a valid bencode dict reaches the three parsers at all — the `magnet`
dictionary lesson, one target over — and not one more second of budget.

## Corpus

A seed corpus is committed as `fuzz/seed-corpus.tar.gz`, unpacked over
`fuzz/corpus/` before a run by both `ci/fuzz.sh` and CI. Loose corpus files
stay git-ignored; only the tarball is tracked.

One tarball rather than thousands of files on purpose. The content is about a
megabyte either way, but as loose files it is thousands of git objects in
every clone, for inputs nobody is going to review individually. As a tarball
it is one line in a diff. `ci/fuzz-seed.sh` packs it deterministically — sorted
names, zeroed mtimes, fixed owner, fixed **mode**, `gzip -n` — so it shows up in
`git status` when its contents changed and not merely because it was rebuilt.

The mode is on that list as of this revision, and it is the one that had been
missed. `cmin` writes the files it keeps, so their permissions come from the
operator's umask — 0664 under 002, 0644 under 022 — and tar records them, so
the same corpus packed by two people produced two different tarballs. It was
found by repacking the 2026-07-30T11:37Z seed on a second machine: identical
5767 files, identical contents, different bytes.

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

| | `bencode` | `metainfo` | `resume` | `json` | `http` | `wire` | `tracker` | `extensions` | `magnet` | `dest` |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Sweep reached | 389 | 456 | 369 | 735 | 566 | 337 | 926 | 432 | 498 | 436 |
| Committed seed reaches | 389 | 456 | 369 | 735 | 566 | 337 | 926 | 432 | 498 | 436 |

(The 2026-08-16 sweep. Ten targets now, and the two rows agree in all ten.)

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

What the tarball holds now is the 2026-08-16 sweep's own minimised corpus —
6451 files, 451 KiB — which carries that sweep's gains in `resume`, `tracker`,
`extensions` and `dest`, and is what the row above was measured on.

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
