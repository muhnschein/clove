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
# API tokens and I2P destinations are both redacted — b32 addresses and the full
# base64 destinations a tracker announce carries in `ip=`.
#
# Destinations used to be kept by default, on the reasoning that they are what
# makes a dial traceable and that the identities in a test run are transient.
# The first half is true and is why --keep-dests exists. The second is an
# assumption about how the script is being used, made by the script, about a
# file whose entire purpose is to be sent to somebody else — and it is wrong the
# first time anyone points this at a daemon holding their real persisted
# identity, which is exactly when it costs the most. A default that is safe when
# the assumption fails is the better default; the reverse asks every user to
# re-derive the reasoning above before their first run.
set -eu

usage() {
    cat <<'USAGE'
usage: ci/live-report.sh [options]

  --up                 bring routers up (build, start, enable SAM, wait) first.
                       Without this, routers that are not already answering are
                       recorded as skipped.
  --routers "a b c"    which routers to test (default: i2pd java emissary)
  --swarm TORRENT      also run the live-swarm tier (ci/live-swarm.sh) against a
                       magnet URI or .torrent you supply. This is the tier worth
                       having; see the note below.
  --swarm-router NAME  which router the swarm run uses (default: the first one
                       answering). One download is enough; three is a day.
  --swarm-deadline N   swarm download budget, seconds (default 3600)
  --swarm-seed N       swarm seeding budget after completion (default 900)
  --stress "16 32 64"  sam-stress concurrency levels (default: 16 32 64 128)
  --out FILE           report path (default: live-report-<timestamp>.txt)
  --lines N            per-command output cap, head+tail (default: 250)
  --skip-tier1         skip the router-free tests (build, unit, smoke, chaos)
  --keep-dests         leave I2P destinations in the report. They are removed
                       by default — both .b32.i2p addresses and full base64
                       destinations. Use this only when the run is a throwaway
                       identity and a traceable dial is what you are debugging
  --redact-dests       accepted and does nothing; redaction is the default
  --help               this

Typical first run:
    ./ci/live-report.sh --up --swarm 'magnet:?xt=urn:btih:…' 2>&1 | tail -20
Then send the file it names.

On --swarm: the loopback tiers below it (readiness, the gated tests,
sam-stress) all put two destinations on one router and need a leaseSet
published seconds ago to be resolvable — the most fragile thing a young router
does, and where every run so far died without implicating clove
(PROTOCOL.i2p-bt 2.6c, 2.8). The swarm tier needs none of that and proves more,
so it runs first and does not depend on the readiness gate.
USAGE
}

ROUTERS="i2pd java emissary"
STRESS="16 32 64 128"
SWARM=""
SWARM_ROUTER=""
# The swarm tier's two budgets. They live here, and the wrapper timeout below
# is computed from them rather than written down a second time — the rule this
# script learned the hard way (PROTOCOL.i2p-bt 2.6d).
SWARM_DEADLINE=${SWARM_DEADLINE:-3600}
SWARM_SEED=${SWARM_SEED:-900}
# Budget handed to the readiness probe, and the margin the outer `timeout` adds
# on top so the wrapper always outlives what it wraps (cargo startup, a slow
# release link, the report the probe prints on its way out). Every nested
# deadline in this script derives from the one it contains.
READY_DEADLINE=${READY_DEADLINE:-240}
# The margin has to cover everything the probe does *outside* its own budget:
# a cargo release build, session setup, and the bounded teardown it now waits
# through (sam-stress's TEARDOWN_GRACE) before it prints. It was 60s, and an
# i2pd probe with a 240s budget was killed at 300s with its report already
# computed and unprinted — a TIMEOUT row where an honest "unfinished 1" was one
# second away. A backstop that fires before the thing it backs up is not a
# backstop.
STEP_SLACK=150
OUT=""
LINES=250
BRING_UP=no
SKIP_TIER1=no
REDACT_DESTS=yes

