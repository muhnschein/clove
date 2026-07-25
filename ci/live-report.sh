#!/bin/sh
# Run every test tier that applies on this machine and write one report.
#
#   ./ci/live-report.sh              # test whatever routers are already up
#   ./ci/live-report.sh --up         # bring the routers up first, then test
#   ./ci/live-report.sh --help
#
# The point is a single file you can hand to someone else. It captures what was
# run, what it did, and enough context to tell a clove bug from a cold router —
# router versions, peer counts, container logs on failure — rather than leaving
# that to a follow-up round of questions.
#
# Nothing here aborts on failure. A router that is down, a test that fails, a
# stress level that collapses: all recorded, all continued past, all summarised
# at the end. A run that stops at the first problem wastes the other twenty
# minutes of your time.
#
# What ends up in the file: command output, router versions, container logs.
# API tokens are redacted. I2P destinations are NOT — they are what makes a
# dial traceable, and the ones here belong to transient test identities that
# live for the length of the run. If you would rather they did not leave your
# machine, use --redact-dests.
set -eu

usage() {
    cat <<'USAGE'
usage: ci/live-report.sh [options]

  --up                 bring routers up (build, start, enable SAM, wait) first.
                       Without this, routers that are not already answering are
                       recorded as skipped.
  --routers "a b c"    which routers to test (default: i2pd java emissary)
  --stress "16 32 64"  sam-stress concurrency levels (default: 16 32 64 128)
  --out FILE           report path (default: live-report-<timestamp>.txt)
  --lines N            per-command output cap, head+tail (default: 250)
  --skip-tier1         skip the router-free tests (build, unit, smoke, chaos)
  --redact-dests       replace .b32.i2p addresses with a placeholder
  --help               this

Typical first run:
    ./ci/live-report.sh --up 2>&1 | tail -20
Then send the file it names.
USAGE
}

ROUTERS="i2pd java emissary"
STRESS="16 32 64 128"
OUT=""
LINES=250
BRING_UP=no
SKIP_TIER1=no
REDACT_DESTS=no

while [ $# -gt 0 ]; do
    case "$1" in
        --up) BRING_UP=yes ;;
        --routers) ROUTERS="${2:?--routers needs a value}"; shift ;;
        --stress) STRESS="${2:?--stress needs a value}"; shift ;;
        --out) OUT="${2:?--out needs a value}"; shift ;;
        --lines) LINES="${2:?--lines needs a value}"; shift ;;
        --skip-tier1) SKIP_TIER1=yes ;;
        --redact-dests) REDACT_DESTS=yes ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown option $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

cd "$(dirname "$0")/.."
[ -n "$OUT" ] || OUT="live-report-$(date +%Y%m%d-%H%M%S).txt"
: > "$OUT"

# Per-step time limits. A live download over I2P is slow but not unbounded;
# these exist so one wedged step cannot eat the whole session.
T_TIER1=1800
T_LIVE=1800
T_STRESS=900
T_UP=600

SUMMARY=""

say() { printf '%s\n' "$*" | tee -a "$OUT"; }
note() { printf '%s\n' "$*" >> "$OUT"; }

# Strip anything that should not travel, and cap the volume. Keeping head and
# tail matters: the head says what was being attempted, the tail says how it
# ended, and the middle of a 40k-line cargo log says neither.
sanitise() {
    sed -E \
        -e 's/(x-clove-token:[[:space:]]*)[A-Za-z0-9]+/\1<redacted>/Ig' \
        -e 's/\b[0-9a-f]{64}\b/<64-hex-redacted>/g' \
    | if [ "$REDACT_DESTS" = yes ]; then
          sed -E 's/[a-z2-7]{52}\.b32\.i2p/<dest>.b32.i2p/g'
      else
          cat
      fi \
    | awk -v n="$LINES" '
        { lines[NR] = $0 }
        END {
            if (NR <= 2 * n) { for (i = 1; i <= NR; i++) print lines[i]; }
            else {
                for (i = 1; i <= n; i++) print lines[i];
                printf "\n… %d lines elided …\n\n", NR - 2 * n;
                for (i = NR - n + 1; i <= NR; i++) print lines[i];
            }
        }'
}

# Run one step: header, bounded execution, captured output, recorded verdict.
step() {
    name="$1"; limit="$2"; shift 2
    say ""
    say "=== $name"
    note "--- command: $*"
    note "--- started: $(date -Is)"
    set +e
    timeout "$limit" "$@" > "$OUT.step" 2>&1
    rc=$?
    set -e
    sanitise < "$OUT.step" >> "$OUT"
    rm -f "$OUT.step"
    note "--- exit: $rc"
    case "$rc" in
        0) verdict=PASS ;;
        124) verdict="TIMEOUT after ${limit}s" ;;
        *) verdict="FAIL (exit $rc)" ;;
    esac
    say "--- $name: $verdict"
    SUMMARY="$SUMMARY
$(printf '%-42s %s' "$name" "$verdict")"
    [ "$rc" = 0 ]
}

