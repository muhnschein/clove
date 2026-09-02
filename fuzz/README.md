# Fuzzing clove's parsers

`docs/SCOPE.md` §9 asks for fuzzing of every parser. Two layers, complementary:

| | where | toolchain | when |
|---|---|---|---|
| **Sweep** | `crates/clove-core/tests/hostile.rs` | stable | every push, seconds |
| **Fuzz** | this directory | nightly + cargo-fuzz | manual / scheduled, hours |

The sweep is a deterministic mutation pass that runs in normal CI, so parsers
get adversarial input on every commit with no special setup and no flakes. The
targets here are coverage-guided and go far deeper, but need a nightly
toolchain, so they run out of band.

This crate is **excluded from the workspace** (see the root `Cargo.toml`), so
`libfuzzer-sys` never enters the shipped build, the dependency budget, or the
workspace `Cargo.lock` that `ci/check-net-deps.sh` gates. Being outside it,
these targets are not compiled by `cargo test --workspace`, so CI runs `cargo
check --manifest-path fuzz/Cargo.toml` on every push — without that, a changed
signature in `clove-core` rots a target silently and the nightly job is the
first to notice, a day later. Building needs no nightly; only *running* does.

## Running

```sh
cargo install cargo-fuzz            # once
rustup toolchain install nightly    # once

make fuzz-all                       # every target, ~20 min, writes a report
make fuzz-all QUICK=1               # ~2 min, "does it still build and run"
make fuzz-all SCALE=8               # a long hunt
make fuzz-all SEED=1                # and keep what it finds (see Corpus)
make fuzz TARGET=metainfo SECS=600  # one target, straight through

make fuzz-coverage                  # which functions the seed actually reaches
make fuzz-sanitizer-ab              # what ASan costs and buys, per target
```

Budgets total 3360s, about twenty minutes of wall clock on four cores:
`ci/fuzz.sh` keeps `nproc - 1` targets running and starts the longest first,
refilling a slot as soon as one frees rather than waiting for a batch.

The report — `fuzz/report-<stamp>.txt` — is the deliverable. It carries the
commit and tree it ran against, the toolchain, per-target executions and
coverage, and, if anything crashed, the input **base64-encoded** with the
commands to reproduce and minimise it: "it crashed on metainfo" is not
something anyone can act on from somewhere else.

Coverage reads `cov 366 -> 436 (+70 this run)`, the final figure minus
libFuzzer's `INITED` line — deliberately not `new_units_added`, since a unit is
kept for reaching a new execution-count *bucket* rather than a new edge. One
sweep added 3262 `json` units without reaching a single new edge.

A crash is written to `fuzz/artifacts/<target>/`:

```sh
cargo +nightly fuzz run bencode fuzz/artifacts/bencode/crash-<hash>
cargo +nightly fuzz tmin bencode fuzz/artifacts/bencode/crash-<hash>
```

Reproduce on a fresh process before believing it — an `oom-` artifact usually
is not a bug, see Target notes. Then add the minimised input to the module's
own tests, which is where it belongs permanently: the fuzzer finds a bug once,
a unit test keeps it dead.

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
| `wire` | a peer's whole byte stream: framing, messages, handshake | a message survives its own encoder unchanged; a reused frame buffer reads what a fresh one does |
| `tracker` | announce responses | a hostile `interval` cannot make us announce inside the local floor |
| `extensions` | `i2p_pex`, `ut_metadata`, BEP 10 handshake | the PEX peer cap holds |
| `magnet` | magnet URIs | non-I2P trackers are filtered out |
| `dest` | SAM base64 destinations and b32 labels | both codecs round trip; a truncated destination is still a whole destination |
| `text` | every foreign string on its way to a terminal | no control, bidi, zero-width or separator character survives `scrub`; length in characters is preserved; scrubbing is idempotent |

The properties matter as much as the crashes: a parser that accepts a torrent
whose paths escape the download directory has not crashed, and is still a
serious bug.

### Is this still the whole surface?

