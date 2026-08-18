#!/bin/sh
# Measure what AddressSanitizer costs and what it buys, per target.
#
#   ./ci/fuzz-sanitizer-ab.sh                    # every target, 120s per arm
#   ./ci/fuzz-sanitizer-ab.sh --secs 300 magnet  # longer, one target
#
# Every crate in this workspace sets `unsafe_code = "forbid"`, so the bug class
# ASan exists to find — reading or writing memory that is not ours — cannot
# occur in the code under test. What it still does here is intercept `memcmp`
# and friends, which is where libFuzzer's *auto*-dictionary comes from, and
# catch memory errors inside dependencies. What it costs is throughput, and the
# `extensions` measurement puts that at roughly 4x.
#
# It also costs the only false crash this fuzzer has ever produced: the RSS
# growth behind that `oom-` artifact is ASan's allocator not returning freed
# pages, so dropping the sanitizer retires that whole problem rather than
# deferring it the way `-rss_limit_mb` does.
#
# That is a trade with numbers on both sides, so it gets measured rather than
# argued, on the same footing as the dictionary experiment: equal wall clock
# per arm, several RNG seeds, and a pristine copy of the committed seed corpus
# for every single run so no arm inherits another's findings.
#
# READ THE OUTPUT CAREFULLY. The two arms do not have identical instrumentation
# — ASan inlines its own shadow checks, which sancov can count as edges — so
# absolute coverage is not comparable across arms. What *is* comparable is each
# arm's gain over its own seed baseline, and its execution count. Those are the
# columns this prints.
set -eu

cd "$(dirname "$0")/.."

TARGETS_ALL='bencode metainfo resume json http wire tracker extensions magnet dest'
SECS=120
SEEDS=3
TARGETS=''

usage() {
    cat <<'USAGE'
usage: ci/fuzz-sanitizer-ab.sh [options] [target...]

  --secs N     wall clock per arm per seed (default 120)
  --seeds N    how many RNG seeds per arm (default 3)
  --out PATH   where to write the report (default fuzz/sanitizer-ab-<stamp>.txt,
               `-` for stdout)
  -h, --help   this

With no targets named, every target is measured. Total runtime is
secs * seeds * 2 * targets, so the default over all ten targets is two hours.
USAGE
}

OUT=''
while [ $# -gt 0 ]; do
    case "$1" in
        --secs) SECS="${2:?--secs needs a number}"; shift 2 ;;
        --seeds) SEEDS="${2:?--seeds needs a number}"; shift 2 ;;
        --out) OUT="${2:?--out needs a path}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "ab: unknown option $1" >&2; usage >&2; exit 2 ;;
        *) TARGETS="$TARGETS $1"; shift ;;
    esac
done
[ -n "$TARGETS" ] || TARGETS="$TARGETS_ALL"

if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    echo "ab: needs a nightly toolchain" >&2
    exit 1
fi
command -v cargo-fuzz >/dev/null 2>&1 || {
    echo "ab: needs cargo-fuzz: cargo install cargo-fuzz --locked" >&2
    exit 1
}

stamp=$(date -u +%Y%m%dT%H%M%SZ)
[ -n "$OUT" ] || OUT="fuzz/sanitizer-ab-$stamp.txt"
if [ "$OUT" = - ]; then
    emit() { printf '%s\n' "$*"; }
else
    : > "$OUT"
    emit() { printf '%s\n' "$*" | tee -a "$OUT"; }
fi

emit "clove fuzz sanitizer A/B"
emit "when      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
emit "commit    $(git rev-parse HEAD 2>/dev/null || echo unknown)"
emit "dirty     $(if [ -n "$(git status --porcelain 2>/dev/null)" ]; then echo yes; else echo no; fi)"
emit "rustc     $(rustc +nightly --version 2>/dev/null || echo unknown)"
emit "arms      address, none — ${SECS}s x ${SEEDS} seed(s) each"
emit ""
emit "Gain is over each arm's own seed baseline, not across arms: ASan's"
emit "inlined shadow checks are instrumented too, so the absolute figures are"
emit "not measuring the same thing. Execs are directly comparable."
emit ""

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

