#!/bin/sh
# End-to-end daemon smoke test (tier 1: no router required).
#
# Unit tests exercise modules in isolation; this drives the real binaries the
# way an operator does — start cloved, talk to it with clove, assert what came
# back. It exists because that is the only thing that catches whole-process
# faults: the two registry-lock deadlocks in Phase F passed every unit test and
# were found only by running the daemon.
#
# No router is needed — and, importantly, none is *used* even if one happens to
# be running: the daemon is pointed at a loopback port nothing listens on, so
# "waiting-for-router" is a property of the fixture rather than of the machine.
# Live-router coverage is `make test-live` (docs/LIVE-TESTING.md).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cloved="$root/target/debug/cloved"
clove="$root/target/debug/clove"
[ -x "$cloved" ] && [ -x "$clove" ] || {
    echo "smoke: build the binaries first (cargo build --workspace)" >&2
    exit 1
}

work=$(mktemp -d)
XDG_DATA_HOME="$work/data"
XDG_RUNTIME_DIR="$work/run"
export XDG_DATA_HOME XDG_RUNTIME_DIR
mkdir -p "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR"
sock="$XDG_RUNTIME_DIR/clove.sock"
daemon_pid=""

cleanup() {
    [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

fail() {
    echo "smoke: FAIL: $*" >&2
    exit 1
}

# Every clove call is wrapped: a hang is a bug we want reported in seconds,
# not a CI job that runs until the platform timeout.
run() {
    timeout 20 "$clove" "$@"
}

expect_contains() {
    haystack=$1
    needle=$2
    what=$3
    case "$haystack" in
    *"$needle"*) ;;
    *) fail "$what: expected to find '$needle' in: $haystack" ;;
    esac
}

expect_status() {
    got=$1
    want=$2
    what=$3
    [ "$got" = "$want" ] || fail "$what: expected exit $want, got $got"
}

# A loopback port nothing listens on, so the SAM connect is refused at once and
# the daemon lands in "waiting-for-router" deterministically.
#
# Without this the smoke test pointed at the default SAM port and asserted
# "waiting-for-router" — which holds only on a machine with no router. It
# therefore failed on exactly the machines this script most wants to run on:
# an operator's box with a live i2pd on 7656, where the daemon reports
# "connecting" or "connected" and the assertion blows up on a working setup.
# Tier 1 must not depend on ambient machine state.
DEAD_SAM_PORT=1

start_daemon() {
    printf 'sam_address 127.0.0.1:%s\n' "$DEAD_SAM_PORT" > "$work/smoke.conf"
    # A previous run's socket file may still be on disk; the daemon replaces
    # it, so waiting for the file alone would race. Wait for an answer.
    timeout 60 "$cloved" -c "$work/smoke.conf" >"$work/daemon.log" 2>&1 &
    daemon_pid=$!
    i=0
    until timeout 5 "$clove" status >/dev/null 2>&1; do
        i=$((i + 1))
        [ "$i" -gt 200 ] && fail "daemon never answered (log: $(cat "$work/daemon.log"))"
        sleep 0.05
    done
}

# Wait for the daemon to report a router state, rather than sampling once.
#
# The daemon starts in "connecting" and only moves to "waiting-for-router"
# after its first SAM attempt fails, so a single read races that transition
# even against a dead port. The race is narrow, which is worse than wide: it
# passes locally and fails on a loaded CI runner.
expect_router_state() {
    want=$1
    i=0
    while :; do
        got=$("$clove" status 2>/dev/null || true)
        case "$got" in
        *"$want"*) return 0 ;;
        esac
        i=$((i + 1))
        [ "$i" -gt 200 ] && fail "router never reached '$want'; last status: $got"
        sleep 0.05
    done
}

stop_daemon() {
    kill "$daemon_pid" 2>/dev/null
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
}

# A real single-file .torrent: 75 bytes of content, one 16 KiB piece.
python3 - "$work/demo.torrent" <<'PY'
import hashlib, sys