Worth re-asking whenever the code has moved, because a target set is only
current until someone adds a parser. The public parse entry points, checked
against `main` on 2026-08-18:

`bencode::{decode, decode_prefix}`, `bitfield::Bitfield::from_bytes`,
`config::{parse, parse_seed_ratio}`, `extension::Handshake::parse`,
`http::{read_response, read_request}`, `json::parse`, `magnet::parse`,
`metadata::MetadataMessage::parse`, `metainfo::MetaInfo::{parse,
from_info_dict}`, `metainfo::TrackerUrl::parse`, `pex::PexMessage::parse`,
`resume::Resume::decode`, `tracker::parse_response`,
`wire::{Message::parse, Handshake::parse, read_frame, read_frame_into}`, and in
`i2pnet`, `addr::{destination_bytes, destination_len, base32_decode,
base32_encode, i2p_base64_decode, i2p_base64_encode}` with
`DestHash::{from_b32, from_b64_destination}`.

`TrackerUrl::parse` is not a target of its own but is not uncovered either:
`MetaInfo::parse` and `magnet::parse` both filter trackers through it, which is
what the "surviving trackers are I2P URLs" property in the table exercises.

The `cargo check` gate keeps signatures honest, but it cannot catch production
moving to a *new* function and leaving the old one behind — still compiling,
still fuzzed, no longer what the daemon runs. That is what happened to `wire`;
this list is the thing that catches it, so re-derive it rather than trusting it.

Three gaps are known and left open:

- **`bitfield::Bitfield::from_bytes`.** Reached by no target, and its input is
  untrusted twice over: `Torrent` hands it the bytes of a peer's `Bitfield`
  message, and the daemon's registry hands it a resume file's. The `wire`
  target parses the message that carries those bytes but never builds a
  `Bitfield` from them. It is a length check, a spare-bits check and a copy —
  small enough that the unit tests plausibly cover it, which is the argument
  for leaving it rather than a reason it is safe.
- **`config::parse`.** A parser, and uncovered. Its input is a file the
  operator writes, which puts it below everything above on the list of things
  an attacker reaches.
- **The SAM line protocol** — `read_sam_line`, `parse_session_status`,
  `read_dest_line` in `crates/i2pnet/src/sam.rs`. Genuinely untrusted, since
  they read the router's socket, but private, so a target needs a seam in
  production code. `dest` covers the parsing they delegate to; the line framing
  around it is not covered.

## Dictionaries

`fuzz/dicts/<target>.dict` lists the tokens a parser actually looks for —
bencode keys with their length prefixes, `magnet:?xt=urn:btih:`, HTTP framing,
wire message headers. `ci/fuzz.sh` and CI pass the file whenever one exists,
and the mutator can then *insert* a token instead of arriving at it byte by
byte.

libFuzzer recovers some unaided — a `strip_prefix("magnet:?")` compiles to a
`memcmp` the sanitizer intercepts, and the literal goes into an auto-dictionary
— but a bencode key lookup walks bytes in a loop, which the interception never
sees. So the win is uneven and was measured rather than assumed: 120s per arm
from the committed seed, three RNG seeds each.

| Target | Edges gained, no dict | Edges gained, dict |
|---|---:|---:|
| `magnet` | +1, +1, +1 | +191, +187, +191 |
| `extensions` | +21, +19, +0 | +113, +114, +102 |

The `magnet` row is the one to look at twice. Without a dictionary that target
had, in effect, stopped: one edge in two minutes, every time, on 561 files that
between them never produced a valid `magnet:?` prefix under mutation. Not a
marginal speedup — the difference between fuzzing the parser and idling in
front of it.

The other dictionaries were written on the same reasoning rather than on
per-target evidence, and most moved a target the previous reports had called
saturated.

## Input length

`-max_len` is passed explicitly, from `max_len_for` in `ci/fuzz.sh`: **4096 for
every target except `extensions`, which gets 20480.**