while [ $# -gt 0 ]; do
    case "$1" in
        --up) BRING_UP=yes ;;
        --routers) ROUTERS="${2:?--routers needs a value}"; shift ;;
        --swarm) SWARM="${2:?--swarm needs a magnet or .torrent}"; shift ;;
        --swarm-router) SWARM_ROUTER="${2:?--swarm-router needs a value}"; shift ;;
        --swarm-deadline) SWARM_DEADLINE="${2:?--swarm-deadline needs a value}"; shift ;;
        --swarm-seed) SWARM_SEED="${2:?--swarm-seed needs a value}"; shift ;;
        --stress) STRESS="${2:?--stress needs a value}"; shift ;;
        --out) OUT="${2:?--out needs a value}"; shift ;;
        --lines) LINES="${2:?--lines needs a value}"; shift ;;
        --skip-tier1) SKIP_TIER1=yes ;;
        # Accepted and a no-op: it is the default now, and a flag somebody
        # has in their shell history should not become an error.
        --redact-dests) REDACT_DESTS=yes ;;
        --keep-dests) REDACT_DESTS=no ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown option $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

# Resolve a relative --out against the caller's directory *before* moving to the
# repo root, so `--out report.txt` writes where the operator is standing rather
# than dropping an untracked file into the checkout.
case "${OUT:-}" in
    "" | /*) ;;
    *) OUT="$PWD/$OUT" ;;
esac
# A .torrent handed to --swarm gets the same treatment: it names a file in the
# caller's directory, not one in the checkout. A magnet is left alone.
case "${SWARM:-}" in
    "" | /* | magnet:*) ;;
    *) SWARM="$PWD/$SWARM" ;;
esac
cd "$(dirname "$0")/.."
[ -n "$OUT" ] || OUT="live-report-$(date +%Y%m%d-%H%M%S).txt"
: > "$OUT"

# Per-step time limits. A live download over I2P is slow but not unbounded;
# these exist so one wedged step cannot eat the whole session.
T_TIER1=1800
T_LIVE=1800
T_STRESS=600
# Budget handed to sam-stress itself; the outer timeout above is the backstop
# for the case where even that fails to return.
STRESS_DEADLINE=${STRESS_DEADLINE:-300}
T_UP=600
# The swarm wrapper outlives what it wraps: both of the script's phases, plus
# a release build and the final verify pass. Derived, never written twice.
T_SWARM=$((SWARM_DEADLINE + SWARM_SEED + 900))

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
          # Both forms a destination takes, because removing only the first is
          # worse than removing neither: it reads as "destinations are gone"
          # while the more complete one is still in the file.
          #
          #   1. the b32 label — a hash of the destination, 52 base32 chars;
          #   2. the destination itself — ~516 chars of I2P base64, which is
          #      what a tracker announce carries in `ip=` and what a SAM
          #      DESTINATION= line carries the private key behind.
          #
          # I2P's base64 alphabet is A-Za-z0-9 plus `-` and `~`, and every one
          # of those is unreserved in a URL — so percent-encoding a destination
          # leaves it byte-identical apart from `=` padding, and one pattern
          # covers the raw and the URL-encoded form. 128 is far below the 516 a
          # real destination runs to and far above any word, hash or path that
          # belongs in a report.
          sed -E \
              -e 's/[a-z2-7]{52}\.b32\.i2p/<dest>.b32.i2p/g' \
              -e 's/[A-Za-z0-9~-]{128,}(%3D|=)*/<i2p-dest-redacted>/g'
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
    timeout --kill-after=30 "$limit" "$@" > "$OUT.step" 2>&1
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

unit_of() {
    case "$1" in
        i2pd) echo i2pd ;;
        java) echo i2p-java ;;
        emissary) echo emissary ;;
        *) echo "" ;;
    esac
}

quadlet_of() {
    case "$1" in
        i2pd) echo i2pd.container ;;
        java) echo i2p-java.container ;;
        emissary) echo emissary.container ;;
        *) echo "" ;;
    esac
}

