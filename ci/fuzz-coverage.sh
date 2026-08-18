#!/bin/sh
# Per-function coverage of the parsers, from the committed seed corpus.
#
#   ./ci/fuzz-coverage.sh              # every target, writes a report
#   ./ci/fuzz-coverage.sh magnet wire  # just these
#   ./ci/fuzz-coverage.sh --out -      # to stdout
#
# The sweep reports edge *counts*, which say how much a run learned but not how
# much of the parser it ever reached. Those are different questions, and the
# second is the one that catches a target too narrow to exercise its own
# subject: `wire` sat at a plateau for three sweeps while `read_frame` was not
# fuzzed at all, and the `extensions` PEX-cap assertion could not fail in any
# run ever done. Both were found by somebody happening to look. A function
# reading "never entered" here is that same class of problem, found on purpose.
#
# So the number to act on is not the percentage. It is the list of functions a
# "saturated" target has never once entered.
#
# Needs nightly, cargo-fuzz and llvm-tools-preview:
#   rustup component add llvm-tools-preview --toolchain nightly
# `cargo install rustfilt` is optional and only makes the symbol names legible.
set -eu

cd "$(dirname "$0")/.."

TARGETS_ALL='bencode metainfo resume json http wire tracker extensions magnet dest'
OUT=''
TARGETS=''

usage() {
    cat <<'USAGE'
usage: ci/fuzz-coverage.sh [options] [target...]

  --out PATH   where to write the report (default fuzz/coverage-<stamp>.txt,
               `-` for stdout)
  -h, --help   this

With no targets named, every target is measured.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT="${2:?--out needs a path}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "coverage: unknown option $1" >&2; usage >&2; exit 2 ;;
        *) TARGETS="$TARGETS $1"; shift ;;
    esac
done
[ -n "$TARGETS" ] || TARGETS="$TARGETS_ALL"

# The source a target is *about*. Coverage over the whole workspace would be
# dominated by code no target claims to reach, which makes the figure
# unactionable; this is the file whose uncovered functions are that target's
# problem to answer for.
subject_for() {
    case "$1" in
        bencode)    echo 'crates/clove-core/src/bencode.rs' ;;
        metainfo)   echo 'crates/clove-core/src/metainfo.rs' ;;
        resume)     echo 'crates/clove-core/src/resume.rs' ;;
        json)       echo 'crates/clove-core/src/json.rs' ;;
        http)       echo 'crates/clove-core/src/http.rs' ;;
        wire)       echo 'crates/clove-core/src/wire.rs' ;;
        tracker)    echo 'crates/clove-core/src/tracker.rs' ;;
        magnet)     echo 'crates/clove-core/src/magnet.rs' ;;
        dest)       echo 'crates/i2pnet/src/addr.rs' ;;
        # The one target with three subjects, which is also why it is slow.
        extensions) echo 'crates/clove-core/src/pex.rs crates/clove-core/src/metadata.rs crates/clove-core/src/extension.rs' ;;
        *) echo '' ;;
    esac
}

for tool in cargo rustc; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "coverage: $tool is not installed" >&2
        exit 1
    }
done
if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    echo "coverage: needs a nightly toolchain: rustup toolchain install nightly" >&2
    exit 1
fi
if ! command -v cargo-fuzz >/dev/null 2>&1; then
    echo "coverage: needs cargo-fuzz: cargo install cargo-fuzz --locked" >&2
    exit 1
fi
# `cargo cov` is llvm-cov behind a shim that knows where the toolchain keeps
# it. Without llvm-tools-preview the shim exists and the binary does not, which
# would fail several minutes into the first build rather than here.
if ! cargo +nightly cov -- --version >/dev/null 2>&1; then
    echo "coverage: needs llvm-tools-preview:" >&2
    echo "  rustup component add llvm-tools-preview --toolchain nightly" >&2
    exit 1
fi

stamp=$(date -u +%Y%m%dT%H%M%SZ)
[ -n "$OUT" ] || OUT="fuzz/coverage-$stamp.txt"
if [ "$OUT" = - ]; then
    emit() { printf '%s\n' "$*"; }
    emit_stream() { cat; }
else
    : > "$OUT"
    emit() { printf '%s\n' "$*" | tee -a "$OUT"; }
    emit_stream() { tee -a "$OUT"; }
fi