content = b"clove smoke test content\n" * 3
name = b"smoke.txt"
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

echo "smoke: config check"
timeout 20 "$cloved" -C >/dev/null || fail "cloved -C failed"

echo "smoke: daemon starts and answers"
start_daemon
run status >/dev/null || fail "clove status failed"
expect_router_state "waiting-for-router"

echo "smoke: add, list, show"
added=$(run add "$work/demo.torrent") || fail "add failed"
info_hash=$(printf '%s' "$added" | awk '{print $2}')
[ -n "$info_hash" ] || fail "add printed no info-hash: $added"
expect_contains "$(run list)" "smoke.txt" "list after add"
expect_contains "$(run show "$info_hash")" "$info_hash" "show"

echo "smoke: duplicate add is refused"
set +e
run add "$work/demo.torrent" >/dev/null 2>&1
code=$?
set -e
expect_status "$code" 1 "duplicate add"

echo "smoke: magnet add stays pending without a router"
magnet_hash=$(printf 'ab%.0s' 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20)
run add "magnet:?xt=urn:btih:$magnet_hash&dn=smoke-magnet" >/dev/null || fail "magnet add failed"
expect_contains "$(run list)" "fetching-metadata" "magnet listing"

echo "smoke: pause persists across a restart"
run pause "$info_hash" >/dev/null || fail "pause failed"
expect_contains "$(run list)" "paused" "list after pause"
stop_daemon
start_daemon
expect_contains "$(run list)" "paused" "pause after restart"
expect_contains "$(run list)" "fetching-metadata" "magnet after restart"

echo "smoke: verify re-checks data on disk"
run verify "$info_hash" >/dev/null || fail "verify failed"

echo "smoke: priorities"
run priorities "$info_hash" 2 >/dev/null || fail "priorities failed"
expect_contains "$(run show "$info_hash")" "high" "priority in show"
# Displayed and persisted is where this used to stop: the engine was never
# told, so a file set to skip downloaded in full. Progress is the cheapest
# observable proof it arrives — nothing wanted is nothing outstanding, and
# this torrent holds none of its one piece.
expect_contains "$(run show "$info_hash" --json)" '"progress":0.0' "progress before skipping"
run priorities "$info_hash" 0 >/dev/null || fail "priorities skip failed"
expect_contains "$(run show "$info_hash" --json)" '"progress":1.0' "skipping the only file leaves nothing outstanding"
run priorities "$info_hash" 1 >/dev/null || fail "priorities normal failed"
expect_contains "$(run show "$info_hash" --json)" '"progress":0.0' "wanting it again reopens the torrent"

echo "smoke: sequential mode persists across a restart"
run sequential "$info_hash" on >/dev/null || fail "sequential on failed"
expect_contains "$(run show "$info_hash" --json)" '"sequential":true' "sequential in show"
stop_daemon
start_daemon
expect_contains "$(run show "$info_hash" --json)" '"sequential":true' "sequential after restart"
run sequential "$info_hash" off >/dev/null || fail "sequential off failed"
expect_contains "$(run show "$info_hash" --json)" '"sequential":false' "sequential back off"

set +e
run sequential "$info_hash" maybe >/dev/null 2>&1
code=$?
set -e
expect_status "$code" 2 "sequential with a bad setting"

echo "smoke: announce refuses a torrent with no router"
set +e
run announce "$info_hash" >/dev/null 2>&1
code=$?
set -e
expect_status "$code" 1 "announce without a running engine"

echo "smoke: resume, then remove both torrents"
run resume "$info_hash" >/dev/null || fail "resume failed"
run remove "$info_hash" --data >/dev/null || fail "remove failed"
run remove "$magnet_hash" >/dev/null || fail "removing the pending magnet failed"
expect_contains "$(run list)" "no torrents" "list after removals"

