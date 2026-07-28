#!/bin/sh
# Tier 3 — the cold-water test: the real binaries, a real router, a real swarm.
#
#   ci/live-swarm.sh <magnet-uri|file.torrent>
#   ci/live-swarm.sh --router java --deadline 7200 'magnet:?xt=urn:btih:…'
#   ci/live-swarm.sh --help
#
# WHY THIS EXISTS, AND WHY IT IS NOT ANOTHER sam-stress
#
# Every live tier before this one measured clove against *itself*: two
# destinations on one router, dialing each other. That topology is a laboratory
# convenience, and the lab turned out to be harder than the field. It needs a
# destination created seconds ago to publish a leaseSet and another session on
# the same router to resolve it — the most fragile netDb operation a young
# router performs, and one that at least emissary cannot do at all
# (PROTOCOL.i2p-bt §2.6c, §2.8). Every recorded run died there, and none of
# those deaths was a clove bug.
#
# The swarm path asks the network for strictly less. Dialing a tracker or a
# swarm peer resolves a destination that has been published for months, by a
# router that wants to be found. Our own leaseSet does not have to be
# resolvable by anyone for the download half to work at all: I2P bundles the
# sender's leaseSet with the stream's opening message, so the far side answers
# without a lookup. The thing that has been blocking the sign-off is not on the
# path this script takes.
#
# And it proves more. A completed download exercises the tracker announce, BEP
# 9 metadata, the picker, the choker, storage, verification and the resume
# writer against i2psnark — the client SCOPE §6 calls normative — over real
# tunnels. Bytes served prove the wire in the other direction. An inbound peer
# proves STREAM FORWARD and that our leaseSet reached the wider netDb, which is
# the half of §2.5 no router-free test can reach.
#
# WHAT IT REPORTS
#
# Milestones, each with the time it was reached, not one pass/fail. A run that
# connects to peers and stalls at zero pieces has said something specific and
# useful; a run that reports only "FAIL" has said nothing. The exit status is 0
# if the download completed, 1 otherwise — but read the table either way.
#
# You supply the torrent. Nothing is hardcoded on purpose: a magnet baked into
# this file would be dead within months and every failure after that would be
# blamed on clove, which is the exact failure mode this script exists to end.
# Pick a well-seeded one from a tracker index (postman, dg-tracker) or from
# i2psnark's own torrent list, and prefer something small enough to finish in
# an hour — this is a correctness test, not a benchmark.
set -eu

usage() {
    cat <<'USAGE'
usage: ci/live-swarm.sh [options] <magnet-uri | file.torrent>

  --router NAME     i2pd | java | emissary — selects the SAM port (default i2pd)
  --router-version V  the router's version string, recorded in the report
  --sam-port N      explicit SAM port; overrides --router
  --deadline SECS   whole-run budget, download phase included (default 3600)
  --seed-for SECS   keep seeding after the download completes (default 900)
  --poll SECS       how often to sample the daemon (default 15)
  --data-dir DIR    daemon data directory (default: a temp dir, removed at exit)
  --keep            keep the data directory and say where it is
  --out FILE        report path (default: live-swarm-<timestamp>.txt)
  --help            this

The torrent is yours to choose; see the note at the top of this file.
Exit status: 0 if the download completed, 1 if it did not.
USAGE
}

ROUTER=i2pd
ROUTER_VERSION=""
SAM_PORT=""
DEADLINE=3600
SEED_FOR=900
POLL=15
DATA_DIR=""
KEEP=no
OUT=""
SUBJECT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --router)    ROUTER="${2:?--router needs a value}"; shift ;;
        --router-version) ROUTER_VERSION="${2:?--router-version needs a value}"; shift ;;
        --sam-port)  SAM_PORT="${2:?--sam-port needs a value}"; shift ;;
        --deadline)  DEADLINE="${2:?--deadline needs a value}"; shift ;;
        --seed-for)  SEED_FOR="${2:?--seed-for needs a value}"; shift ;;
        --poll)      POLL="${2:?--poll needs a value}"; shift ;;
        --data-dir)  DATA_DIR="${2:?--data-dir needs a value}"; shift ;;
        --keep)      KEEP=yes ;;
        --out)       OUT="${2:?--out needs a value}"; shift ;;
        --help|-h)   usage; exit 0 ;;
        -*)          echo "unknown option $1 (try --help)" >&2; exit 2 ;;
        *)
            [ -z "$SUBJECT" ] || { echo "give exactly one torrent" >&2; exit 2; }
            SUBJECT="$1"
            ;;
    esac
    shift