emit "clove fuzz coverage"
emit "when      $(date -u +%Y-%m-%dT%H:%M:%SZ)"
emit "commit    $(git rev-parse HEAD 2>/dev/null || echo unknown)"
emit "dirty     $(if [ -n "$(git status --porcelain 2>/dev/null)" ]; then echo yes; else echo no; fi)"
emit "rustc     $(rustc +nightly --version 2>/dev/null || echo unknown)"
emit ""
emit "Regions reached by replaying the committed seed corpus — no mutation, so"
emit "this is the floor every run starts from, not what a run reaches."
emit ""

# Seed the corpus the way a run does, so the figure belongs to the committed
# seed rather than to whatever the working tree has accumulated.
if [ -f fuzz/seed-corpus.tar.gz ]; then
    mkdir -p fuzz/corpus
    tar xzf fuzz/seed-corpus.tar.gz -C fuzz -k 2>/dev/null || true
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

# Symbol names are left mangled on purpose: a demangled Rust name contains
# spaces (`<T as Trait>::method`), and the awk below reads fixed columns, so
# demangling before parsing turns every generic impl into a mis-parsed row.
# `rustfilt` is applied afterwards, to the names only, when it is installed.
demangle() {
    if command -v rustfilt >/dev/null 2>&1; then rustfilt; else cat; fi
}

cold_total=0
for t in $TARGETS; do
    subjects=$(subject_for "$t")
    if [ -z "$subjects" ]; then
        emit "skip  $t (no subject mapped)"
        continue
    fi

    printf 'coverage: %s\n' "$t" >&2
    if ! cargo +nightly fuzz coverage "$t" >"$work/$t.build" 2>&1; then
        emit "FAIL  $t — coverage build or replay failed:"
        tail -15 "$work/$t.build" | sed 's/^/      /' | emit_stream >/dev/null
        continue
    fi

    # cargo-fuzz has moved this path between releases, so find it rather than
    # spelling it out and breaking on the next one. Newest wins, in case an
    # older layout is still lying around from a previous version.
    bin=$(find fuzz/target -type f -name "$t" -path '*coverage*' \
        -exec ls -t {} + 2>/dev/null | head -1)
    prof="fuzz/coverage/$t/coverage.profdata"
    if [ -z "$bin" ] || [ ! -f "$prof" ]; then
        emit "FAIL  $t — no instrumented binary or profile found"
        continue
    fi

    for src in $subjects; do
        if ! cargo +nightly cov -- report "$bin" \
            "-instr-profile=$prof" \
            -ignore-filename-regex='/rustc/|\.cargo/registry|/fuzz_targets/' \
            -show-functions "$src" > "$work/rep" 2>/dev/null
        then
            emit "FAIL  $t — llvm-cov report failed for $src"
            continue
        fi

        # With -show-functions the columns are:
        #   Name  Regions  MissedRegions  Cover  Lines  MissedLines  Cover ...
        # so $2 and $3 are the region count and the miss count, and a row where
        # they are equal is a function nothing ever entered.
        set -- $(awk '/^TOTAL/ { print $2, $3, $4; exit }' "$work/rep")
        regions=${1:-0}; missed=${2:-0}; pct=${3:-?}
        emit "$(printf '%-11s %-16s regions %5s of %-6s %7s' \
            "$t" "$(basename "$src")" "$((regions - missed))" "$regions" "$pct")"

        awk '$1 != "TOTAL" && NF >= 4 && $2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ &&
             $2 == $3 && $2 > 0 { print $1, $2 }' \
            "$work/rep" > "$work/cold"
        while read -r sym n; do
            [ -n "$sym" ] || continue
            emit "$(printf '        never entered: %s (%s regions)' \
                "$(printf '%s' "$sym" | demangle)" "$n")"
        done < "$work/cold"
        cold_total=$((cold_total + $(wc -l < "$work/cold" | tr -d ' ')))
    done
done

emit ""
emit "--- reading this ---"
emit ""
emit "A low percentage is not automatically a problem. Error paths, Display"
emit "impls and encode-side helpers are regions too, and some are unreachable"
emit "from the target by design. The rows that matter are the functions marked"
emit "\"never entered\": a target that never calls one is not fuzzing it,"
emit "whatever its edge count says."
emit ""
emit "$cold_total function(s) never entered across the targets measured."
[ "$OUT" = - ] || echo "coverage: report written to $OUT" >&2