echo "smoke: error paths"
set +e
run remove 0000000000000000000000000000000000000000 >/dev/null 2>&1
code=$?
set -e
expect_status "$code" 1 "removing an unknown info-hash"

set +e
run bogus-command >/dev/null 2>&1
code=$?
set -e
expect_status "$code" 2 "unknown command"

set +e
timeout 20 "$clove" --socket "$work/nonexistent.sock" status >/dev/null 2>&1
code=$?
set -e
expect_status "$code" 3 "unreachable daemon"

# A configured data_dir/api_socket has to be honoured by *both* binaries. The
# CLI used to parse an empty configuration, so a clove.conf that moved either
# one left it looking for a token in a directory the daemon never used — and
# nothing here noticed, because everything above runs on the XDG defaults.
echo "smoke: a configured data_dir and socket are honoured end to end"
conf_dir="$work/conf"
mkdir -p "$conf_dir/data" "$conf_dir/run"
cat >"$conf_dir/clove.conf" <<EOF
data_dir $conf_dir/data
api_socket $conf_dir/run/clove.sock
EOF
timeout 60 "$cloved" -c "$conf_dir/clove.conf" >"$work/daemon-conf.log" 2>&1 &
conf_pid=$!
i=0
until timeout 5 "$clove" -c "$conf_dir/clove.conf" status >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -gt 200 ] && fail "configured daemon never answered (log: $(cat "$work/daemon-conf.log"))"
    sleep 0.05
done
[ -S "$conf_dir/run/clove.sock" ] || fail "the configured api_socket was not created"
[ -f "$conf_dir/data/token" ] || fail "the token was not created in the configured data_dir"
expect_contains "$(timeout 5 "$clove" -c "$conf_dir/clove.conf" list)" \
    "no torrents" "list against the configured daemon"
# Without -c the CLI looks at the default socket, which is a different daemon.
expect_router_state "waiting-for-router"
kill "$conf_pid" 2>/dev/null
wait "$conf_pid" 2>/dev/null || true

# `preallocate` is the one config key with a visible effect on disk, so it is
# the one that can be checked rather than merely parsed. Without it a fresh
# torrent's files are created empty and grow as blocks land; with it they are
# at full length from the moment the torrent is added, before any peer exists.
echo "smoke: preallocate lays files out at full length"
pre_dir="$work/prealloc"
mkdir -p "$pre_dir/data" "$pre_dir/run"
cat >"$pre_dir/clove.conf" <<EOF
data_dir $pre_dir/data
api_socket $pre_dir/run/clove.sock
preallocate yes
EOF
timeout 20 "$cloved" -C -c "$pre_dir/clove.conf" >/dev/null \
    || fail "cloved -C rejected a config with preallocate"
timeout 60 "$cloved" -c "$pre_dir/clove.conf" >"$work/daemon-prealloc.log" 2>&1 &
pre_pid=$!
i=0
until timeout 5 "$clove" -c "$pre_dir/clove.conf" status >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -gt 200 ] && fail "preallocating daemon never answered (log: $(cat "$work/daemon-prealloc.log"))"
    sleep 0.05
done
timeout 5 "$clove" -c "$pre_dir/clove.conf" add "$work/demo.torrent" >/dev/null \
    || fail "add against the preallocating daemon failed"
laid_out="$pre_dir/data/downloads/smoke.txt"
i=0
until [ -f "$laid_out" ]; do
    i=$((i + 1))
    [ "$i" -gt 200 ] && fail "preallocate never created $laid_out"
    sleep 0.05
done
size=$(stat -c '%s' "$laid_out")
[ "$size" = "75" ] || fail "preallocated file is $size bytes, expected the full 75"
kill "$pre_pid" 2>/dev/null
wait "$pre_pid" 2>/dev/null || true

echo "smoke: token file is 0600"
mode=$(stat -c '%a' "$XDG_DATA_HOME/clove/token")
[ "$mode" = "600" ] || fail "token mode is $mode, expected 600"

echo "smoke: ok"
