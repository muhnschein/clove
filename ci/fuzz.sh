#!/bin/sh
# Run the coverage-guided fuzz targets and write one report.
#
#   ./ci/fuzz.sh                 # every target, default budgets (~20 min)
#   ./ci/fuzz.sh metainfo        # one target
#   ./ci/fuzz.sh --scale 4       # four times the budget, everything
#   ./ci/fuzz.sh --quick         # a couple of minutes, for "does it still build"
#   ./ci/fuzz.sh --seed          # and fold what the run found into the seed
#   ./ci/fuzz.sh --budget magnet # just print that target's budget (CI uses this)
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
SEED=no
TARGETS=''

usage() {
    cat <<'USAGE'
usage: ci/fuzz.sh [options] [target...]

  --scale N     multiply every budget by N (default 1)
  --quick       30s per target, for a smoke check rather than a hunt
  --seed        after the run, minimise the corpus and repack the committed
                seed so the next run starts from what this one found
  --budget T    print target T's budget in seconds and exit; this is how CI
                gets it, so the table below is the only copy of it
  --out PATH    where to write the report (default fuzz/report-<stamp>.txt)
  -h, --help    this

With no targets named, every target runs.
USAGE
}

OUT=''
BUDGET_OF=''
while [ $# -gt 0 ]; do
    case "$1" in
        --scale) SCALE="${2:?--scale needs a number}"; shift 2 ;;
        --quick) QUICK=yes; shift ;;
        --seed) SEED=yes; shift ;;
        --budget) BUDGET_OF="${2:?--budget needs a target}"; shift 2 ;;
        --out) OUT="${2:?--out needs a path}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "fuzz: unknown option $1" >&2; usage >&2; exit 2 ;;
        *) TARGETS="$TARGETS $1"; shift ;;
    esac
done
[ -n "$TARGETS" ] || TARGETS="$TARGETS_ALL"

# Per-target budget in seconds. Deliberately not flat, and set from what the
# last report measured rather than from taste. The comments are edges gained
# per 100 seconds in the 2026-07-30 sweep — the first with dictionaries — and
# the ordering they imply is nothing like the previous allocation, which had
# `magnet` and `resume` (the two fastest climbers) on the lowest tier and
# `metainfo` (0.3) on the joint highest. Total is unchanged at 3360s: this is
# a reallocation, not a bigger bill.
budget_for() {
    case "$1" in
        wire) echo 60 ;;                        # 0.0 — nothing new in 36M execs
        bencode) echo 180 ;;                    # 0.0
        json|metainfo) echo 240 ;;              # 0.0, 0.3
        http|extensions) echo 420 ;;            # 3.9, 2.0
        resume|tracker) echo 480 ;;             # 21.7, 9.0
        magnet) echo 840 ;;                     # 21.1, and 190 edges in absolute
        *) echo 300 ;;
    esac
}

# CI asks for the budget rather than carrying its own copy of the table. The
# two used to be duplicated, with a comment in each telling the reader they
# must agree — which is a rule somebody has to remember, on a file nobody edits
# often, where the cost of forgetting is a scheduled job quietly running the
# wrong budgets for months. Answering the question is cheaper than checking it.
if [ -n "$BUDGET_OF" ]; then
    case " $TARGETS_ALL " in
        *" $BUDGET_OF "*) budget_for "$BUDGET_OF"; exit 0 ;;
        *) echo "fuzz: no such target: $BUDGET_OF" >&2; exit 2 ;;
    esac
fi

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

corpus_count() { ls "fuzz/corpus/$1" 2>/dev/null | wc -l | tr -d ' '; }

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
note "dicts     $(ls fuzz/dicts/*.dict 2>/dev/null | wc -l | tr -d ' ') file(s) in fuzz/dicts"
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

# Start the long targets first, and refill a slot the moment one frees rather
# than waiting for a whole batch. The previous version launched in groups of
# $jobs and waited for the group, which left cores idle for as long as the
# spread within a group: `wire` at 60s next to `http` at 360s idled one core
# for five minutes. Same fuzzing, ~1160s of wall clock instead of ~1560s.
sched=$(for t in $TARGETS; do echo "$(budget_for "$t") $t"; done | sort -rn |
    while read -r _ name; do printf '%s ' "$name"; done)

# `wait -n` would say this in one line, but it is a bashism and this is
# /bin/sh. The slot count comes from the status files the jobs write on exit
# rather than from `kill -0` on their pids: a child that has exited but not
# been reaped is still a live pid as far as `kill` is concerned, and POSIX does
# not settle when a shell reaps. dash reaps promptly, so `kill -0` happens to
# work here — but that would be leaning on the /bin/sh nobody promised.
unfinished() {
    n=0
    for name in $1; do [ -f "$logs/$name.status" ] || n=$((n + 1)); done
    echo "$n"
}

