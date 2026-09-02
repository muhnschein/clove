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

# Real single-file .torrents: 75 bytes of content, one 16 KiB piece. Two of
# them, because a client that hosts one torrent is not the one being tested —
# the bulk commands need a second real torrent to act on.
python3 - "$work/demo.torrent" "$work/second.torrent" <<'PY'
import hashlib, sys

def build(path, name, content):
    piece_length = 16384
    info = (
        b"d6:lengthi" + str(len(content)).encode() + b"e"
        b"4:name" + str(len(name)).encode() + b":" + name +
        b"12:piece lengthi" + str(piece_length).encode() + b"e"
        b"6:pieces20:" + hashlib.sha1(content).digest() + b"e"
    )
    with open(path, "wb") as handle:
        handle.write(b"d4:info" + info + b"e")

build(sys.argv[1], b"smoke.txt", b"clove smoke test content\n" * 3)
build(sys.argv[2], b"second.txt", b"clove smoke second torrent\n" * 3)
PY

echo "smoke: config check"
timeout 20 "$cloved" -C >/dev/null || fail "cloved -C failed"

# The peer ceilings are numbers, and a typo in one has to fail the start
# rather than be discovered later as a wedged session.
cat >"$work/limits.conf" <<EOF
data_dir $work/limits-data
peer_limit 80
torrent_peer_limit 12
EOF
timeout 20 "$cloved" -C -c "$work/limits.conf" >/dev/null || fail "valid peer limits rejected"
# A key that no longer exists must fail the start loudly rather than be
# read past — the whole point of unknown keys being fatal.
for bad in "peer_limit 0" "peer_limit lots" "torrent_peer_limit 0" \
    "watch_dir /srv/torrents/inbox" "i_know_sam_is_remote yes" \
    "max_active_downloads 3" "max_active_seeds 5"; do
    printf 'data_dir %s\n%s\n' "$work/limits-data" "$bad" >"$work/bad.conf"
    set +e
    timeout 20 "$cloved" -C -c "$work/bad.conf" >/dev/null 2>&1
    code=$?
    set -e
    [ "$code" -ne 0 ] || fail "cloved -C accepted \"$bad\""
done

echo "smoke: daemon starts and answers"
start_daemon
run status >/dev/null || fail "clove status failed"
expect_router_state "waiting-for-router"

# One data directory, one daemon. A second start on the same configuration
# used to unlink the first's socket and bind its own, and both then persisted
# into the same resume files. It now has to refuse, at once, saying why — and
# leave the first daemon answering on the socket it already had.
echo "smoke: a second daemon on the same data_dir refuses to start"
set +e
timeout 20 "$cloved" -c "$work/smoke.conf" >"$work/second.log" 2>&1
code=$?
set -e
[ "$code" -ne 0 ] || fail "a second cloved on the same data_dir started"
expect_contains "$(cat "$work/second.log")" "another cloved is running" "the second daemon's refusal"
kill -0 "$daemon_pid" 2>/dev/null || fail "the first daemon died when a second one tried to start"
run status >/dev/null || fail "the first daemon stopped answering after a second one tried to start"

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

# A magnet without its metadata has no engine to act on, but it is right
# there in the listing — so the refusal has to say that rather than claim the
# torrent does not exist.
set +e
out=$(run pause "$magnet_hash" 2>&1)
code=$?
set -e
expect_status "$code" 1 "pausing a magnet that has no metadata yet"
expect_contains "$out" "still fetching its metadata" "the refusal names the real state"

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

# `announce` has no CLI wrapper to drive any more: it is a versioned endpoint
# and nothing else. What this used to prove — that a torrent with no engine is
# refused rather than silently accepted — is
# `registry::tests::announce_now_refuses_a_torrent_that_is_not_running`, which
# also covers the rate limit and the unknown-hash case this never reached.

echo "smoke: a torrent answers to a unique prefix of its hash"
# The whole point of prefixes is not having to paste forty characters, so the
# proof has to run a real command against a short one.
prefix=$(printf '%s' "$info_hash" | cut -c1-6)
run pause "$prefix" >/dev/null || fail "pause by prefix failed"
expect_contains "$(run list)" "paused" "pause by prefix took effect"
run resume "$prefix" >/dev/null || fail "resume by prefix failed"
expect_contains "$(run show "$prefix")" "$info_hash" "show by prefix"

# Too short, or not hex, is not a torrent reference at all, and the CLI says
# so before it builds a request path out of it (exit 2, usage). The daemon
# applies the same rule to whatever reaches it; a well-formed reference that
# matches nothing is "no such torrent" from the daemon (exit 1), below.
for bad in abc zzzz 'abcd?data=1'; do
    set +e
    run pause "$bad" >/dev/null 2>&1
    code=$?
    set -e
    expect_status "$code" 2 "pause with an unusable reference ($bad)"
done

