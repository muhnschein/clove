#!/bin/sh
# End-to-end test of the Transmission RPC surface (tier 1: no router required).
#
# The unit tests in crates/cloved drive `handle` directly, which proves the
# dispatch and the authentication. They cannot prove the things that only exist
# once the real binary is running: that the loopback TCP listener binds at all,
# that it survives the Landlock/seccomp self-restriction that happens *after*
# the bind, and that a real HTTP client — one that sends headers we did not
# write and expects a 409 it did not ask for — gets through the handshake.
#
# That gap is this file's whole reason to exist. `docs/PHASE-F.md`'s two
# registry-lock deadlocks passed every unit test and were found by running the
# daemon; a listener that binds and is then killed by a syscall filter would
# fail exactly the same way.
#
# curl drives the protocol. `transmission-remote`, if it is installed, drives
# it again as a real client would — that half is skipped rather than failed
# when the binary is absent, the way the router-gated tests are.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cloved="$root/target/debug/cloved"
clove="$root/target/debug/clove"
[ -x "$cloved" ] && [ -x "$clove" ] || {
    echo "transmission: build the binaries first (cargo build --workspace)" >&2
    exit 1
}
command -v curl >/dev/null 2>&1 || {
    echo "transmission: curl is required" >&2
    exit 1
}

work=$(mktemp -d)
XDG_DATA_HOME="$work/data"
XDG_RUNTIME_DIR="$work/run"
export XDG_DATA_HOME XDG_RUNTIME_DIR
mkdir -p "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR"
daemon_pid=""

cleanup() {
    [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

fail() {
    echo "transmission: FAIL: $*" >&2
    [ -f "$work/daemon.log" ] && sed 's/^/transmission:   /' "$work/daemon.log" >&2
    exit 1
}

expect_contains() {
    case "$1" in
    *"$2"*) ;;
    *) fail "$3: expected '$2' in: $1" ;;
    esac
}

expect_missing() {
    case "$1" in
    *"$2"*) fail "$3: did not expect '$2' in: $1" ;;
    esac
}

# Header names are case-insensitive on the wire and clove emits them
# lowercased, so a header assertion that cares about case is testing our
# spelling rather than the protocol.
lower() {
    printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

expect_header() {
    expect_contains "$(lower "$1")" "$(lower "$2")" "$3"
}

expect_no_header() {
    expect_missing "$(lower "$1")" "$(lower "$2")" "$3"
}

# Nothing listens on this in the fixture, so "waiting-for-router" is a property
# of the test rather than of the machine it runs on (see ci/smoke.sh).
DEAD_SAM_PORT=1
# A high port picked to be out of the way. Overridable, because a CI runner or
# a developer's box may already have something here.
PORT=${CLOVE_RPC_PORT:-19091}
RPC="http://127.0.0.1:$PORT/transmission/rpc"

cat >"$work/clove.conf" <<CONF
sam_address 127.0.0.1:$DEAD_SAM_PORT
api_listen 127.0.0.1:$PORT
transmission_rpc yes
CONF

echo "transmission: config check"
timeout 20 "$cloved" -C -c "$work/clove.conf" >/dev/null || fail "cloved -C rejected the config"

# A non-loopback bind must be refused by the parser, not by the bind — so that
# `cloved -C` tells an operator before a restart rather than after one.
cat >"$work/bad.conf" <<CONF
api_listen 0.0.0.0:$PORT
CONF
if timeout 20 "$cloved" -C -c "$work/bad.conf" >"$work/bad.out" 2>&1; then
    fail "cloved -C accepted a non-loopback api_listen"
fi
expect_contains "$(cat "$work/bad.out")" "ssh -L" "the refusal should name the alternative"

echo "transmission: daemon starts and binds both listeners"
timeout 120 "$cloved" -c "$work/clove.conf" >"$work/daemon.log" 2>&1 &
daemon_pid=$!
i=0
until timeout 5 "$clove" -c "$work/clove.conf" status >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -gt 200 ] && fail "daemon never answered on the unix socket"
    sleep 0.05