done

if [ -z "$SUBJECT" ]; then
    echo "ci/live-swarm.sh: no torrent given." >&2
    echo "Pass a magnet URI or a .torrent path — see --help and the note at the" >&2
    echo "top of this script for why none is built in." >&2
    exit 2
fi

case "$ROUTER" in
    i2pd)     : "${SAM_PORT:=7656}" ;;
    java)     : "${SAM_PORT:=7666}" ;;
    emissary) : "${SAM_PORT:=7676}" ;;
    *)
        if [ -z "$SAM_PORT" ]; then
            echo "unknown router '$ROUTER'; use --sam-port to name its SAM port" >&2
            exit 2
        fi
        ;;
esac

# A .torrent path is resolved before the cd below, so a relative path means
# what the operator typed rather than something under the checkout.
case "$SUBJECT" in
    magnet:*) ;;
    /*)       [ -f "$SUBJECT" ] || { echo "no such file: $SUBJECT" >&2; exit 2; } ;;
    *)
        [ -f "$SUBJECT" ] || { echo "no such file: $SUBJECT" >&2; exit 2; }
        SUBJECT="$PWD/$SUBJECT"
        ;;
esac
case "${OUT:-}" in
    "" | /*) ;;
    *) OUT="$PWD/$OUT" ;;
esac

cd "$(dirname "$0")/.."
[ -n "$OUT" ] || OUT="live-swarm-$(date +%Y%m%d-%H%M%S).txt"
: > "$OUT"

cloved="$PWD/target/release/cloved"
clove="$PWD/target/release/clove"

# ------------------------------------------------------------------ plumbing

START=$(date +%s)
elapsed() { echo $(($(date +%s) - START)); }

say() { printf '%s\n' "$*" | tee -a "$OUT"; }
note() { printf '%s\n' "$*" >> "$OUT"; }
stamp() { printf '[%5ss] %s\n' "$(elapsed)" "$*" | tee -a "$OUT"; }

# Milestones, in the order a healthy run reaches them. Each is recorded with
# the second it was first observed, and stays unset otherwise. Held as
# newline-separated "name<TAB>seconds" so POSIX sh needs no arrays.
MILESTONES=""
REACHED=""

# Every milestone this run could reach, and what each one proves. Printed in
# this order at the end whether or not it was reached, because an empty row is
# information: it says exactly how far the run got.
milestone_list() {
    cat <<'LIST'
daemon-up	cloved answers the control socket
router-connected	SAM session up and STREAM FORWARD accepted (PROTOCOL §2.7, §2.5)
torrent-added	the daemon took the magnet or .torrent
metadata	info dictionary fetched over BEP 9 from a live peer
peers-known	the tracker (or PEX) returned peer destinations
peer-connected	we dialed a real swarm peer and handshaked (§1.2)
first-bytes	a peer sent us payload
first-piece	a piece arrived and verified against the metainfo
download-complete	M3: full download from a live swarm
pex-acquisition	M3: peers learned via i2p_pex beyond the tracker's set (§4.3)
bytes-served	M3: we served payload to a swarm peer
inbound-peer	a remote peer dialed our destination (§2.5, the inbound half)
LIST
}

reach() {
    case "$REACHED" in
        *"|$1|"*) return 0 ;;
    esac
    REACHED="$REACHED|$1|"
    MILESTONES="$MILESTONES$1	$(elapsed)
"
    stamp "reached: $1"
}

reached() {
    case "$REACHED" in
        *"|$1|"*) return 0 ;;
        *) return 1 ;;
    esac
}

at_of() {
    printf '%s' "$MILESTONES" | awk -F'\t' -v want="$1" '$1 == want { print $2; exit }'
}

DAEMON_PID=""
cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    if [ "$KEEP" = yes ] && [ -n "${DATA_DIR:-}" ]; then
        say "data directory kept at $DATA_DIR"
    elif [ "$KEEP" = no ] && [ -n "${TEMP_DATA:-}" ]; then
        rm -rf "$TEMP_DATA"
    fi
}
trap cleanup EXIT INT TERM

# One CLI call, always bounded. A daemon that stops answering must not turn
# into a script that never returns.
cl() { timeout 30 "$clove" -c "$CONF" "$@" 2>>"$OUT"; }

# The same, for probes whose failure is an expected state rather than news —
# `show` against a magnet that has not resolved yet. Its stderr goes nowhere:
# a poll loop that reports one expected 404 per tick fills the report with the
# one thing the reader already knows and buries what they do not.
cl_quiet() { timeout 30 "$clove" -c "$CONF" "$@" 2>/dev/null; }

# Pull one scalar out of the daemon's JSON. The objects are flat and
# hand-encoded (`"key":value`), and the nested arrays in `show` use keys that
# do not collide with any read here.
field() {
    printf '%s' "$2" | sed -n "s/.*\"$1\":\\([0-9][0-9]*\\).*/\\1/p" | head -1
}
# String fields stop at the first *unescaped* quote and are then unescaped, so
# a value carrying a quote — an error message quoting a hostname, say — is not
# truncated at it. The daemon escapes newlines too, so a value always stays on
# one line.
field_str() {
    printf '%s' "$2" \
        | sed -nE "s/.*\"$1\":\"(([^\"\\\\]|\\\\.)*)\".*/\\1/p" \
        | head -1 \
        | sed -e 's/\\"/"/g' -e 's/\\n/ /g' -e 's/\\\\/\\/g'
}
# A missing or unparsable number reads as zero, so arithmetic below is total.
num() { n=$(field "$1" "$2"); echo "${n:-0}"; }