It is set explicitly because the default is not "no ceiling". With the flag
unset libFuzzer uses the larger of 4096 and the biggest file in the seed, and
every corpus here had sat just under 4096 for its whole history — 4058 bytes
for `extensions`, 4018 for `wire`, 4003 for `magnet`, 3757 for `json`. That is
not a fact about the inputs. It is the ceiling printing its own shape: the
mutator cannot cross 4096, so a corpus climbs toward it and stops. Pinning the
value also stops one large file arriving in a corpus from quietly raising the
limit for everything else.

4 KiB is the right ceiling for nine targets. It was wrong for `extensions`,
which has two rejection branches gated above it — `pex::MAX_PEX_PEERS` needs
512 × 32 bytes of hashes, and `metadata::METADATA_PIECE_LEN` needs a 16 KiB
piece, so both want ~16.4 KiB, four times what the mutator could produce. So
`Error::PieceTooLarge` was unreachable, and so was the target's own

```rust
assert!(message.added.len() + message.dropped.len() <= pex::MAX_PEX_PEERS);
```

— the PEX cap the Targets table credits it with asserting. It could not have
failed at any budget, on any corpus, in any run.

Both bounds have unit tests, so what was wrong was the coverage this file
*claimed*, not the parser. That is the distinction worth keeping: **a property
nothing can trip is indistinguishable from a property that holds, and the
report prints `ok` either way.** Raising the ceiling is not sufficient on its
own either — mutation will not arrive at a 16 KiB length-prefixed bencode
string by chance — so the seed carries one input per branch, generated against
the parsers and asserted to land on the error it exists for. They are the two
largest files in the corpus by a factor of four.

Confirmed by measurement since: replaying the committed seed puts `extensions`
at **437 edges against the 432 the sweep recorded**, while `json` reads 735
both times. The two crafted inputs are the only difference between those
corpora, so the five edges are the two branches that nothing could reach
before.

## Budgets

Not flat, and set from what the reports measured rather than from taste. The
table lives in exactly one place, `budget_for` in `ci/fuzz.sh`; CI and the
Makefile ask for it (`./ci/fuzz.sh --budget magnet`, `--max-len magnet`) rather
than keeping copies. Revising a budget is one edit.

| Target | Budget | At 100x the budget | Reading |
|---|---:|---|---|
| `bencode` `metainfo` `json` `http` `wire` `magnet` | 120s each | flat, as at 8x and 80x | floor |
| `dest` | 120s | +70 edges, all by ~1506s | gains now in the seed |
| `resume` | 720s | +12, still gaining at 83x | corpus carries it |
| `tracker` | 900s | +28, still gaining at 56x | corpus carries it |
| `extensions` | 900s | +7, all by ~36540s | see Target notes |

Total 3360s across ten targets.

The floor is a choice, and not a measured plateau: once the seed reaches a
target's ceiling no budget buys another edge, so no report can say how few
seconds would do. What the seconds still buy there is crash search, which
coverage cannot price. 120s is small enough to be cheap and long enough to be a
real hunt.

None of the raised budgets reaches its plateau and none is meant to — `resume`
and `tracker` were still climbing at 83x and 56x, which is not a budget anybody
schedules. What carries a gain between runs is the seed corpus. A budget only
has to keep finding *some* of the ground each run for the corpus to accumulate
the rest.

### Reading a run for what it says about time

`ci/fuzz.sh` reports the point at which coverage stopped growing:

```
      note: +2 edges, all of them by ~300s of 3360s (1x budget: 420s) — the rest bought nothing
```

libFuzzer prints `cov:` on every progress line, so the first line carrying the
run's final figure is the moment it stopped learning; everything after it is
budget that demonstrably bought nothing. Past the halfway mark the note reads
the other way — still gaining at ~*n*s, worth more time — so a budget that is
genuinely too small says so in the same sentence. In seconds and against the 1x
budget rather than as a percentage of the run, so it transcribes into the table
above without arithmetic across a scale factor.

Three rules for reading that note, each of which this table once got wrong:

- **A plateau means saturated given this corpus and these mutators, never
  "finished".** `tracker` returned +1 edge for 2.2 billion executions in one
  sweep and +28 in the next; across four sweeps it has gone 689 → 883 → 898 →
  926 and has never once stopped. `wire` sat flat for three sweeps because the
  target was too narrow, not because the codec was clean.
- **Budget from the margin, not the gain.** A budget raised because a target
  gained a lot last time is always a sweep behind; what matters is whether it
  was still gaining at the *end*. The script once called `magnet` worth more
  time on +96 edges, and eight times the budget returned +96 again.
- **A plateau time expires with the corpus that produced it.** It measures what
  *discovering* those edges cost against the corpus the run began from, and
  that run's `--seed` pass then banks them. `dest` plateaued at ~1506s against
  a 120s budget, which reads as twelve times too small; it is not, because the
  next run starts at 436 edges rather than the 366 that one did.

## Coverage

`make fuzz-coverage` replays the committed seed under instrumentation and
reports, per target, how much of its subject the corpus reaches — and which
functions it has **never entered**.

That last list is the point of the exercise. A sweep reports edge counts, which
say how much a run learned; they say nothing about how much of the parser the
target can reach *at all*. Both of this fuzzer's real failures were of the
second kind — `wire` plateaued for three sweeps with `read_frame` unfuzzed, and
the `extensions` PEX cap could not be tripped at any budget — and both were
caught by somebody happening to look. A function listed as never entered is the
same problem, found on purpose.

A low percentage is not by itself wrong: error paths, `Display` impls and
encode-side helpers are regions too, and some are unreachable from a target by
design. Read the never-entered list, not the total.

Needs `llvm-tools-preview` on the nightly toolchain, and nothing else —
`llvm-cov` is found beside the toolchain rather than on `PATH`. It does *not*
need cargo-binutils; the first version of this script probed for `cargo cov`,
which is that crate's shim, and so reported a missing rustup component to an
operator who already had it. `rustfilt` is optional and only makes the symbol
names legible.

## Sanitizer

