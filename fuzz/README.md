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

make fuzz-all                       # every target, ~50 min, writes a report
make fuzz-all QUICK=1               # ~5 min, "does it still build and run"
make fuzz-all SCALE=8               # a long hunt
make fuzz TARGET=metainfo SECS=600  # one target, straight through
```

`make fuzz-all` runs `ci/fuzz.sh`, which writes `fuzz/report-<stamp>.txt`. The
report is the deliverable: it carries the commit and tree it ran against, the
toolchain, per-target executions and coverage, and — if anything crashed — the
input **base64-encoded** along with the commands to reproduce and minimise it.
That is deliberate. "It crashed on metainfo" is not something anyone can act
on from somewhere else; a report that contains the failing bytes is.

`ci/fuzz.sh` also flags a target that found no new coverage in its budget, so
the budgets below can be revised from evidence rather than taste.

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

## Budgets

Not flat. Measured over a 300-second run of every target on 2026-07-29:

| Target | Executions | Coverage | Budget |
|---|---:|---:|---:|
| `wire` | 204M | 168 | 120s |
| `bencode` | 5.1M | 387 | 300s |
| `resume` | 4.1M | 283 | 300s |
| `http` | 3.4M | 439 | 300s |
| `magnet` | 6.8M | 308 | 300s |
| `json` | 25M | 732 | 420s |
| `tracker` | 10M | 591 | 420s |
| `metainfo` | 9.0M | 366 | 600s |
| `extensions` | 2.1M | 304 | 600s |

`wire` reached all 168 of its edges within seconds and spent the remaining 299
re-proving them; `extensions` and `metainfo` were still finding new coverage
when the clock ran out. Spending the same on both wastes one and short-changes
the other. The table lives in two places — `ci/fuzz.sh` and the CI matrix — and
they must agree.

## Corpus

A seed corpus is committed as `fuzz/seed-corpus.tar.gz`, unpacked over
`fuzz/corpus/` before a run by both `ci/fuzz.sh` and CI. Loose corpus files
stay git-ignored; only the tarball is tracked.

One tarball rather than thousands of files on purpose. The content is about a
megabyte either way, but as loose files it is thousands of git objects in
every clone, for inputs nobody is going to review individually. As a tarball
it is one line in a diff.

Starting cold is the thing this avoids. An unseeded run spends much of its
budget rediscovering that input has to be bencode at all — which is fuzzing
the question "is this a torrent" rather than the parser behind it. The
scheduled CI job additionally caches what each run *discovers* and restores it
next time, so the corpus compounds; the committed seed is the floor it can
never fall below.

Regenerate the seed after a long sweep has grown the corpus:

```sh
make fuzz-seed      # cmin every target, then repack the tarball
```