# Record a step we did not run, so the summary distinguishes "not run" from
# "passed" — the two are nothing alike and blurring them wastes a run.
skip() {
    say ""
    say "=== $1: SKIPPED — $2"
    SUMMARY="$SUMMARY
$(printf '%-42s %s' "$1" "SKIP ($2)")"
}

port_answers() {
    if command -v bash >/dev/null 2>&1; then
        timeout 2 bash -c "</dev/tcp/127.0.0.1/$1" 2>/dev/null
    elif command -v nc >/dev/null 2>&1; then
        nc -z -w 2 127.0.0.1 "$1" >/dev/null 2>&1
    else
        return 1
    fi
}

sam_port_of() {
    case "$1" in
        i2pd) echo 7656 ;;
        java) echo 7666 ;;
        emissary) echo 7676 ;;
        *) echo "" ;;
    esac
}

container_of() {
    case "$1" in
        i2pd) echo systemd-i2pd ;;
        java) echo systemd-i2p-java ;;
        emissary) echo systemd-emissary ;;
        *) echo "" ;;
    esac
}

# ---------------------------------------------------------------- environment

say "clove live report"
say "generated: $(date -Is)"
say "report file: $OUT"
say ""
say "=== environment"
{
    echo "uname:        $(uname -srmo 2>/dev/null || uname -a)"
    echo "distro:       $(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME" || echo unknown)"
    echo "rustc:        $(rustc --version 2>/dev/null || echo 'not found')"
    echo "cargo:        $(cargo --version 2>/dev/null || echo 'not found')"
    echo "podman:       $(podman --version 2>/dev/null || echo 'not found')"
    echo "systemd-user: $(systemctl --user --version 2>/dev/null | head -1 || echo 'not available')"
    echo "clove commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown) \
$(git describe --tags --always --dirty 2>/dev/null || true)"
    echo "clove branch: $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
    echo "dirty files:  $(git status --porcelain 2>/dev/null | wc -l)"
    echo "cpus:         $(nproc 2>/dev/null || echo unknown)"
    echo "memory:       $(free -h 2>/dev/null | awk '/^Mem:/{print $2" total, "$7" available"}' || echo unknown)"
    echo "disk (cwd):   $(df -h . 2>/dev/null | awk 'NR==2{print $4" available"}' || echo unknown)"
    echo "routers:      $ROUTERS"
    echo "stress levels:$STRESS"
} | sanitise >> "$OUT"

# ------------------------------------------------------------------ bring up

if [ "$BRING_UP" = yes ]; then
    say ""
    say "########## bringing routers up ##########"
    for r in $ROUTERS; do
        step "up/$r: build image"    "$T_UP" make router-build ROUTER="$r" || true
        step "up/$r: start"          "$T_UP" make router-up ROUTER="$r" || true
        # Java I2P and emissary write the config the SAM switch lives in on
        # their first boot, so the enable step has to come after a wait, not
        # before it. i2pd needs neither.
        if [ "$r" != i2pd ]; then
            say "--- giving $r 60s to write its initial config before enabling SAM"
            sleep 60
            step "up/$r: enable SAM"  "$T_UP" make router-sam-enable ROUTER="$r" || true
        fi
    done
    say ""
    say "--- routers started. A cold router needs minutes to reseed and build"
    say "--- tunnels; waiting up to 300s each before testing."
    for r in $ROUTERS; do
        step "up/$r: wait for SAM" "$T_UP" make router-wait ROUTER="$r" WAIT=300 || true
    done
fi

# ------------------------------------------------------------------- tier 1

if [ "$SKIP_TIER1" = no ]; then
    say ""
    say "########## tier 1 — no router needed ##########"
    step "tier1: build"        "$T_TIER1" cargo build --workspace || true
    step "tier1: unit tests"   "$T_TIER1" cargo test --workspace || true
    step "tier1: smoke"        "$T_TIER1" ./ci/smoke.sh || true
    step "tier1: chaos"        "$T_TIER1" ./ci/chaos.sh || true
    step "tier1: no-clearnet gate" 120 ./ci/check-net-deps.sh || true
    step "tier1: man pages"    120 make man-lint || true
else
    skip "tier1" "--skip-tier1"
fi

# ------------------------------------------------------------------- tier 2

say ""
say "########## tier 2 — live routers ##########"