started=''
for t in $sched; do
    if [ "$QUICK" = yes ]; then
        secs=30
    else
        secs=$(( $(budget_for "$t") * SCALE ))
    fi

    # A dictionary of the tokens the parser actually looks for. libFuzzer
    # learns some of these on its own — a `strip_prefix` compiles to an
    # intercepted memcmp, and it will recover the literal from that — but a
    # bencode key lookup walks bytes in a loop, which the interception never
    # sees. Measured on 2026-07-30 over 120s from the committed seed:
    # `magnet` went from +1 edge to +191, `extensions` from +21 to +113.
    dict=''
    [ -f "fuzz/dicts/$t.dict" ] && dict="-dict=$PWD/fuzz/dicts/$t.dict"

    corpus_count "$t" > "$logs/$t.before"
    say "fuzz: $t (${secs}s)"
    (
        # shellcheck disable=SC2086  # $dict is one flag or nothing
        cargo +nightly fuzz run "$t" -- $dict \
            -max_total_time="$secs" -print_final_stats=1 \
            >"$logs/$t.log" 2>&1
        echo "$?" > "$logs/$t.status"
    ) &
    started="$started $t"
    while [ "$(unfinished "$started")" -ge "$jobs" ]; do sleep 1; done
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
    # Coverage the corpus already had, before this run mutated anything.
    # libFuzzer prints it once, on the INITED line, after replaying the seeds.
    inited=$(grep -oE 'INITED cov: [0-9]+' "$logs/$t.log" 2>/dev/null |
        head -1 | grep -oE '[0-9]+$')
    before=$(cat "$logs/$t.before" 2>/dev/null || echo 0)
    after=$(corpus_count "$t")

    gained=''
    if [ -n "${cov:-}" ] && [ -n "${inited:-}" ]; then
        gained=$((cov - inited))
    fi

    if [ "$status" = 0 ]; then
        note "ok    $t"
    else
        note "FAIL  $t  (exit $status)"
        fail=1
    fi
    note "      execs ${execs:-?}  (${per_sec:-?}/s)  cov ${cov:-?}${gained:+ (+$gained this run)}"
    note "      corpus ${before:-0} -> ${after:-0} files  new-units ${added:-?}  peak-rss ${rss:-?} MiB"

    # What a budget should be revised from. Not `new_units_added`, which is
    # what the earlier version of this script used and which says almost
    # nothing: in the 2026-07-30 sweep `json` added 3262 units and did not
    # reach a single edge it had not already reached in 30 seconds. A unit is
    # kept for hitting a new *counter bucket*; only coverage says the run
    # learned something about the parser.
    #
    # Three bands, not two, because "+1 edge in six hundred seconds" and "+191"
    # are not the same message and the earlier wording called both of them
    # climbing. The line between trickling and climbing is drawn at 1% of the
    # target's own coverage, so it means the same thing for `wire` at 168 edges
    # as for `json` at 732.
    if [ -n "$gained" ]; then
        edges=edges
        [ "$gained" -eq 1 ] && edges=edge
        if [ "$gained" -eq 0 ]; then
            note "      note: no new edges — this budget is more than it needs"
        elif [ $((gained * 100)) -lt "${cov:-0}" ]; then
            note "      note: +$gained $edges — barely moving; trim before adding"
        else
            note "      note: +$gained $edges — still climbing, worth more time"
        fi
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

# What the run found is worth more than the fact that it found it. Left on
# disk, `fuzz/corpus/` is git-ignored and dies with the working tree; folded
# into the seed, it is the floor every future run and every fresh clone starts
# from. Not the default, because a smoke check should not rewrite a committed
# file as a side effect — but the report says so when there is something to
# fold, so the choice is at least an informed one.
note "--- corpus ---"
note ""
if [ "$SEED" = yes ]; then
    say "fuzz: minimising corpus and repacking the seed"
    # shellcheck disable=SC2086  # deliberate word splitting of the target list
    ./ci/fuzz-seed.sh $TARGETS >>"$OUT" 2>&1 || note "seed: FAILED"
else
    grew=0
    for t in $TARGETS; do
        b=$(cat "$logs/$t.before" 2>/dev/null || echo 0)
        a=$(corpus_count "$t")
        [ "$a" -gt "$b" ] 2>/dev/null && grew=$((grew + a - b))
    done
    if [ "$grew" -gt 0 ]; then
        note "This run added $grew file(s) to fuzz/corpus/, which is git-ignored."
        note "To keep them — minimised to the smallest set reaching the same"
        note "coverage — and make them the floor for every future run:"
        note ""
        note "    make fuzz-seed        # or re-run with ci/fuzz.sh --seed"
    else
        note "No new corpus files; the committed seed already covers this run."
    fi
fi
note ""

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