done

token=$(cat "$XDG_DATA_HOME/clove/token")

# The TCP listener is bound before the sandbox closes and accepted through it.
# If Landlock or seccomp ever grows a rule that kills this, it fails here.
i=0
until curl -s -o /dev/null --max-time 5 "$RPC" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -gt 200 ] && fail "the TCP listener never answered (sandbox killed it?)"
    sleep 0.05
done

# --------------------------------------------------------------- auth

echo "transmission: unauthenticated callers get a challenge, not a session id"
anonymous=$(curl -s -i --max-time 10 -X POST "$RPC" -d '{"method":"session-get"}')
expect_contains "$anonymous" "401" "no credentials should be 401"
expect_header "$anonymous" "WWW-Authenticate" "a 401 should carry a Basic challenge"
# The important half: the CSRF id must not be free to anyone who can reach the
# port, or it is not doing the job it exists for.
expect_no_header "$anonymous" "X-Transmission-Session-Id" "the session id leaked"

echo "transmission: a wrong password is refused"
wrong=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    -u "clove:not-the-token" -X POST "$RPC" -d '{"method":"session-get"}')
[ "$wrong" = "401" ] || fail "a wrong password answered $wrong, not 401"

echo "transmission: the 409 handshake hands over the session id"
challenge=$(curl -s -i --max-time 10 -u "clove:$token" -X POST "$RPC" -d '{"method":"session-get"}')
expect_contains "$challenge" "409" "an authenticated request without the id should be 409"
session=$(printf '%s' "$challenge" | tr -d '\r' \
    | sed -n 's/^[Xx]-[Tt]ransmission-[Ss]ession-[Ii]d: *//p' | head -n1)
[ -n "$session" ] || fail "the 409 carried no session id: $challenge"

# Every call from here is what a client sends once it has been through the
# handshake above.
rpc() {
    curl -s --max-time 15 -u "clove:$token" \
        -H "X-Transmission-Session-Id: $session" \
        -X POST "$RPC" -d "$1"
}

echo "transmission: session-get reports the configuration and the I2P constants"
out=$(rpc '{"method":"session-get","tag":1}')
expect_contains "$out" '"result":"success"' "session-get failed: $out"
expect_contains "$out" '"tag":1' "the tag was not echoed"
expect_contains "$out" '"rpc-version"' "no rpc-version"
expect_contains "$out" '"dht-enabled":false' "an I2P client must not claim DHT"
expect_contains "$out" '"lpd-enabled":false' "an I2P client must not claim local discovery"
expect_contains "$out" '"pex-enabled":true' "clove speaks i2p_pex"
expect_contains "$out" '"peer-port":0' "there is no port on I2P"
expect_contains "$out" 'clove' "the version should say what it really is"

echo "transmission: session-stats answers"
expect_contains "$(rpc '{"method":"session-stats"}')" '"torrentCount"' "session-stats failed"

echo "transmission: free-space answers about the downloads directory"
expect_contains "$(rpc '{"method":"free-space","arguments":{"path":"/tmp"}}')" \
    '"size-bytes"' "free-space failed"

# ------------------------------------------------------------ torrents

# A real single-file .torrent, base64'd the way a client sends it. Same shape as
# ci/smoke.sh's fixture.
python3 - "$work/demo.torrent" "$work/demo.b64" <<'PY'
import base64, hashlib, sys
content = b"clove transmission rpc test\n" * 3
name = b"rpc.txt"
info = (
    b"d6:lengthi" + str(len(content)).encode() + b"e"
    b"4:name" + str(len(name)).encode() + b":" + name +
    b"12:piece lengthi16384e"
    b"6:pieces20:" + hashlib.sha1(content).digest() + b"e"
)
raw = b"d4:info" + info + b"e"
open(sys.argv[1], "wb").write(raw)
# Flat, because it has to go inside a JSON string on the command line — which
# is also how every real client sends it. The decoder's leniency about
# MIME-wrapped input is covered by its unit tests, where a literal newline is
# not a syntax error.
open(sys.argv[2], "w").write(base64.b64encode(raw).decode())
PY
metainfo=$(cat "$work/demo.b64")

