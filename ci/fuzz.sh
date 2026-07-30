#!/bin/sh
# Run the coverage-guided fuzz targets and write one report.
#
#   ./ci/fuzz.sh                 # every target, default budgets (~50 min)
#   ./ci/fuzz.sh metainfo        # one target
#   ./ci/fuzz.sh --scale 4       # four times the budget, everything
#   ./ci/fuzz.sh --quick         # a couple of minutes, for "does it still build"
#
# The point of the report is that it can be read by somebody who was not at the
# machine. A crash is no use as "it crashed on metainfo": the file carries the
# input base64-encoded, the panic, and the command to reproduce it, so the bug
# can be diagnosed and turned into a regression test from the report alone.
#
# Everything here needs a nightly toolchain and cargo-fuzz; see fuzz/README.md.
# Nothing in the shipped build depends on either.
set -eu

cd "$(dirname "$0")/.."

TARGETS_ALL='bencode metainfo resume json http wire tracker extensions magnet'
SCALE=1
QUICK=no
TARGETS=''

usage() {
    cat <<'USAGE'
usage: ci/fuzz.sh [options] [target...]

  --scale N     multiply every budget by N (default 1)
  --quick       30s per target, for a smoke check rather than a hunt
  --out PATH    where to write the report (default fuzz/report-<stamp>.txt)
  -h, --help    this

With no targets named, every target runs.
USAGE
}

OUT=''
while [ $# -gt 0 ]; do
    case "$1" in
        --scale) SCALE="${2:?--scale needs a number}"; shift 2 ;;
        --quick) QUICK=yes; shift ;;
        --out) OUT="${2:?--out needs a path}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "fuzz: unknown option $1" >&2; usage >&2; exit 2 ;;
        *) TARGETS="$TARGETS $1"; shift ;;
    esac
done
[ -n "$TARGETS" ] || TARGETS="$TARGETS_ALL"

# Per-target budget in seconds. Deliberately not flat: measured on 2026-07-29,
# `wire` reached all 168 of its edges within seconds and spent the rest of a
# 300-second run re-proving them, while `extensions` was still turning up new
# ones when the clock ran out. Spending the same on both wastes one and
# short-changes the other.
budget_for() {
    case "$1" in
        wire) echo 120 ;;                       # saturates almost immediately
        extensions|metainfo) echo 600 ;;        # still climbing at 300s
        json|tracker) echo 420 ;;               # widest surface after those
        *) echo 300 ;;
    esac
}

for tool in cargo rustc; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "fuzz: $tool is not installed" >&2
        exit 1
    }
done
if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    echo "fuzz: needs a nightly toolchain: rustup toolchain install nightly" >&2
    exit 1
fi
if ! command -v cargo-fuzz >/dev/null 2>&1; then
    echo "fuzz: needs cargo-fuzz: cargo install cargo-fuzz --locked" >&2
    exit 1
fi

# Unpack the committed seed corpus over whatever is already there. Without it
# a cold run spends much of its budget rediscovering that input has to be
# bencode at all, which is fuzzing the question "is this a torrent" rather than
# the parser behind it. `-k` keeps anything the local corpus already has, so a
# run that has grown its own is never set back by seeding.
seed_corpus() {
    [ -f fuzz/seed-corpus.tar.gz ] || return 0
    mkdir -p fuzz/corpus
    tar xzf fuzz/seed-corpus.tar.gz -C fuzz -k 2>/dev/null || true
}

stamp=$(date -u +%Y%m%dT%H%M%SZ)
[ -n "$OUT" ] || OUT="fuzz/report-$stamp.txt"
logs=$(mktemp -d)
trap 'rm -rf "$logs"' EXIT

# One line to the terminal, everything to the report.
say() { printf '%s\n' "$*" | tee -a "$OUT"; }
note() { printf '%s\n' "$*" >> "$OUT"; }

: > "$OUT"
note "clove fuzz report"
note "when      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
note "commit    $(git rev-parse HEAD 2>/dev/null || echo unknown)"
note "tree      $(git rev-parse HEAD^{tree} 2>/dev/null || echo unknown)"
note "dirty     $(if [ -n "$(git status --porcelain 2>/dev/null)" ]; then echo yes; else echo no; fi)"
note "rustc     $(rustc +nightly --version 2>/dev/null || echo unknown)"
note "cargo-fuzz $(cargo-fuzz --version 2>/dev/null || echo unknown)"
note "host      $(uname -srm)"
note "scale     $SCALE${QUICK:+ (quick=$QUICK)}"
note ""

seed_corpus
note "seeded    $(find fuzz/corpus -type f 2>/dev/null | wc -l | tr -d ' ') corpus file(s) before the run"
note ""

