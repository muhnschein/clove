#!/bin/sh
# Run sam-stress across a ladder of N, several times each, into one file.
#
# The R2 harness answers a question that needs repetition to answer: does one
# session's dial path degrade as concurrent streams pile up? One run at one N
# cannot say. The first attempt at this was five `make sam-stress` invocations
# typed by hand, each scrolling past in its own terminal, compared from memory
# — and the comparison that mattered (success rate against N) turned out to be
# non-monotonic, which is exactly the shape nobody spots that way.
#
# So: every level, every repeat, one file, and a table at the end.
#
#   ./ci/sam-stress-sweep.sh                        # 1 16 32 64 128 200, ×3
#   ./ci/sam-stress-sweep.sh --levels "16 64" --repeats 5
#   ./ci/sam-stress-sweep.sh --router java --payload 1024
#   make sam-sweep
#
# Nothing here aborts on a failing level. A ladder that stops at the first bad
# rung tells you where it broke and nothing about what lies above it, and the
# levels above are the question.
set -eu

usage() {
    cat <<'EOF'
usage: ci/sam-stress-sweep.sh [options]

  --router NAME     router to test (i2pd|java|emissary; default i2pd)
  --dial NAME       router the *dialer* uses (default: same as --router,
                    i.e. both sessions on one router)
  --levels "N…"     concurrency ladder (default "1 16 32 64 128 200")
  --repeats K       runs per level (default 3)
  --deadline S      per-run budget in seconds (default 360)
  --payload B       echo payload in bytes (default 4096; the control plane is
                    what R2 asks about, and a large payload measures tunnel
                    bandwidth instead)
  --out FILE        report path (default sam-stress-sweep-<timestamp>.txt)
  --help

Every run's full output is kept. The table at the end is built from each run's
machine-readable result line, not from parsing the pretty one.
EOF
}

ROUTER=i2pd
DIAL=""
LEVELS="1 16 32 64 128 200"
REPEATS=3
DEADLINE=360
# Deliberately not sam-stress's own 64 KiB default. A sweep runs this dozens of
# times, and at 64 KiB a congested router spent a median of 78s per echo — so
# the ladder measured tunnel bandwidth, took hours, and left the dial path
# (the R2 question) buried under it.
PAYLOAD=4096
OUT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --router)   ROUTER="${2:?--router needs a value}"; shift ;;
        --dial)     DIAL="${2:?--dial needs a value}"; shift ;;
        --levels)   LEVELS="${2:?--levels needs a value}"; shift ;;
        --repeats)  REPEATS="${2:?--repeats needs a value}"; shift ;;
        --deadline) DEADLINE="${2:?--deadline needs a value}"; shift ;;
        --payload)  PAYLOAD="${2:?--payload needs a value}"; shift ;;
        --out)      OUT="${2:?--out needs a value}"; shift ;;
        --help|-h)  usage; exit 0 ;;
        *) echo "unknown option $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

[ -n "$DIAL" ] || DIAL="$ROUTER"

sam_port_of() {
    case "$1" in
        i2pd) echo 7656 ;; java) echo 7666 ;; emissary) echo 7676 ;; *) echo "" ;;
    esac
}
container_of() {
    case "$1" in
        i2pd) echo systemd-i2pd ;; java) echo systemd-i2p-java ;;
        emissary) echo systemd-emissary ;; *) echo "" ;;
    esac
}

PORT=$(sam_port_of "$ROUTER")
DIAL_PORT=$(sam_port_of "$DIAL")
if [ -z "$PORT" ] || [ -z "$DIAL_PORT" ]; then
    echo "unknown router name (want i2pd|java|emissary)" >&2
    exit 2
fi