Runs default to AddressSanitizer, which is cargo-fuzz's default. `ci/fuzz.sh
--sanitizer none` switches it, and the report header records which was used.

Whether `address` is the right default *here* is an open question with numbers
on both sides, and `make fuzz-sanitizer-ab` is what settles it. Every crate in
this workspace sets `unsafe_code = "forbid"`, so the bug class ASan exists to
find cannot occur in the code under test. Against keeping it:

- it costs roughly 4x throughput — the `extensions` measurement puts that
  target at 9x slower than `json` without a sanitizer against 30-36x with;
- it is the cause of the only false crash this fuzzer has produced. The RSS
  growth behind that `oom-` artifact is ASan's allocator, so dropping it
  *retires* the problem that `-rss_limit_mb` only defers.

For keeping it:

- it intercepts `memcmp`, which is where libFuzzer's *auto*-dictionary comes
  from. The ten hand-written dictionaries cover some of that ground; how much
  is exactly what the A/B measures;
- dependencies do contain unsafe code, even if the parsers barely reach it.

The A/B gives each arm equal wall clock, several RNG seeds, and a pristine
corpus for every run. Two things the first run of it settled:

**Throughput, measured.** Dropping the sanitizer is worth **3.0x on
`extensions` and 2.3x on `json`** — 7.8M executions against 2.6M, and 79M
against 35M, over 3 x 120s per arm.

**Start cold, or there is nothing to measure.** Run from the committed seed,
both arms gained 0 edges in every single run: the seed already sits at each
target's ceiling, so neither arm had ground left to find and the coverage half
of the question got no signal at all. Runs now start from an empty corpus,
where how much of the corpus an arm rediscovers per second is exactly the
question. `--warm` keeps the old behaviour for the different question of
whether more budget helps from where we already are.

Scores are a fraction of each arm's *own* ceiling, never a raw edge count.
The instrumentation gap is far too large to compare directly: the same `json`
corpus reads **735 edges under `address` and 474 under `none`**, because ASan's
inlined shadow checks are instrumented too. A ratio of like-for-like figures is
comparable where the figures themselves are not.

Neither column prices crash search, which is what more executions actually buy,
nor memory errors in dependencies, which is what the sanitizer actually returns.

## Corpus

Committed as `fuzz/seed-corpus.tar.gz` and unpacked over `fuzz/corpus/` before
a run by both `ci/fuzz.sh` and CI. Loose corpus files stay git-ignored; only
the tarball is tracked. It currently holds 6453 files, 451 KiB.

One tarball rather than thousands of files: the content is about a megabyte
either way, but as loose files it is thousands of git objects in every clone,
for inputs nobody will review individually. `ci/fuzz-seed.sh` packs it
deterministically — sorted names, zeroed mtimes, fixed owner, fixed **mode**,
`gzip -n` — so it shows up in `git status` when its contents changed rather
than merely because it was rebuilt. Mode is on that list because `cmin` writes
the files it keeps: without it the operator's umask lands in the archive and
two people packing an identical corpus produce different tarballs.

Seeding is what stops a run spending its budget rediscovering that input has to
be bencode at all — fuzzing the question "is this a torrent" rather than the
parser behind it.

**A corpus is an asset only while it is minimised.** libFuzzer keeps an input
for reaching a new execution-count bucket, not a new edge, so a corpus grows
without the coverage growing with it; one scale-8 sweep ended with 25 003 files
that `cmin` took to 5 173 at identical coverage. That bloat is charged to the
next run twice — startup replays all of it, and mutation energy is split across
all of it. On `magnet` the cost once exceeded the benefit of seeding entirely:
120s from a 561-file seed reached 308 edges where 120s from an *empty*
directory reached 498. Seeding is not free, and `cmin` is not housekeeping.

**What is committed is checked, not assumed.** `ci/fuzz-seed.sh` replays the
packed seed with `-runs=0` and prints the coverage each target starts from, so
it can be compared against what the run reached. For the current seed the two
agree in all ten targets:

| `bencode` | `metainfo` | `resume` | `json` | `http` | `wire` | `tracker` | `extensions` | `magnet` | `dest` |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 389 | 456 | 369 | 735 | 566 | 337 | 926 | 432 | 498 | 436 |

Worth running every time, because the alternative is silent: `cmin` rewrites a
corpus rather than trimming it, and two sweeps once began from *identical*
coverage because the second never received the first's findings — 26 880
seconds spent re-deriving covered ground. Note that an edge count is a property
of the corpus **and** the code, so a figure that drops without the corpus
changing is a question for `git log`, not a corpus regression.

Two files in `extensions` are the only inputs here a run did not find, because
no run *could* — see Input length. If a replay ever shows `extensions` starting
below 432, check whether a `cmin` dropped them.

So fold runs back in rather than letting `fuzz/corpus/` accumulate until it
dies with the working tree:

```sh
make fuzz-all SEED=1   # sweep, then minimise and repack in one go
make fuzz-seed         # or fold in whatever the corpus has grown since
```

In CI the scheduled job caches each target's corpus and restores it next time,
`cmin`s *before* the cache is written so the compounding is in coverage rather
than file count, and uploads the minimised corpus as an artifact kept 30 days —
which is what somebody folds into the committed seed with `make fuzz-seed`.

## Target notes

### `wire`

A peer connection is a handshake and then a *stream*, and this target used to
parse one body in isolation: 168 edges, unmoved across five runs and 239
million executions, with most of `wire.dict` — length-prefixed framing that a
body parser can only reject — talking past it. Reading a stream of frames
instead took it to 337 from the same corpus.

There are two framing entry points and the target drives both. `read_frame`
allocates a buffer per frame; `read_frame_into` fills a caller-owned one, which
is what `Torrent`'s peer read loop uses, because allocating 16 KiB per block
per peer across hundreds of threads keeps pages the heap never returns. When
production moved to the second, the target did not follow, and this file spent a
while reporting 337 saturated edges over a function the daemon no longer ran on
its hot path.

A reused buffer is also where the interesting failure lives: carry the tail of a
longer previous frame into a shorter one and the result is a message the peer
never sent, with nothing out of bounds and nothing to panic about. So the target
now reads the same bytes through both entry points in lockstep, a cursor each,
and requires them to agree frame for frame.

Still uncovered: a *session*. `Message::parse` feeding `on_message` against a
real `Torrent` is where the state machine's bugs live. This target reaches the
codec, not the protocol.

### `extensions`

The slowest target by a wide margin — 1761 execs/s against `json`'s 62 872 in
the same sweep — and the reason is measured rather than guessed. Timed
per-parser over the committed corpus, release build, no sanitizer:

| | Per exec |
|---|---:|
| all three parsers | 25.6 µs |
| `pex::PexMessage::parse` alone | 9.4 µs |
| `metadata::MetadataMessage::parse` alone | 9.5 µs |
| `extension::Handshake::parse` alone | 9.8 µs |
| **`bencode::decode` alone, same corpus** | **9.5 µs** |

**Each of the three parsers costs exactly one `bencode::decode`, and the
extension-specific work on top does not register** — under 5%, at the edge of
the measurement. There is no hot spot: the target decodes the same input three
times, on a corpus whose mean input is around three times `bencode`'s. That is
a factor of nine; the sweeps show thirty-odd, and the rest is ASan amplifying
three `Value` trees per execution on an allocator that does not give pages back.

So `extensions` spends over 95% of its seconds inside a parser that already has
its own target, saturated at 389 edges. Buying it extension-specific coverage
is a question about how often a valid bencode dict reaches the three parsers —
the `magnet` dictionary lesson, one target over — not about more budget.

### `dest`

The one parser here whose mis-reading has already cost something. A destination
and a *private key blob* are both one long base64 run, separated by a
length-prefixed certificate header 384 bytes in; before `destination_len`
existed nobody looked, and every announce carried our private keys in the `ip`
parameter until postman's tracker refused them (`crates/i2pnet/src/addr.rs`
carries the account).

The arithmetic telling the two apart reads an attacker-supplied 16-bit length
and slices with it, which is the shape a fuzzer exists for, so the target
asserts what that incident violated: a blob truncated to its destination is
itself a whole destination, the length never runs past the input, and
`destination_bytes` and `from_b64_destination` agree about whether the input
was a destination at all. The two codecs around it are asserted as round trips
in the direction we control — a codec that loses a byte corrupts a peer
identity rather than failing.

### An `oom-` artifact is usually not a bug

The first failure this fuzzer ever produced was an `oom-` on `resume` that did
not reproduce. The chain that establishes that is worth repeating, because the
next one will look identical:

- The named input **replays in 1 ms** on a fresh process. Nothing grows.
- No allocation in the target exceeds 64 MiB under `-malloc_limit_mb=64`.
- Built with `--sanitizer none`, the target does 12M executions flat at 31 MiB.
- Under ASan the process grows monotonically with executions — 53 MiB at
  `INITED`, 553 at 2M, 702 at 5.4M — crossing 2048 MiB near 90M.

The growth is the sanitizer's allocator, which does not return freed pages, and
it will cross *any* fixed ceiling given a long enough run. libFuzzer's RSS
watchdog then names whichever unit was executing at the crossing, not the unit
that grew anything. `resume` hits it first because it decodes, re-encodes and
re-decodes on every execution.

`RSS_LIMIT` is therefore `2048 * SCALE`, so a long campaign is not a guaranteed
false crash while a genuine runaway — which blows any limit in one execution —
is still caught. But it is capped at 16 GiB, so it stops scaling at `--scale 8`,
and `resume` has already reached 7087 MiB at `--scale 100`: 43% of a ceiling
that cannot rise, with the crossing extrapolating to somewhere near `--scale
250`. The false OOM is deferred, not fixed.

`-fork=1` is what retires it, by running batches in child processes so RSS
resets per job. It was measured here and it works. It also replaces every
progress line with a format carrying no `INITED` line and no `stat::` block —
the entire input to the report's results section — so adopting it means
rewriting that parsing. Worth doing; not worth doing silently.