# Why a router did not start. `make router-up` reports only that systemctl
# failed; the reason is in the unit status and the journal, and a report that
# omits it buys a whole extra round trip. Quadlet's own complaints land here
# too: a key it does not recognise means no unit is generated at all, and the
# only visible symptom is "Unit foo.service not found".
router_diagnosis() {
    r="$1"; unit=$(unit_of "$r")
    echo "--- systemctl --user status $unit"
    systemctl --user status "$unit" --no-pager --lines=0 2>&1 | head -15 || true
    echo
    echo "--- unit file generated by quadlet?"
    if systemctl --user list-unit-files "$unit.service" --no-pager 2>/dev/null | grep -q "$unit"; then
        echo "yes"
    else
        echo "NO — quadlet did not generate $unit.service from the .container file."
        echo "    That is usually a key quadlet does not recognise; the generator"
        echo "    logs the offending line in the journal below."
    fi
    echo
    echo "--- journalctl --user -u $unit (last 40)"
    journalctl --user -u "$unit" --no-pager --lines=40 2>&1 | tail -40 || true
    echo
    echo "--- quadlet generator complaints (last 20)"
    journalctl --user --no-pager --lines=200 2>/dev/null \
        | grep -iE 'quadlet|podman-system-generator' | tail -20 || echo "(none found)"
}

container_of() {
    case "$1" in
        i2pd) echo systemd-i2pd ;;
        java) echo systemd-i2p-java ;;
        emissary) echo systemd-emissary ;;
        *) echo "" ;;
    esac
}