# Resolve --out against the caller's directory before moving to the repo root,
# so a relative path writes where the operator is standing.
case "${OUT:-}" in
    "" | /*) ;;
    *) OUT="$PWD/$OUT" ;;
esac
cd "$(dirname "$0")/.."
[ -n "$OUT" ] || OUT="sam-stress-sweep-$(date +%Y%m%d-%H%M%S).txt"
: > "$OUT"

say()  { printf '%s\n' "$*" | tee -a "$OUT"; }
note() { printf '%s\n' "$*" >> "$OUT"; }

say "clove sam-stress sweep"
say "generated: $(date -Is)"
say "report:    $OUT"
say "router:    $ROUTER (SAM 127.0.0.1:$PORT)$( [ "$DIAL" = "$ROUTER" ] || echo ", dialing from $DIAL (127.0.0.1:$DIAL_PORT)" )"
say "version:   $(./ci/router-version.sh "$ROUTER" 2>/dev/null || echo 'not recorded')"
say "levels:    $LEVELS   ×$REPEATS each"
say "budget:    ${DEADLINE}s per run, ${PAYLOAD}-byte echo"
say "commit:    $(git rev-parse --short HEAD 2>/dev/null || echo unknown)$(
        [ -n "$(git status --porcelain 2>/dev/null)" ] && echo ' (dirty)')"
say "uname:     $(uname -srm)"
say "ulimit -n: $(ulimit -n)"
say ""

# The inbound half needs the router in our network namespace (LIVE-TESTING
# §3.1). Worth saying before the ladder rather than after: every level would
# fail identically, and the reason has nothing to do with concurrency.
if command -v podman >/dev/null 2>&1 && podman container exists "$(container_of "$ROUTER")" 2>/dev/null; then
    say "!! $ROUTER runs in a container, so it cannot reach our forwarded"
    say "!! listener and every level below will fail on the listening side."
    say "!! See docs/LIVE-TESTING.md §3.1. Use a host-installed router, or run"
    say "!! clove inside the router's namespace. Sweeping anyway, for the record."
    say ""
fi

# Descriptors: two per stream, plus the session sockets. A ladder that dies of
# EMFILE at the top rung looks like a router limit and is not one.
top=0
for n in $LEVELS; do [ "$n" -gt "$top" ] && top=$n; done
want=$((2 * top + 32))
have=$(ulimit -n)
case "$have" in
    unlimited) ;;
    *) if [ "$have" -lt "$want" ]; then
           say "!! ulimit -n is $have; N=$top wants about $want. The top of the"
           say "!! ladder will hit EMFILE — raise it with 'ulimit -n $want' or"
           say "!! drop the top level. Sweeping anyway."
           say ""
       fi ;;
esac

say "=== building sam-stress (release)"
if ! cargo build --release -p i2pnet --bin sam-stress >>"$OUT" 2>&1; then
    say "build failed — see above. Nothing to sweep."
    exit 1
fi
say ""

RESULTS=""
# One run may legitimately spend its whole budget; the wrapper has to outlive
# that plus the bounded teardown sam-stress waits through before it prints.
STEP_LIMIT=$((DEADLINE + 150))

for n in $LEVELS; do
    rep=1
    while [ "$rep" -le "$REPEATS" ]; do
        label="N=$n run $rep/$REPEATS"
        # Progress goes to the terminal, the banner to the file. Sending both
        # to both interleaves them into one unreadable line.
        printf '  %-18s … ' "$label" >&2
        note "########## $label ##########"
        started=$(date +%s)
        set +e
        CLOVE_SAM_PORT="$PORT" \
        CLOVE_SAM_PORT_DIAL="$DIAL_PORT" \
        CLOVE_STRESS_DEADLINE="$DEADLINE" \
        CLOVE_STRESS_PAYLOAD="$PAYLOAD" \
            timeout --kill-after=30 "$STEP_LIMIT" \
            ./target/release/sam-stress "$n" >"$OUT.step" 2>&1
        rc=$?
        set -e
        elapsed=$(( $(date +%s) - started ))
        cat "$OUT.step" >> "$OUT"
        note ""
        note "--- exit: $rc after ${elapsed}s"
        note ""

        line=$(grep -m1 '^sam-stress-result' "$OUT.step" 2>/dev/null || true)
        if [ -n "$line" ]; then
            RESULTS="$RESULTS$line	rc=$rc
"
            printf '%s\n' "$(printf '%s' "$line" \
                | sed -E 's/.*dialed=([0-9]+).*echoed=([0-9]+).*/dialed \1, echoed \2/')" >&2
        elif [ "$rc" -eq 124 ]; then
            RESULTS="$RESULTS	n=$n	TIMEOUT after ${elapsed}s
"
            echo "TIMEOUT after ${elapsed}s" >&2
        else
            RESULTS="$RESULTS	n=$n	no result (exit $rc)
"
            echo "no result (exit $rc)" >&2
        fi
        rep=$((rep + 1))
    done
done
rm -f "$OUT.step"

say ""
say "########## sweep summary ##########"
say ""
say "  N  dialed  echoed  failed  unfin  hungup   tries  conn_p50  conn_p99   rtt_p50   wall"
printf '%s' "$RESULTS" | while IFS= read -r line; do
    [ -n "$line" ] || continue
    case "$line" in
        *sam-stress-result*)
            # Field order is not assumed: each key is looked up by name, so a
            # new field in sam-stress cannot shift this table's columns.
            get() { printf '%s' "$line" | tr '\t' '\n' | sed -n "s/^$1=//p" | head -1; }
            say "$(printf '%4s %7s %7s %7s %6s %7s %7s %9s %9s %9s %6ss' \
                "$(get n)" "$(get dialed)" "$(get echoed)" "$(get failed)" \
                "$(get unfinished)" "$(get gave_up)" "$(get tries)" \
                "$(get connect_p50_ms)" "$(get connect_p99_ms)" \
                "$(get rtt_p50_ms)" "$(get wall_s)")"
            ;;
        *) say "  $(printf '%s' "$line" | tr '\t' ' ')" ;;
    esac
done

say ""
say "Times are milliseconds unless marked. dialed = the stream was established"
say "(the control-plane number R2 asks about); echoed = it also carried the"
say "payload back. unfin = dialed but still in flight at the deadline; hungup ="
say "our own echo handler gave up, which surfaces as the dialer's \"failed to"
say "fill whole buffer\" and is ours, not the router's."
say ""
say "Read dialed against N first. If that holds as N climbs, the session's dial"
say "path is not degrading, whatever the echo columns do — those are tunnel"
say "bandwidth, and on a same-router run every byte crosses the router twice."
say ""
say "Record the outcome in docs/LIVE-TESTING.md §6.3 with the router version."
say "Report: $OUT"