say "fuzz: building targets"
if ! cargo +nightly fuzz build >"$logs/build.log" 2>&1; then
    say "fuzz: BUILD FAILED"
    note ""
    note "--- build output ---"
    tail -60 "$logs/build.log" >> "$OUT"
    say "fuzz: report written to $OUT"
    exit 1
fi

# libFuzzer is single-threaded per process; leave a core for everything else.
cores=$(nproc 2>/dev/null || echo 2)
jobs=$((cores - 1))
[ "$jobs" -ge 1 ] || jobs=1

say "fuzz: running on $jobs of $cores core(s)"
running=0
for t in $TARGETS; do
    if [ "$QUICK" = yes ]; then
        secs=30
    else
        secs=$(( $(budget_for "$t") * SCALE ))
    fi
    say "fuzz: $t (${secs}s)"
    (
        cargo +nightly fuzz run "$t" -- \
            -max_total_time="$secs" -print_final_stats=1 \
            >"$logs/$t.log" 2>&1
        echo "$?" > "$logs/$t.status"
    ) &
    running=$((running + 1))
    if [ "$running" -ge "$jobs" ]; then
        wait
        running=0
    fi
done
wait

note "--- results ---"
note ""
fail=0
for t in $TARGETS; do
    status=$(cat "$logs/$t.status" 2>/dev/null || echo '?')
    stat_of() {
        grep -oE "stat::$1: *[0-9]+" "$logs/$t.log" 2>/dev/null |
            tail -1 | grep -oE '[0-9]+$'
    }
    execs=$(stat_of number_of_executed_units)
    per_sec=$(stat_of average_exec_per_sec)
    added=$(stat_of new_units_added)
    rss=$(stat_of peak_rss_mb)
    cov=$(grep -oE 'cov: [0-9]+' "$logs/$t.log" 2>/dev/null | tail -1 | grep -oE '[0-9]+$')
    corpus=$(ls "fuzz/corpus/$t" 2>/dev/null | wc -l | tr -d ' ')

    if [ "$status" = 0 ]; then
        note "ok    $t"
    else
        note "FAIL  $t  (exit $status)"
        fail=1
    fi
    note "      execs ${execs:-?}  (${per_sec:-?}/s)  cov ${cov:-?}  new-units ${added:-?}"
    note "      corpus ${corpus:-0} files  peak-rss ${rss:-?} MiB"

    # A target that stops finding new coverage has spent its budget; one that
    # is still adding is worth more time. Saying so here is what lets the
    # budgets above be revised from evidence rather than taste.
    if [ -n "${added:-}" ] && [ "$added" -eq 0 ] 2>/dev/null; then
        note "      note: found nothing new — this budget is more than it needs"
    fi
    note ""
done

note "--- crashes ---"
note ""
artifacts=$(find fuzz/artifacts -type f 2>/dev/null | sort || true)
if [ -z "$artifacts" ]; then
    note "none"
else
    for a in $artifacts; do
        t=$(basename "$(dirname "$a")")
        note "target    $t"
        note "artifact  $a"
        note "size      $(wc -c < "$a" | tr -d ' ') bytes"
        note "reproduce cargo +nightly fuzz run $t $a"
        note "minimise  cargo +nightly fuzz tmin $t $a"
        note ""
        note "input (base64, so the report is enough to reconstruct it):"
        base64 < "$a" >> "$OUT"
        note ""
        note "panic:"
        # From the panic line onward, not a grep for it: the *message* — which
        # is the one line that says what invariant broke — is printed on the
        # line after "panicked at", and a pattern match on the panic line alone
        # silently drops it. Found by making this script crash on purpose and
        # reading what it produced.
        if grep -q "panicked at" "$logs/$t.log" 2>/dev/null; then
            sed -n '/panicked at/,$p' "$logs/$t.log" | head -30 >> "$OUT"
        else
            # A sanitizer report or an abort has no Rust panic line; the tail
            # is the best available.
            tail -30 "$logs/$t.log" >> "$OUT" 2>/dev/null || true
        fi
        note ""
    done
fi

note "--- what to do with a crash ---"
note ""
note "Reproduce it, minimise it, then put the minimised input in the module's"
note "own tests as a regression case. That is where it belongs permanently:"
note "the fuzzer finds a bug once, a unit test keeps it dead."

say ""
if [ "$fail" -eq 0 ] && [ -z "$artifacts" ]; then
    say "fuzz: ok — no crashes"
else
    say "fuzz: FAILURES — see $OUT"
fi
say "fuzz: report written to $OUT"
exit "$fail"