# Build both arms first, so a rebuild does not land inside a timed window.
for arm in address none; do
    printf 'ab: building --sanitizer %s\n' "$arm" >&2
    for t in $TARGETS; do
        cargo +nightly fuzz build --sanitizer "$arm" "$t" >/dev/null 2>&1 || {
            echo "ab: build failed for $t (--sanitizer $arm)" >&2
            exit 1
        }
    done
done

for t in $TARGETS; do
    dict=''
    [ -f "fuzz/dicts/$t.dict" ] && dict="-dict=$PWD/fuzz/dicts/$t.dict"
    maxlen=$(./ci/fuzz.sh --max-len "$t")

    # Unpack this target's slice of the committed seed once; each run gets a
    # copy of it rather than a fresh extraction.
    rm -rf "$work/seed"
    mkdir -p "$work/seed"
    if [ -f fuzz/seed-corpus.tar.gz ]; then
        tar xzf fuzz/seed-corpus.tar.gz -C "$work/seed" "corpus/$t" 2>/dev/null || true
    fi

    emit "$t"
    for arm in address none; do
        gains=''
        execs=''
        base=''
        s=1
        while [ "$s" -le "$SEEDS" ]; do
            # A pristine corpus per run. Without this the second arm starts
            # from everything the first one found, which is not a comparison —
            # it is the second arm being handed the answer.
            corpus="$work/$t-$arm-$s"
            rm -rf "$corpus"
            mkdir -p "$corpus"
            if [ -d "$work/seed/corpus/$t" ]; then
                cp "$work/seed/corpus/$t"/* "$corpus/" 2>/dev/null || true
            fi

            printf 'ab: %s --sanitizer %s seed %s\n' "$t" "$arm" "$s" >&2
            log="$work/$t.$arm.$s.log"
            # shellcheck disable=SC2086  # $dict is one flag or nothing
            cargo +nightly fuzz run --sanitizer "$arm" "$t" "$corpus" -- $dict \
                -max_total_time="$SECS" -max_len="$maxlen" -seed="$s" \
                -print_final_stats=1 >"$log" 2>&1 || true

            inited=$(grep -oE 'INITED cov: [0-9]+' "$log" 2>/dev/null |
                head -1 | grep -oE '[0-9]+$' || true)
            final=$(grep -oE 'cov: [0-9]+' "$log" 2>/dev/null |
                tail -1 | grep -oE '[0-9]+$' || true)
            n=$(grep -oE 'stat::number_of_executed_units: *[0-9]+' "$log" \
                2>/dev/null | tail -1 | grep -oE '[0-9]+$' || true)

            if [ -n "${inited:-}" ] && [ -n "${final:-}" ]; then
                gains="$gains $((final - inited))"
                [ -n "$base" ] || base="$inited"
            else
                gains="$gains ?"
            fi
            execs="$execs ${n:-?}"
            s=$((s + 1))
        done
        emit "$(printf '  %-9s seed cov %-6s gained%-18s execs%s' \
            "$arm" "${base:-?}" "$gains" "$execs")"
    done
    emit ""
done

emit "--- reading this ---"
emit ""
emit "ASan is worth its throughput here if the address arm finds edges the"
emit "none arm does not, or finds them at a comparable rate. If the none arm"
emit "gains at least as much per run while executing several times as often,"
emit "the sanitizer is being paid for in coverage and returning memory-error"
emit "detection that a forbid(unsafe_code) workspace cannot produce."
emit ""
emit "Two things this cannot price, and neither belongs in the table:"
emit ""
emit "  - Memory errors inside dependencies, which do contain unsafe code."
emit "    sha1, sha2 and rustix are what the parsers reach; none of them is"
emit "    doing pointer arithmetic on fuzzer input, but 'none' is not 'never'."
emit "  - Crash search. Coverage is a proxy for it and a poor one. More execs"
emit "    is more crash search, which argues the same way as the gain column,"
emit "    but neither column measures it."
emit ""
emit "If the numbers say drop it, the switch is ci/fuzz.sh --sanitizer, and the"
emit "honest arrangement is probably a default of none with a periodic address"
emit "sweep rather than dropping it outright."
[ "$OUT" = - ] || echo "ab: report written to $OUT" >&2