# Both torrents share no prefix by construction (one is ab-repeated), so an
# ambiguous case needs a second magnet that does. This is the failure the
# feature must never have: two candidates and a choice made.
amb_a="abcd$(printf '1%.0s' $(seq 36))"
amb_b="abcd$(printf '2%.0s' $(seq 36))"
run add "magnet:?xt=urn:btih:$amb_a&dn=amb-a" >/dev/null || fail "ambiguous magnet a"
run add "magnet:?xt=urn:btih:$amb_b&dn=amb-b" >/dev/null || fail "ambiguous magnet b"
set +e
out=$(run pause abcd 2>&1)
code=$?
set -e
expect_status "$code" 1 "an ambiguous prefix is refused"
expect_contains "$out" "matches 2 torrents" "the refusal says how many it hit"
run remove "$amb_a" >/dev/null || fail "removing ambiguous magnet a"
run remove "$amb_b" >/dev/null || fail "removing ambiguous magnet b"

echo "smoke: the listing is in add order and carries rates"
# The magnet was added after the torrent, so it comes second whatever the two
# info-hashes sort like — which is the point.
first_row=$(run list | sed -n '4p')
expect_contains "$first_row" "smoke.txt" "the first-added torrent lists first"
expect_contains "$(run list)" "▼" "the listing carries a rate column"
expect_contains "$(run list | sed -n '1p')" "clove " "the listing opens with a header bar"
expect_contains "$(run list | sed -n '3p')" "PEERS" "the listing carries a peer column"
expect_contains "$(run status --json)" '"down_rate"' "status carries client-wide rates"
expect_contains "$(run status --json)" '"peer_limit"' "status reports the peer budget"
# Nothing is moving, so every rate is a dash rather than a column of zeroes.
expect_contains "$(run show "$info_hash")" "down_rate" "detail carries rates"

echo "smoke: a torrent answers to its listing position"
run pause 1 >/dev/null || fail "pause by listing position failed"
expect_contains "$(run show 1 --json)" '"state":"paused"' "show by position"
run resume 1 >/dev/null || fail "resume by position failed"
set +e
out=$(run pause 99 2>&1)
code=$?
set -e
expect_status "$code" 1 "a position past the end of the listing"
expect_contains "$out" "no torrent at position 99" "the refusal names the position"

echo "smoke: several torrents in one command, and --all"
second=$(run add "$work/second.torrent") || fail "adding the second torrent failed"
second_hash=$(printf '%s' "$second" | awk '{print $2}')
[ -n "$second_hash" ] || fail "second add printed no info-hash: $second"

run pause "$info_hash" "$second_hash" >/dev/null || fail "pause of two torrents failed"
expect_contains "$(run show "$info_hash" --json)" '"state":"paused"' "first paused"
expect_contains "$(run show "$second_hash" --json)" '"state":"paused"' "second paused"
run resume "$info_hash" "$second_hash" >/dev/null || fail "resume of two torrents failed"

# One failure does not abandon the rest: the unknown hash fails, the real
# torrent is still acted on, and the command exits 1.
set +e
out=$(run pause "$info_hash" 0000000000000000000000000000000000000000 2>&1)
code=$?
set -e
expect_status "$code" 1 "a partial failure exits 1"
expect_contains "$out" "paused $info_hash" "the reachable torrent was still paused"
expect_contains "$out" "1 of 2 failed" "the summary counts the failure"

# --all reaches every torrent it can act on, whatever state they are in.
run resume --all >/dev/null || fail "resume --all failed"
expect_contains "$(run show "$info_hash" --json)" '"state":"waiting-for-router"' "resumed by --all"
run remove "$second_hash" >/dev/null || fail "removing the second torrent failed"

echo "smoke: status totals the client as well as the daemon"
expect_contains "$(run status)" "router" "status reports the router state"
expect_contains "$(run status)" "torrents" "status reports a torrent count"
expect_contains "$(run status)" "peers" "status reports peers against the budget"
expect_contains "$(run status)" "uploaded" "status reports lifetime bytes"
# The commands that went with the simplification must be gone, not aliased.
for removed in top watch stats announce peer start; do
    set +e
    run "$removed" >/dev/null 2>&1
    code=$?
    set -e
    expect_status "$code" 2 "$removed is no longer a command"
done

echo "smoke: add --paused and --sequential apply at add time"
flagged=$(run add --paused --sequential "$work/second.torrent") || fail "add with flags failed"
flagged_hash=$(printf '%s' "$flagged" | awk '{print $2}')
[ -n "$flagged_hash" ] || fail "flagged add printed no info-hash: $flagged"
expect_contains "$(run show "$flagged_hash" --json)" '"state":"paused"' "added paused"
expect_contains "$(run show "$flagged_hash" --json)" '"sequential":true' "added sequential"
run remove "$flagged_hash" >/dev/null || fail "removing the flagged torrent"

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
# torrent's files are created empty and grow as blocks land; with it the blocks
# are claimed from the moment the torrent is added, before any peer exists.
echo "smoke: preallocate claims the disk up front"
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
# Length alone is what a sparse file also has. Blocks are what was promised.
blocks=$(stat -c '%b' "$laid_out")
[ "$blocks" -gt 0 ] || fail "preallocated file holds no blocks; the space was not claimed"
kill "$pre_pid" 2>/dev/null
wait "$pre_pid" 2>/dev/null || true

echo "smoke: token file is 0600"
mode=$(stat -c '%a' "$XDG_DATA_HOME/clove/token")
[ "$mode" = "600" ] || fail "token mode is $mode, expected 600"

echo "smoke: ok"
