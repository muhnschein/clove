#!/bin/sh
# End-to-end daemon smoke test (tier 1: no router required).
#
# Unit tests exercise modules in isolation; this drives the real binaries the
# way an operator does — start cloved, talk to it with clove, assert what came
# back. It exists because that is the only thing that catches whole-process
# faults: the two registry-lock deadlocks in Phase F passed every unit test and
# were found only by running the daemon.
#
# No router is needed. The daemon reports "waiting-for-router" and torrents sit
# in that state, which is exactly what we assert. Live-router coverage is
# `make test-live` (docs/LIVE-TESTING.md).
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

start_daemon() {
    # A previous run's socket file may still be on disk; the daemon replaces
    # it, so waiting for the file alone would race. Wait for an answer.
    timeout 60 "$cloved" >"$work/daemon.log" 2>&1 &
    daemon_pid=$!
    i=0
    until timeout 5 "$clove" status >/dev/null 2>&1; do
        i=$((i + 1))
        [ "$i" -gt 200 ] && fail "daemon never answered (log: $(cat "$work/daemon.log"))"
        sleep 0.05
    done
}

stop_daemon() {
    kill "$daemon_pid" 2>/dev/null
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
}

# A real single-file .torrent: 72 bytes of content, one 16 KiB piece.
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
status=$(run status) || fail "clove status failed"
expect_contains "$status" "waiting-for-router" "status without a router"

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

echo "smoke: token file is 0600"
mode=$(stat -c '%a' "$XDG_DATA_HOME/clove/token")
[ "$mode" = "600" ] || fail "token mode is $mode, expected 600"

echo "smoke: ok"
