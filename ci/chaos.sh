#!/bin/sh
# Chaos tests: crash and I/O-failure resilience (docs/SCOPE.md §9).
#
# The promise clove makes about state is specific: "Crash at any point must
# never corrupt resume state — worst case is re-verification." That promise is
# implemented by writing every state file to a temporary and renaming it over
# the target, and it is worth exactly nothing untested. This script attacks it.
#
#   1. SIGKILL storm — kill the daemon repeatedly while it is persisting, and
#      require that it comes back every time with its torrents intact.
#   2. Torn temporaries — leave junk .tmp files behind and require that a
#      restart ignores them rather than reading one as state.
#   3. Unwritable state directory — take away write permission underneath a
#      running daemon and require that it reports the failure and survives,
#      with the last good state still on disk.
#
# No router and no privilege needed, so this runs in CI alongside the smoke
# test. It is deliberately bounded: a few seconds, not a soak.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cloved="$root/target/debug/cloved"
clove="$root/target/debug/clove"
[ -x "$cloved" ] && [ -x "$clove" ] || {
    echo "chaos: build the binaries first (cargo build --workspace)" >&2
    exit 1
}

# How many kill/restart cycles. Each one is a fresh chance to catch a write in
# flight; more is better, but CI time is not free.
CYCLES=${CHAOS_CYCLES:-12}

work=$(mktemp -d)
XDG_DATA_HOME="$work/data"
XDG_RUNTIME_DIR="$work/run"
export XDG_DATA_HOME XDG_RUNTIME_DIR
mkdir -p "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR"
state="$XDG_DATA_HOME/clove/state"
daemon_pid=""
churn_pid=""

cleanup() {
    [ -n "$churn_pid" ] && kill "$churn_pid" 2>/dev/null
    [ -n "$daemon_pid" ] && kill -9 "$daemon_pid" 2>/dev/null
    # The read-only test may leave the directory unwritable.
    [ -d "$state" ] && chmod u+w "$state" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

fail() {
    echo "chaos: FAIL: $*" >&2
    exit 1
}

run() {
    timeout 20 "$clove" "$@"
}

start_daemon() {
    # Not under `timeout`: the pid this records is the one the SIGKILL storm
    # below shoots, and SIGKILL cannot be forwarded. Wrapped, the kill took
    # out timeout(1) and left cloved running as an orphan; the next "restart"
    # then started a second daemon on the same data_dir, which unlinked the
    # orphan's socket and took over — twelve daemons deep by the end, all
    # persisting into one state directory, and the test passing. The
    # instance lock now refuses that second daemon, which is how this came
    # to light. The trap kills the daemon on exit; that is the safety net.
    "$cloved" >>"$work/daemon.log" 2>&1 &
    daemon_pid=$!
    i=0
    until timeout 5 "$clove" status >/dev/null 2>&1; do
        i=$((i + 1))
        [ "$i" -gt 200 ] && fail "daemon never answered (log: $(tail -5 "$work/daemon.log"))"
        # If the daemon died outright, say so rather than spinning.
        kill -0 "$daemon_pid" 2>/dev/null || fail "daemon exited on start (log: $(tail -5 "$work/daemon.log"))"
        sleep 0.05
    done
}

python3 - "$work/chaos.torrent" <<'PY'
import hashlib, sys

content = b"clove chaos test content\n" * 3
name = b"chaos.txt"
piece_length = 16384
info = (
    b"d6:lengthi" + str(len(content)).encode() + b"e"
    b"4:name" + str(len(name)).encode() + b":" + name +
    b"12:piece lengthi" + str(piece_length).encode() + b"e"
    b"6:pieces20:" + hashlib.sha1(content).digest() + b"e"
)
with open(sys.argv[1], "wb") as handle:
    handle.write(b"d4:info" + info + b"e")
PY

echo "chaos: seeding a torrent"
start_daemon
added=$(run add "$work/chaos.torrent") || fail "add failed"
info_hash=$(printf '%s' "$added" | awk '{print $2}')
[ -n "$info_hash" ] || fail "add printed no info-hash"

# ---------------------------------------------------------------- 1. SIGKILL
# Each cycle drives a stream of state-writing operations, then kills the
# daemon without warning partway through one. SIGKILL cannot be caught, so
# there is no orderly shutdown path to hide behind: whatever is on disk is
# whatever the rename discipline left there.
echo "chaos: $CYCLES SIGKILL cycles during state writes"
cycle=1
while [ "$cycle" -le "$CYCLES" ]; do
    # Churn in the background: pause/resume each rewrite the resume file.
    (
        while :; do
            timeout 5 "$clove" pause "$info_hash" >/dev/null 2>&1
            timeout 5 "$clove" resume "$info_hash" >/dev/null 2>&1
            timeout 5 "$clove" priorities "$info_hash" 1 >/dev/null 2>&1
        done
    ) &
    churn_pid=$!

    # Land the kill somewhere inside the churn rather than at a fixed point,
    # so different cycles catch different moments.
    sleep "0.$((cycle % 5 + 1))"
    kill -9 "$daemon_pid" 2>/dev/null
    wait "$daemon_pid" 2>/dev/null || true
    # The storm has to land: a pid that is still alive here means the kill hit
    # something else (a wrapper, once), and every assertion after this point
    # would then be about a daemon that never died.
    kill -0 "$daemon_pid" 2>/dev/null && fail "the SIGKILL missed the daemon (pid $daemon_pid is still alive)"
    daemon_pid=""
    kill "$churn_pid" 2>/dev/null
    wait "$churn_pid" 2>/dev/null || true
    churn_pid=""

    # The whole point: it must come back, and the torrent must still be there.
    start_daemon
    listing=$(run list) || fail "cycle $cycle: list failed after SIGKILL"
    case "$listing" in
    *"chaos.txt"*) ;;
    *) fail "cycle $cycle: torrent lost after SIGKILL: $listing" ;;
    esac
    case "$(tail -20 "$work/daemon.log")" in
    *"skipping"*) fail "cycle $cycle: daemon refused its own state file: $(grep skipping "$work/daemon.log" | tail -1)" ;;
    *) ;;
    esac
    cycle=$((cycle + 1))