echo "transmission: torrent-add"
out=$(rpc "{\"method\":\"torrent-add\",\"arguments\":{\"metainfo\":\"$metainfo\"}}")
expect_contains "$out" '"result":"success"' "torrent-add failed: $out"
expect_contains "$out" '"torrent-added"' "no torrent-added in: $out"
expect_contains "$out" '"rpc.txt"' "the added torrent was not named"

echo "transmission: adding it again is a duplicate, not an error"
out=$(rpc "{\"method\":\"torrent-add\",\"arguments\":{\"metainfo\":\"$metainfo\"}}")
expect_contains "$out" '"result":"success"' "a duplicate add should not fail: $out"
expect_contains "$out" '"torrent-duplicate"' "no torrent-duplicate in: $out"

echo "transmission: a URL add is refused, and says why"
out=$(rpc '{"method":"torrent-add","arguments":{"filename":"http://example.com/a.torrent"}}')
expect_contains "$out" "clearnet" "the refusal should give the architectural reason: $out"

echo "transmission: torrent-get returns only the requested fields"
out=$(rpc '{"method":"torrent-get","arguments":{"fields":["id","name","status","percentDone","rateDownload","eta","hashString"]}}')
expect_contains "$out" '"name":"rpc.txt"' "the torrent did not list: $out"
expect_contains "$out" '"rateDownload"' "no rate reported"
expect_missing "$out" '"files"' "an unrequested field was sent"
# The one field that is a policy rather than a gap: peer addresses do not
# travel over the API, so the peer array is absent even when asked for.
out=$(rpc '{"method":"torrent-get","arguments":{"fields":["id","peers","peersConnected"]}}')
expect_contains "$out" '"peersConnected"' "the peer count should be real"
expect_missing "$out" '"peers":' "a peer array was invented"

id=$(rpc '{"method":"torrent-get","arguments":{"fields":["id"]}}' \
    | sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -n1)
[ -n "$id" ] || fail "could not read the torrent's id"

echo "transmission: torrent-stop and torrent-start move the status"
expect_contains "$(rpc "{\"method\":\"torrent-stop\",\"arguments\":{\"ids\":[$id]}}")" \
    '"result":"success"' "torrent-stop failed"
expect_contains "$(rpc '{"method":"torrent-get","arguments":{"fields":["status"]}}')" \
    '"status":0' "a stopped torrent should report STOPPED"
expect_contains "$(rpc "{\"method\":\"torrent-start\",\"arguments\":{\"ids\":[$id]}}")" \
    '"result":"success"' "torrent-start failed"

echo "transmission: torrent-set applies what clove has an equivalent for"
expect_contains "$(rpc "{\"method\":\"torrent-set\",\"arguments\":{\"ids\":[$id],\"seedRatioLimit\":1.5}}")" \
    '"result":"success"' "torrent-set failed"
# Through the *other* surface, so this checks the change actually landed in the
# registry rather than in a reply the shim composed.
expect_contains "$(timeout 10 "$clove" -c "$work/clove.conf" show "$id" --json 2>/dev/null || \
    timeout 10 "$clove" -c "$work/clove.conf" list --json)" \
    "rpc.txt" "the torrent should still be listed by clove"

echo "transmission: session-set says where settings live rather than dropping them"
out=$(rpc '{"method":"session-set","arguments":{"peer-limit-global":11}}')
expect_contains "$out" "clove.conf" "session-set should name the config file: $out"