human() {
    awk -v b="${1:-0}" 'BEGIN {
        split("B KiB MiB GiB TiB", u, " ")
        i = 1
        while (b >= 1024 && i < 5) { b /= 1024; i++ }
        printf (i == 1 ? "%d %s\n" : "%.1f %s\n"), b, u[i]
    }'
}

# ---------------------------------------------------------------- the run

say "clove live swarm run"
say "generated:  $(date -Is)"
say "report:     $OUT"
# The version is not discoverable over SAM — `HELLO REPLY VERSION=` is the
# SAM protocol's version, not the router's — and every router publishes its
# own somewhere different. So it is the operator's to pass, and its absence is
# said out loud: LIVE-TESTING §6.3 requires a version against every result,
# because "works on i2pd" is worth very little a year from now, and three
# routers were once compared in one sitting with only one version recorded.
say "router:     $ROUTER ${ROUTER_VERSION:-(version not recorded)} (SAM 127.0.0.1:$SAM_PORT)"
say "subject:    $SUBJECT"
say "budget:     ${DEADLINE}s download, then ${SEED_FOR}s seeding"
{
    echo "uname:      $(uname -srmo 2>/dev/null || uname -a)"
    echo "rustc:      $(rustc --version 2>/dev/null || echo 'not found')"
    echo "commit:     $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "branch:     $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
    echo "dirty:      $(git status --porcelain 2>/dev/null | wc -l) file(s)"
} >> "$OUT"

say ""
say "=== building the binaries (release)"
if ! timeout 900 cargo build --workspace --release >>"$OUT" 2>&1; then
    say "build failed — see $OUT"
    exit 1
fi
[ -x "$cloved" ] && [ -x "$clove" ] || { say "binaries missing after build"; exit 1; }

# The clock starts here, not at invocation. A cold `target/` can spend minutes
# linking, and charging that to the download budget would make --deadline mean
# something different on every machine — and would put a build in the middle of
# every milestone timestamp.
START=$(date +%s)
say "build done; the clock starts now."

if [ -z "$DATA_DIR" ]; then
    TEMP_DATA=$(mktemp -d "${TMPDIR:-/tmp}/clove-swarm.XXXXXX")
    DATA_DIR="$TEMP_DATA"
