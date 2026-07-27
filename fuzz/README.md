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

cargo +nightly fuzz list
cargo +nightly fuzz run bencode
cargo +nightly fuzz run metainfo -- -max_total_time=600
```

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

## Corpus

No corpus is committed. Each target's seeds are cheap to regenerate — the
sweep in `hostile.rs` builds valid samples of every format, and cargo-fuzz
finds structure quickly from empty. If a corpus becomes worth keeping, put it
in `fuzz/corpus/<target>/` (git-ignored today) and revisit that decision.