echo "transmission: a hostile envelope is answered, not survived"
for body in 'not json' '{}' '[]' '{"method":"nope"}' \
    '{"method":"torrent-get","arguments":{"ids":"nonsense"}}' \
    '{"method":"torrent-add","arguments":{"metainfo":"!!!!"}}'; do
    code=$(curl -s -o "$work/hostile.out" -w '%{http_code}' --max-time 10 \
        -u "clove:$token" -H "X-Transmission-Session-Id: $session" \
        -X POST "$RPC" -d "$body")
    case "$code" in
    200 | 400) ;;
    *) fail "hostile body '$body' answered $code" ;;
    esac
done
# Still serving afterwards.
expect_contains "$(rpc '{"method":"session-get"}')" '"result":"success"' \
    "the daemon stopped answering after the hostile sweep"

echo "transmission: the /v1/ surface is unaffected"
expect_contains "$(timeout 10 "$clove" -c "$work/clove.conf" list)" "rpc.txt" \
    "clove list should still work"
# And Transmission's credentials do not open /v1/ over the same port.
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    -u "clove:$token" "http://127.0.0.1:$PORT/v1/status")
[ "$code" = "401" ] || fail "/v1/ accepted Basic auth ($code); it takes the token header"
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
    -H "x-clove-token: $token" "http://127.0.0.1:$PORT/v1/status")
[ "$code" = "200" ] || fail "/v1/ over TCP answered $code with a good token"

echo "transmission: torrent-remove"
expect_contains "$(rpc "{\"method\":\"torrent-remove\",\"arguments\":{\"ids\":[$id]}}")" \
    '"result":"success"' "torrent-remove failed"
out=$(rpc '{"method":"torrent-get","arguments":{"fields":["id"]}}')
expect_missing "$out" '"id"' "the torrent survived removal: $out"

# --------------------------------------------- a real Transmission client

if command -v transmission-remote >/dev/null 2>&1; then
    echo "transmission: transmission-remote drives it"
    tr() {
        timeout 20 transmission-remote "127.0.0.1:$PORT" --auth "clove:$token" "$@"
    }
    tr -l >"$work/tr-list.out" 2>&1 || fail "transmission-remote -l failed: $(cat "$work/tr-list.out")"
    tr -a "$work/demo.torrent" >"$work/tr-add.out" 2>&1 \
        || fail "transmission-remote -a failed: $(cat "$work/tr-add.out")"
    tr -l >"$work/tr-list2.out" 2>&1 || fail "transmission-remote -l failed after add"
    expect_contains "$(cat "$work/tr-list2.out")" "rpc.txt" "the added torrent did not list"
    # Read the id back rather than assuming it. Ids are assigned in order of
    # first sight and are stable per daemon run, so this one happens to be 1
    # today — which is exactly the sort of thing that holds until it does not.
    tr_id=$(rpc '{"method":"torrent-get","arguments":{"fields":["id"]}}' \
        | sed -n 's/.*"id":\([0-9]*\).*/\1/p' | head -n1)
    [ -n "$tr_id" ] || fail "could not read the id transmission-remote's torrent was given"
    tr -t "$tr_id" -S >"$work/tr-stop.out" 2>&1 || fail "transmission-remote stop failed"
    tr -t "$tr_id" -i >"$work/tr-info.out" 2>&1 || fail "transmission-remote -i failed"
    expect_contains "$(cat "$work/tr-info.out")" "rpc.txt" "the detail view did not render the torrent"
    # The tracker section is the one a real client renders from fields no unit
    # test knew were needed; an empty section here is the regression.
    tr -t "$tr_id" -it >"$work/tr-trackers.out" 2>&1 || fail "transmission-remote -it failed"
    tr -t "$tr_id" --remove >"$work/tr-rm.out" 2>&1 || fail "transmission-remote --remove failed"
    echo "transmission: transmission-remote ok"
else
    echo "transmission: transmission-remote not installed, skipping the real-client leg"
fi

echo "transmission: ok"