for r in $ROUTERS; do
    port=$(sam_port_of "$r")
    container=$(container_of "$r")
    say ""
    say "########## router: $r (SAM 127.0.0.1:$port) ##########"

    if [ -z "$port" ]; then
        skip "$r" "unknown router name"
        continue
    fi

    # Context first, so a failure below is already explained by what is above.
    say ""
    say "=== $r: router context"
    {
        echo "unit state:   $(systemctl --user is-active "$(basename "$container" | sed 's/^systemd-//')" 2>/dev/null || echo unknown)"
        if podman container exists "$container" 2>/dev/null; then
            echo "container:    present"
            echo "image:        $(podman inspect -f '{{.ImageName}}' "$container" 2>/dev/null || echo unknown)"
            echo "started:      $(podman inspect -f '{{.State.StartedAt}}' "$container" 2>/dev/null || echo unknown)"
            echo "restarts:     $(podman inspect -f '{{.RestartCount}}' "$container" 2>/dev/null || echo unknown)"
            case "$r" in
                i2pd) echo "version:      $(podman exec "$container" i2pd --version 2>/dev/null | head -1 || echo unknown)" ;;
                emissary) echo "version:      $(podman exec "$container" emissary-cli --version 2>/dev/null | head -1 || echo unknown)" ;;
                java) echo "version:      $(podman inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$container" 2>/dev/null || echo 'see image tag')" ;;
            esac
        else
            echo "container:    absent"
        fi
        echo "SAM port:     $(port_answers "$port" && echo answering || echo 'not answering')"
        # Peer count separates "clove cannot dial" from "this router knows
        # nobody to dial through", which looked identical last time round.
        case "$r" in
            i2pd)
                if port_answers 7070; then
                    echo "console:      answering on 7070"
                    echo "netdb (best effort):"
                    curl -s -m 5 http://127.0.0.1:7070/ 2>/dev/null \
                        | sed -E 's/<[^>]+>/ /g' \
                        | grep -iE 'routers|floodfills|tunnel|uptime|received|sent' \
                        | head -12 | sed 's/^/  /' || echo "  (could not read)"
                else
                    echo "console:      not answering on 7070"
                fi
                ;;
            java)
                if port_answers 7657; then echo "console:      answering on 7657"
                else echo "console:      not answering on 7657"; fi
                ;;
            emissary) echo "console:      none (emissary has no web console here)" ;;
        esac
    } 2>&1 | sanitise >> "$OUT"

    if ! port_answers "$port"; then
        skip "$r: live tests" "SAM not answering on 127.0.0.1:$port"
        if podman container exists "$container" 2>/dev/null; then
            say ""
            say "=== $r: container log (SAM down — this is usually why)"
            podman logs --tail 80 "$container" 2>&1 | sanitise >> "$OUT"
        else
            note ""
            note "To bring it up:  make router-up ROUTER=$r"
            if [ "$r" = emissary ]; then
                note "                 (build it first: make router-build ROUTER=emissary)"
            fi
            if [ "$r" != i2pd ]; then
                note "                 then: make router-sam-enable ROUTER=$r"
            fi
        fi
        continue
    fi

    live_ok=yes
    step "$r: live tests" "$T_LIVE" make test-live ROUTER="$r" || live_ok=no

    for n in $STRESS; do
        step "$r: sam-stress N=$n" "$T_STRESS" make sam-stress ROUTER="$r" N="$n" || true
    done

    # Logs are worth having whenever anything failed: the router's account of
    # a failed dial is usually more informative than ours.
    if [ "$live_ok" = no ] && podman container exists "$container" 2>/dev/null; then
        say ""
        say "=== $r: container log after failure"
        podman logs --tail 120 "$container" 2>&1 | sanitise >> "$OUT"
    fi
done

# ------------------------------------------------------------------ summary

say ""
say "########## summary ##########"
say "$SUMMARY"
# A short companion: environment, the summary table, and every section that
# did not pass. Long enough to diagnose from, short enough to paste. The full
# report stays alongside it for when the answer is in a section that passed.
{
    echo "clove live report — SHORT FORM"
    echo "generated: $(date -Is)"
    echo "full report: $OUT"
    echo
    sed -n '/^=== environment/,/^####/p' "$OUT" | sed '$d'
    echo "########## summary ##########"
    printf '%s\n' "$SUMMARY"
    echo
    echo "########## sections that did not pass ##########"
    # Everything from the summary banner down is already above; the
    # environment block is already above too. Cutting both keeps this file
    # readable instead of printing itself back three times.
    sed '/^########## summary ##########/,$d' "$OUT" | awk '
        /^(=== |########## )/ {
            if (buffered && !passed && !skip_section) { printf "%s", buf }
            buf = ""; buffered = 1; passed = 0
            skip_section = ($0 == "=== environment")
        }
        { buf = buf $0 "\n" }
        /^--- .*: PASS$/ { passed = 1 }
        END { if (buffered && !passed && !skip_section) printf "%s", buf }
    ' 
} > "$OUT.short"

say ""
say "reports written:"
say "  full:  $OUT ($(wc -l < "$OUT" | tr -d ' ') lines, $(wc -c < "$OUT" | tr -d ' ') bytes)"
say "  short: $OUT.short ($(wc -l < "$OUT.short" | tr -d ' ') lines, $(wc -c < "$OUT.short" | tr -d ' ') bytes)"
say ""
say "Send the short one first — it carries the environment, the verdict table,"
say "and every section that did not pass. The full one has the passing output"
say "too, for when the answer turns out to be hiding in a test that went green."