fi
mkdir -p "$DATA_DIR"
CONF="$DATA_DIR/clove.conf"
cat > "$CONF" <<EOF
data_dir $DATA_DIR
api_socket $DATA_DIR/clove.sock
sam_address 127.0.0.1:$SAM_PORT
EOF
note ""
note "--- config"
note "$(cat "$CONF")"

say ""
say "=== starting cloved"
"$cloved" -c "$CONF" >>"$DATA_DIR/cloved.log" 2>&1 &
DAEMON_PID=$!

i=0
until cl status >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -gt 300 ]; then
        say "the daemon never answered; its log:"
        tail -40 "$DATA_DIR/cloved.log" | tee -a "$OUT"
        exit 1
    fi
    kill -0 "$DAEMON_PID" 2>/dev/null || {
        say "the daemon exited during startup; its log:"
        tail -40 "$DATA_DIR/cloved.log" | tee -a "$OUT"
        exit 1
    }
    sleep 0.2
done
reach daemon-up

# The sandbox line is worth capturing on every live run: LIVE-TESTING §6.1's
# last box is "layer 2 is actually on, and everything still passes with it
# enforced", and container CI cannot see Landlock at all.
note ""
note "--- daemon startup log"
head -20 "$DATA_DIR/cloved.log" >> "$OUT" 2>/dev/null || true
sandbox=$(grep -i -m1 'sandbox' "$DATA_DIR/cloved.log" 2>/dev/null || true)
[ -n "$sandbox" ] && say "sandbox:    $sandbox"

# The router comes up on the supervisor's backoff, so this is a poll, not a
# single read. A router that never connects is the one failure worth aborting
# on: everything below needs a session.
#
# It gets a slice of the budget rather than all of it. cloved's supervisor
# backs off to at most a minute, so a reachable SAM bridge connects in seconds
# and an unreachable one will not become reachable by being waited on for an
# hour — it would just spend the session to say what it says here in five
# minutes. Derived from DEADLINE so a short run cannot ask for longer than it
# has (PROTOCOL.i2p-bt 2.6d).
ROUTER_WAIT=$((DEADLINE < 300 ? DEADLINE : 300))
say ""
say "=== waiting for the router (SAM 127.0.0.1:$SAM_PORT, up to ${ROUTER_WAIT}s)"
router=""
while [ "$(elapsed)" -lt "$ROUTER_WAIT" ]; do
    router=$(field_str router "$(cl status --json || true)")
    case "$router" in
        connected) break ;;
        unsupported-sam-address)
            say "the daemon rejected sam_address — check --sam-port"
            exit 1
            ;;
    esac
    sleep "$POLL"
done
if [ "$router" != connected ]; then
    say "the daemon never reached 'connected' in ${ROUTER_WAIT}s (last state:"
    say "${router:-unknown}). That is a router problem, not a swarm one: check the"
    say "SAM bridge answers on 127.0.0.1:$SAM_PORT and that the router has peers"
    say "and built tunnels. The daemon's own account:"
    tail -30 "$DATA_DIR/cloved.log" | tee -a "$OUT"
    exit 1
fi
reach router-connected

say ""
say "=== adding the torrent"
added=$(cl add "$SUBJECT") || { say "add failed: $added"; exit 1; }
say "$added"
INFO_HASH=$(printf '%s' "$added" | awk '{print $2}')
[ -n "$INFO_HASH" ] || { say "add printed no info-hash"; exit 1; }
reach torrent-added

is_magnet=no
case "$SUBJECT" in magnet:*) is_magnet=yes ;; esac
[ "$is_magnet" = no ] && reach metadata

# ------------------------------------------------------------- the poll loop
#
# One sampler drives both phases. The download phase runs to the deadline; the
# seeding phase starts when the download completes and runs for --seed-for on
# top, because "it downloaded" and "it can serve" are separate claims and the
# second one needs peers to come asking.

say ""
say "=== watching (poll ${POLL}s, deadline ${DEADLINE}s)"
say ""
printf '%-8s %-14s %6s %6s %5s %5s %5s %10s %10s\n' \
    TIME STATE PROG PEERS KNOWN PEX IN DOWN UP | tee -a "$OUT"