# Does this router run in a container, i.e. in a network namespace that is not
# ours? Everything that needs the *inbound* half turns on this.
#
# `STREAM FORWARD` carries no HOST= and clove's forwarded listener is bound to
# 127.0.0.1, so the router connects back to whatever address our SAM control
# connection appears to come from — the same address only when the router
# shares our namespace (docs/LIVE-TESTING.md §3.1). A containerized router
# therefore dials out perfectly well and can never hand us an inbound stream.
#
# This is not a subtlety worth rediscovering per run. A whole matrix sweep once
# reported FAIL for every containerized router — readiness, the loopback tier,
# every stress level, and half the cross pairs — while `make swarm` was
# downloading happily against those same routers minutes earlier. The swarm
# tier only needs the outbound half; these tiers need both. Skipping with the
# reason beats a red row that means nothing.
is_containerised() {
    # Not named `container`: this runs inside the router loop, which holds that
    # name, and POSIX sh has no locals.
    _ctr=$(container_of "$1")
    [ -n "$_ctr" ] && podman container exists "$_ctr" 2>/dev/null
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
    echo "memory:       $(awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} END{if (t) printf "%.1f GiB total, %.1f GiB available", t/1048576, a/1048576; else print "unknown"}' /proc/meminfo 2>/dev/null || echo unknown)"
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
        if ! step "up/$r: wait for SAM" "$T_UP" make router-wait ROUTER="$r" WAIT=300; then
            say ""
            say "=== up/$r: why it did not come up"
            router_diagnosis "$r" 2>&1 | sanitise >> "$OUT"
        fi
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

# ------------------------------------------------------------------- tier 3
#
# Ahead of tier 2 on purpose. Tier 2 is the loopback topology — two
# destinations on one router, dialing each other — which needs a leaseSet
# published seconds ago to be resolvable by a sibling session on the same
# router. That is the most fragile netDb operation a young router performs, one
# router cannot do it at all (PROTOCOL.i2p-bt 2.8), and it is where every
# recorded run has died without any of those deaths implicating clove.
#
# The swarm tier asks the network for strictly less: it resolves destinations
# that have been published for months, and the download half never needs our
# own leaseSet resolved by anybody, because I2P bundles it with the stream's
# opening message. It also proves far more — tracker, BEP 9, picker, choker,
# storage, PEX, and the inbound path, against i2psnark. So it goes first, and
# it does not sit behind the readiness gate.

if [ -n "$SWARM" ]; then
    if [ -z "$SWARM_ROUTER" ]; then
        for r in $ROUTERS; do
            p=$(sam_port_of "$r")
            if [ -n "$p" ] && port_answers "$p"; then SWARM_ROUTER="$r"; break; fi
        done
    fi
    say ""
    say "########## tier 3 — a live swarm ##########"
    if [ -z "$SWARM_ROUTER" ]; then
        skip "tier3: live swarm" "no router is answering"
    else
        say "subject: $SWARM"
        say "router:  $SWARM_ROUTER"
        step "tier3: live swarm ($SWARM_ROUTER)" "$T_SWARM" \
            ./ci/live-swarm.sh --router "$SWARM_ROUTER" \
            --deadline "$SWARM_DEADLINE" --seed-for "$SWARM_SEED" \
            "$SWARM" || true
    fi
else
    skip "tier3: live swarm" "no --swarm torrent given"
fi

# ------------------------------------------------------------------- tier 2

say ""
say "########## tier 2 — live routers (loopback topology) ##########"

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
        echo "unit state:   $(systemctl --user is-active "$(unit_of "$r")" 2>/dev/null | head -1 || true)"
        if podman container exists "$container" 2>/dev/null; then
            echo "container:    present"
            echo "image:        $(podman inspect -f '{{.ImageName}}' "$container" 2>/dev/null || echo unknown)"
            echo "started:      $(podman inspect -f '{{.State.StartedAt}}' "$container" 2>/dev/null || echo unknown)"
            echo "restarts:     $(podman inspect -f '{{.RestartCount}}' "$container" 2>/dev/null || echo unknown)"
            # Shared with ci/live-swarm.sh, so the two reports cannot drift
            # into disagreeing about what version was under test.
            echo "version:      $(./ci/router-version.sh "$r" 2>/dev/null || echo unknown)"
        else
            echo "container:    absent"
        fi
        echo "SAM port:     $(port_answers "$port" && echo answering || echo 'not answering')"
        # Is the running container actually built from the quadlet in this
        # checkout? A report once showed a Java router logging "EXT_PORT is
        # unset" — firewalling itself and skewing every result — while the
        # repo's quadlet had set EXT_PORT for weeks. The container simply
        # predated it: `make router-up` copies the file but will not recreate a
        # container that is already running, and nothing said so. An hour of
        # test results against a stale router is worse than no results.
        quadlet_src="contrib/podman/$(quadlet_of "$r")"
        quadlet_dst="$HOME/.config/containers/systemd/$(quadlet_of "$r")"
        if ! podman container exists "$container" 2>/dev/null; then
            # SAM answers but no container: a host-installed router. Often the
            # *better* subject — a daemon that has been up for weeks has a warm
            # netDb and real peer profiles, which no freshly reseeded container
            # can match. Nothing to warn about; the only thing worth saying is
            # that the repo's quadlet settings are not in play, so its transport
            # port and firewall status live in that router's own config.
            echo "quadlet:      n/a — host-installed router, not a container."
            echo "              (Often the better subject: a long-running router"
            echo "              has a warmer netDb than a fresh container. The"
            echo "              repo's transport-port settings do not apply — see"
            echo "              this router's own config.)"
        elif [ ! -f "$quadlet_dst" ]; then
            echo "quadlet:      NOT INSTALLED at $quadlet_dst"
        elif cmp -s "$quadlet_src" "$quadlet_dst"; then
            echo "quadlet:      matches this checkout"
            # Installed file is current, but the container may still predate it.
            if podman container exists "$container" 2>/dev/null; then
                started=$(podman inspect -f '{{.State.StartedAt}}' "$container" 2>/dev/null || echo)
                changed=$(date -Is -r "$quadlet_dst" 2>/dev/null || echo)
                echo "quadlet mtime:$changed"
                echo "  (container started $started — if that is EARLIER, the"
                echo "   container predates the config: make router-down router-up)"
            fi
        else
            echo "quadlet:      STALE — $quadlet_dst differs from $quadlet_src"
            echo "  Results below are against a router built from older config."
            echo "  Refresh with: make router-down ROUTER=$r && make router-up ROUTER=$r"
            diff -u "$quadlet_dst" "$quadlet_src" 2>/dev/null | head -20 | sed 's/^/  /'
        fi
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
        say ""
        say "=== $r: why it is not answering"
        router_diagnosis "$r" 2>&1 | sanitise >> "$OUT"
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

    # Everything from here down dials a destination whose listener is on *this*
    # router, so all of it needs the inbound half — which a containerized router
    # cannot give us (see `is_containerised`). Say so once and move on, rather
    # than spending READY_DEADLINE proving it again and filing six red rows
    # about clove for a namespace boundary.
    if is_containerised "$r"; then
        skip "$r: readiness (one stream)" \
            "router runs in a container — the inbound path needs a shared netns (§3.1)"
        skip "$r: live tests" "needs the inbound half; see above"
        for n in $STRESS; do
            skip "$r: sam-stress N=$n" "needs the inbound half; see above"
        done
        note ""
        note "To test the inbound half against this router, run clove inside its"
        note "network namespace:  podman run --network=container:$container …"
        note "or use a host-installed router. LIVE-TESTING.md §3.1 has the detail."
        continue
    fi

    # One dial before many. A router that cannot carry a single stream will
    # not carry sixteen, and finding that out costs four minutes here versus
    # five minutes per stress level below.
    #
    # The outer timeout is derived from the budget we hand the probe, never
    # written independently. It was once a bare `180` next to a 240s
    # READY_DEADLINE, so the wrapper killed every probe a minute before its own
    # deadline and turned three FAILs into three uninformative TIMEOUTs — the
    # same nested-budget mistake this script exists to report, reintroduced one
    # layer up from where it was fixed. One owner for the number, and the shell
    # asks rather than assumes.
    if ! step "$r: readiness (one stream)" "$((READY_DEADLINE + STEP_SLACK))" \
        make router-ready ROUTER="$r" READY_DEADLINE="$READY_DEADLINE"; then
        skip "$r: live tests" "failed readiness — see above"
        for n in $STRESS; do
            skip "$r: sam-stress N=$n" "router not ready"
        done
        if podman container exists "$container" 2>/dev/null; then
            say ""
            say "=== $r: container log (readiness failed)"
            podman logs --tail 120 "$container" 2>&1 | sanitise >> "$OUT"
        fi
        continue
    fi

    live_ok=yes
    step "$r: live tests" "$T_LIVE" make test-live ROUTER="$r" || live_ok=no

    for n in $STRESS; do
        step "$r: sam-stress N=$n" "$T_STRESS" \
            env CLOVE_STRESS_DEADLINE="$STRESS_DEADLINE" \
            make sam-stress ROUTER="$r" N="$n" || true
    done

    # Logs are worth having whenever anything failed: the router's account of
    # a failed dial is usually more informative than ours.
    if [ "$live_ok" = no ] && podman container exists "$container" 2>/dev/null; then
        say ""
        say "=== $r: container log after failure"
        podman logs --tail 120 "$container" 2>&1 | sanitise >> "$OUT"
    fi
done

# -------------------------------------------------------------- cross-router

# A destination on one router, dialed from another — the path a swarm peer
# actually takes.
#
# Every run so far tested two sessions on a *single* router, and that may be the
# harder case rather than the simpler one: emissary resolves a same-router
# destination through a full netDb lookup and times out instead of using the
# leaseSet it already holds (PROTOCOL.i2p-bt 2.6c). So a same-router failure
# does not tell us clove is wrong, and until a cross-router pair has been tried
# the loopback checklist cannot distinguish the two. Runs regardless of whether
# readiness passed, because when readiness fails this is exactly the question.
say ""
say "########## cross-router (a destination on A, dialed from B) ##########"

live_routers=""
for r in $ROUTERS; do
    p=$(sam_port_of "$r")
    [ -n "$p" ] && port_answers "$p" && live_routers="$live_routers $r"
done

if [ -z "$live_routers" ] || [ "$(echo $live_routers | wc -w)" -lt 2 ]; then
    skip "cross-router" "needs two routers answering; have:${live_routers:- none}"
else
    for a in $live_routers; do
        # Only the *listener's* router needs the inbound half, so a
        # containerized router is still a perfectly good dialer. Skipping the
        # pair rather than the router keeps the half of the matrix that a
        # host-installed listener can still answer.
        if is_containerised "$a"; then
            for b in $live_routers; do
                [ "$a" = "$b" ] && continue
                skip "cross: listen $a, dial $b" \
                    "listener is in a container — needs a shared netns (§3.1)"
            done
            continue
        fi
        for b in $live_routers; do
            [ "$a" = "$b" ] && continue
            step "cross: listen $a, dial $b" "$((READY_DEADLINE + STEP_SLACK))" \
                make sam-stress ROUTER="$a" DIAL="$b" N=1 \
                CLOVE_STRESS_DEADLINE="$READY_DEADLINE" || true
        done
    done
fi

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
