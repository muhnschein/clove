#!/bin/sh
# Fold what the local corpus has grown into the committed seed.
#
#   ./ci/fuzz-seed.sh            # every target
#   ./ci/fuzz-seed.sh magnet     # just one
#
# Two steps, and the first is the one that matters. `cargo fuzz cmin` throws
# away every input that reaches nothing another input already reaches, which is
# most of what a long run accumulates: libFuzzer keeps a file for hitting a new
# execution-count *bucket*, not a new edge, so a corpus grows without the
# coverage growing with it. In the 2026-07-30 sweep `json` finished with 5116
# files at exactly the 732 edges it had 30 seconds in, and `magnet` with 5107.
#
# That bloat is not free. Every run replays the whole corpus at startup and
# then splits its mutation energy across it, so an unminimised corpus makes the
# next run slower *and* shallower — measured on `magnet`, 120s from the 561-file
# committed seed reached 308 edges while the same 120s from an empty directory
# reached 498. A corpus is an asset only while it is minimised.
#
# Then repack. The seed ships as one tarball rather than thousands of loose
# files: the content is ~1 MiB either way, but as files it is thousands of git
# objects in every clone, for inputs nobody can review individually anyway.
set -eu

cd "$(dirname "$0")/.."

TARGETS=${*:-}
if [ -z "$TARGETS" ]; then
    TARGETS=$(cargo +nightly fuzz list 2>/dev/null) || {
        echo "seed: cannot list targets (needs nightly + cargo-fuzz)" >&2
        exit 1
    }
fi

before_total=$(find fuzz/corpus -type f 2>/dev/null | wc -l | tr -d ' ')
# Spelled out rather than `$(wc -c < f || echo 0)`: a failed redirection leaves
# the pipeline's status to `tr`, which succeeds on empty input, so the fallback
# never fires and the arithmetic below divides an empty string.
before_size=0
if [ -f fuzz/seed-corpus.tar.gz ]; then
    before_size=$(wc -c < fuzz/seed-corpus.tar.gz | tr -d ' ')
fi

for t in $TARGETS; do
    printf 'cmin: %-12s' "$t"
    if [ ! -d "fuzz/corpus/$t" ] || [ -z "$(ls -A "fuzz/corpus/$t" 2>/dev/null)" ]; then
        printf 'no corpus\n'
        continue
    fi
    b=$(ls "fuzz/corpus/$t" | wc -l | tr -d ' ')
    if cargo +nightly fuzz cmin "$t" >/dev/null 2>&1; then
        printf '%6s -> %-6s\n' "$b" "$(ls "fuzz/corpus/$t" | wc -l | tr -d ' ')"
    else
        printf '%6s    skipped (cmin failed)\n' "$b"
    fi
done

# Repack deterministically. Without this the tarball's bytes change on every
# run from mtimes and gzip's embedded timestamp alone, so a committed artifact
# shows up in `git status` after a sweep that discovered nothing — and once a
# diff is always there, nobody reads it. With it, the seed changes in git
# exactly when its contents changed.
if tar --help 2>&1 | grep -q -- '--sort'; then
    tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
        -cf - -C fuzz corpus | gzip -9n > fuzz/seed-corpus.tar.gz
else
    # BSD tar has no --sort; the archive is still correct, just not stable
    # byte-for-byte between runs.
    echo "seed: note: GNU tar not found, archive will not be reproducible" >&2
    tar czf fuzz/seed-corpus.tar.gz -C fuzz corpus
fi

after_total=$(find fuzz/corpus -type f 2>/dev/null | wc -l | tr -d ' ')
after_size=$(wc -c < fuzz/seed-corpus.tar.gz | tr -d ' ')

echo "seed: $before_total -> $after_total file(s), \
$(( before_size / 1024 )) -> $(( after_size / 1024 )) KiB in fuzz/seed-corpus.tar.gz"