complete_at=""
last_body=""
last_print=-999
# A row every poll for two hours is a report nobody reads, so a sample is
# printed when something actually moved — and, failing that, every
# HEARTBEAT seconds, so a long seed still shows a live terminal rather than a
# hung one. The time column is excluded from the comparison, since it changes
# every poll by definition and would defeat the whole thing.
HEARTBEAT=300
# When to say out loud that a magnet is not resolving: a fifth of the download
# budget, floored at two minutes. Derived rather than fixed, so a short run
# still gets the warning and a long one is not nagged in its first minute.
METADATA_STALL=$((DEADLINE / 5))
[ "$METADATA_STALL" -lt 120 ] && METADATA_STALL=120
stall_warned=""
while :; do
    now=$(elapsed)
    if [ -n "$complete_at" ]; then
        [ "$((now - complete_at))" -ge "$SEED_FOR" ] && break
    elif [ "$now" -ge "$DEADLINE" ]; then
        break
    fi

    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        say ""
        say "the daemon died mid-run — that is a clove bug, and its log is below."
        tail -60 "$DATA_DIR/cloved.log" | tee -a "$OUT"
        break
    fi

    # `show` 404s until a magnet's metadata lands — expected, not an error,
    # and its complaint is muted for exactly that reason. The first version of
    # this loop let it through, and a run that spent nine minutes fetching
    # metadata produced thirty-four identical "no such torrent" lines and not
    # one word about what the fetch was doing.
    detail=$(cl_quiet show "$INFO_HASH" --json || true)
    if [ -z "$detail" ]; then
        listing=$(cl list --json || true)
        case "$listing" in
            *fetching-metadata*)
                # The pending entry carries the fetch's own account of itself:
                # rounds run, trackers reached, peers returned, and the reason
                # the last attempt did not work.
                rounds=$(num fetch_rounds "$listing")
                tok=$(num trackers_ok "$listing")
                tfail=$(num trackers_failed "$listing")
                known=$(num known_peers "$listing")
                tried=$(num peers_tried "$listing")
                why=$(field_str last_error "$listing")
                body=$(printf 'fetching-metadata  round %s · trackers %s ok / %s failed · %s peer(s) known, %s dialed' \
                    "$rounds" "$tok" "$tfail" "$known" "$tried")
                [ -n "$why" ] && body="$body
             last: $why"
                if [ "$body" != "$last_body" ] || [ "$((now - last_print))" -ge "$HEARTBEAT" ]; then
                    printf '%-8s %s\n' "${now}s" "$body" | tee -a "$OUT"
                    last_body="$body"
                    last_print=$now
                fi
                # Metadata is a prerequisite for everything below, so a magnet
                # stuck here is worth saying out loud once rather than at the
                # end of an hour. Derived from the budget, not a second
                # independent constant.
                if [ -z "$stall_warned" ] && [ "$now" -ge "$METADATA_STALL" ]; then
                    stall_warned=yes
                    say ""
                    say "note: ${now}s without metadata. Nothing below this can start until"
                    say "the info dictionary arrives. The line above says which stage is"
                    say "failing; if it is a tracker name, check the router's address book"
                    say "knows that host — a lookup that fails is negative-cached for up to"
                    say "30 minutes, so retries get rarer, not more frequent."
                    say ""
                fi
                ;;
            *) note "no detail and no pending entry for $INFO_HASH" ;;
        esac
        sleep "$POLL"
        continue
    fi
    [ "$is_magnet" = yes ] && reach metadata

    state=$(field_str state "$detail")
    pieces=$(num pieces "$detail")
    have=$(num have "$detail")
    peers=$(num peers "$detail")
    known=$(num known_peers "$detail")
    pex=$(num pex_peers "$detail")
    inbound=$(num inbound_peers "$detail")
    down=$(num downloaded "$detail")
    up=$(num uploaded "$detail")

    [ "$known" -gt 0 ] && reach peers-known
    [ "$peers" -gt 0 ] && reach peer-connected
    [ "$down" -gt 0 ] && reach first-bytes
    [ "$have" -gt 0 ] && reach first-piece
    [ "$pex" -gt 0 ] && reach pex-acquisition
    [ "$up" -gt 0 ] && reach bytes-served
    [ "$inbound" -gt 0 ] && reach inbound-peer

    pct=0
    [ "$pieces" -gt 0 ] && pct=$((have * 100 / pieces))
    body=$(printf '%-14s %5s%% %6s %5s %5s %5s %10s %10s' \
        "$state" "$pct" "$peers" "$known" "$pex" "$inbound" \
        "$(human "$down")" "$(human "$up")")
    if [ "$body" != "$last_body" ] || [ "$((now - last_print))" -ge "$HEARTBEAT" ]; then
        printf '%-8s %s\n' "${now}s" "$body" | tee -a "$OUT"
        last_body="$body"
        last_print=$now
    fi

    if [ -z "$complete_at" ] && [ "$pieces" -gt 0 ] && [ "$have" -eq "$pieces" ]; then
        reach download-complete
        complete_at=$now
        say ""
        say "download complete after ${now}s — seeding for ${SEED_FOR}s to prove"
        say "the other direction. Interrupt if you have seen enough."
        say ""
    fi

    sleep "$POLL"