done

# ------------------------------------------------------------ 2. Torn temps
# A crash mid-write leaves a partial .tmp behind. It must never be mistaken
# for state: only the renamed target is real.
echo "chaos: restart ignores leftover temporaries"
kill "$daemon_pid" 2>/dev/null
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
printf 'this is not bencode' >"$state/$info_hash.resume.tmp"
printf 'neither is this' >"$state/$info_hash.torrent.tmp"
printf 'd' >"$state/0000000000000000000000000000000000000000.torrent"
start_daemon
listing=$(run list) || fail "list failed with leftover temporaries present"
case "$listing" in
*"chaos.txt"*) ;;
*) fail "torrent lost when temporaries were present: $listing" ;;
esac
# The bogus .torrent is skipped with a log line, not a crash — that is the
# designed behaviour for an unreadable state file.
grep -q "skipping" "$work/daemon.log" || fail "a corrupt state file was not reported"
rm -f "$state"/*.tmp "$state/0000000000000000000000000000000000000000.torrent"

# ------------------------------------------------ 3. Unwritable state (ENOSPC-shaped)
# Taking write permission away from the state directory makes every persist
# fail the way a full disk does. The daemon must report it and keep running,
# and the last good state must survive untouched.
# Root bypasses directory permission checks, so this section would pass
# without exercising anything at all. Say so and skip rather than claim
# coverage we did not get; CI runners are unprivileged, where it does run.
if [ "$(id -u)" = "0" ]; then
    echo "chaos: SKIP unwritable-state test (running as root; permissions are not enforced)"
else
    echo "chaos: unwritable state directory"
    before=$(wc -c <"$state/$info_hash.resume")
    chmod u-w "$state"
    set +e
    run pause "$info_hash" >/dev/null 2>&1
    pause_code=$?
    set -e
    chmod u+w "$state"

    # The write cannot succeed, so the API must report the failure rather than
    # claim a state change it did not persist.
    [ "$pause_code" -ne 0 ] || fail "pause reported success while the state directory was unwritable"
    # And the daemon must survive a failed write.
    kill -0 "$daemon_pid" 2>/dev/null || fail "daemon died when the state directory became unwritable"
    run status >/dev/null || fail "daemon stopped answering after a failed state write"

    after=$(wc -c <"$state/$info_hash.resume")
    [ "$before" = "$after" ] || fail "resume file changed while writes were failing"

    # And the untouched state still decodes: a restart proves it.
    kill "$daemon_pid" 2>/dev/null
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
    start_daemon
    listing=$(run list) || fail "list failed after the unwritable-directory episode"
    case "$listing" in
    *"chaos.txt"*) ;;
    *) fail "torrent lost after failed writes: $listing" ;;
    esac
fi

echo "chaos: ok"