done

# ------------------------------------------------------------------ verdict

# Re-hash everything on disk, which is the only claim that matters at the end:
# "downloaded" is our own accounting, and this is the independent check of it.
# The engine has to stop first — it is still writing otherwise, and the daemon
# refuses on that ground.
say ""
say "=== verifying what landed on disk"
cl pause "$INFO_HASH" >/dev/null 2>&1 || true
if verified=$(cl verify "$INFO_HASH"); then
    say "$verified"
    say "(pieces verified against the metainfo, independently of our own counters)"
else
    say "verify failed — the bytes on disk do not match the metainfo."
    say "If the download reported complete, that is a clove bug worth a report."
fi

say ""
say "=== final state"
cl show "$INFO_HASH" | tee -a "$OUT" || true

say ""
if [ -z "$ROUTER_VERSION" ]; then
    say "note: no --router-version was given, so this report cannot fill in"
    say "      LIVE-TESTING §6.3's version column. Re-run with e.g."
    say "      --router-version '$ROUTER 2.61.0' if you are recording a result."
    say ""
fi
say "########## milestones ##########"
milestone_list | while IFS='	' read -r name what; do
    at=$(at_of "$name")
    if [ -n "$at" ]; then
        printf '  %-19s %6ss  %s\n' "$name" "$at" "$what"
    else
        # ASCII dash, and the same column width as a reached row: printf pads
        # by bytes, so a UTF-8 em dash here would silently shift the column.
        printf '  %-19s %6s   %s\n' "$name" "-" "$what"
    fi
done | tee -a "$OUT"

say ""
if reached download-complete; then
    say "VERDICT: the download completed from a live swarm."
    reached bytes-served || say "  (nothing was served back — a longer --seed-for may be needed)"
    reached pex-acquisition || say "  (no peers arrived over PEX — M3's PEX row stays open)"
    reached inbound-peer || say "  (no peer dialed us — the inbound half of §2.5 stays open)"
    say ""
    say "Record this in docs/LIVE-TESTING.md §6.3 with the router version."
    exit 0
fi

say "VERDICT: the download did not complete. The milestone table says how far"
say "it got; the first empty row is where to look."
if reached peer-connected; then
    say "  Peers connected but the download stalled: that is clove's problem to"
    say "  answer — choking, the picker, or requests going unanswered."
elif reached peers-known; then
    say "  The tracker gave us peers and not one dial succeeded. Compare with"
    say "  'make cross' — if cross-router dials also fail, the router is the"
    say "  subject, not clove."
elif reached metadata; then
    say "  Metadata arrived but no peers were learned. The tracker answered"
    say "  once and has not since, or it has no peers for this info-hash."
elif reached torrent-added; then
    say "  The magnet never resolved, so nothing below it could start. The"
    say "  fetch rounds above name the failing stage; the daemon's own log"
    say "  has every attempt:"
    say "      grep 'metadata fetch' $DATA_DIR/cloved.log | tail -20"
    say "  A tracker name that will not resolve is the usual cause. Check the"
    say "  router knows the host — i2pd's console has an address book, and a"
    say "  b32 tracker URL sidesteps the question entirely."
fi
say ""
say "Report: $OUT"
exit 1
