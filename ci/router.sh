#!/bin/sh
# Drive cloved against a fake SAM bridge (tier 2: the network path, no router).
#
# `ci/smoke.sh` points the daemon at a dead port on purpose, so everything past
# "waiting-for-router" is untested by it: session creation, the forwarded inbound
# listener, naming lookup, dialling out, an announce, a peer. Those are the parts
# every live run has broken in, and the parts nothing router-free could reach —
# so `ci/fake-sam.py` answers as a router does and this drives the daemon through
# them.
#
# Two things it asserts, and they are different claims:
#
#   1. The daemon brings the whole tree up and uses it. The bridge must see a
#      SESSION CREATE, a STREAM FORWARD, a NAMING LOOKUP and a STREAM CONNECT,
#      and `clove status` must say `connected`.
#   2. With --trace, the post-init seccomp allowlist is complete: no syscall the
#      daemon makes after the filter is installed comes back EPERM. This is what
#      keeps `cloved`'s ALLOWED list honest as the daemon learns to do new things,
#      and it is how that list was derived in the first place (SCOPE §5).
#
# --trace needs strace and is skipped without it, like man-lint without mandoc.
set -u

root=$(cd "$(dirname "$0")/.." && pwd)
cloved="$root/target/debug/cloved"
clove="$root/target/debug/clove"
[ -x "$cloved" ] && [ -x "$clove" ] || {
    echo "router: build the binaries first (cargo build --workspace)" >&2
    exit 1
}

trace=no
[ "${1:-}" = "--trace" ] && trace=yes

command -v python3 >/dev/null 2>&1 || {
    echo "router: SKIP (python3 is not installed; it runs the fake bridge)"
    exit 0
}
if [ "$trace" = yes ] && ! command -v strace >/dev/null 2>&1; then
    echo "router: SKIP --trace (strace is not installed; apt install strace)"
    trace=no
fi

work=$(mktemp -d)
XDG_DATA_HOME="$work/data"
XDG_RUNTIME_DIR="$work/run"
XDG_CONFIG_HOME="$work/config"
export XDG_DATA_HOME XDG_RUNTIME_DIR XDG_CONFIG_HOME
mkdir -p "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/clove"

sam_pid=""
daemon_pid=""
cleanup() {
    [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
    [ -n "$sam_pid" ] && kill "$sam_pid" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

fail() {
    echo "router: FAIL: $*" >&2
    echo "--- daemon ---" >&2
    cat "$work/daemon.log" >&2 2>/dev/null
    echo "--- bridge ---" >&2
    cat "$work/sam.log" >&2 2>/dev/null
    exit 1
}

# A free loopback port for the bridge, chosen the same way the daemon's own
# forward listener does it: bind 0 and ask what you got.
sam_port=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')

# A one-piece torrent with an I2P tracker, so the announce path runs.
info_hash=$(python3 - "$work/t.torrent" <<'PY'
import hashlib, sys
content = bytes(range(256)) * 64          # 16384 bytes: exactly one piece
pieces = hashlib.sha1(content).digest()
info = (b"d6:lengthi" + str(len(content)).encode() + b"e4:name9:trace.bin"
        + b"12:piece lengthi16384e6:pieces20:" + pieces + b"e")
open(sys.argv[1], "wb").write(
    b"d8:announce26:http://tracker.trace.i2p/a4:info" + info + b"e")
print(hashlib.sha1(info).hexdigest())
PY
)

printf 'sam_address 127.0.0.1:%s\n' "$sam_port" > "$XDG_CONFIG_HOME/clove/clove.conf"

python3 "$root/ci/fake-sam.py" "$sam_port" "$info_hash" 2>"$work/sam.log" &
sam_pid=$!
sleep 0.5

if [ "$trace" = yes ]; then
    strace -f -qq -o "$work/trace.txt" -s 200 "$cloved" >"$work/daemon.log" 2>&1 &
else
    "$cloved" >"$work/daemon.log" 2>&1 &
fi
daemon_pid=$!

# Wait for the session rather than sleeping a guess at it: a fixed sleep is a
# flake on a loaded machine and wasted seconds on an idle one.
waited=0
while ! grep -q "router connected" "$work/daemon.log" 2>/dev/null; do
    waited=$((waited + 1))
    [ "$waited" -gt 200 ] && fail "the daemon never reported a connected router"
    sleep 0.1
done
echo "router: session up"

run() { timeout 20 "$clove" "$@" 2>/dev/null; }

run add "$work/t.torrent" >/dev/null || fail "could not add a torrent"

# The announce is on the swarm's own clock, so wait for the bridge to see it.
waited=0
while ! grep -q "serving a tracker announce" "$work/sam.log" 2>/dev/null; do
    waited=$((waited + 1))
    [ "$waited" -gt 300 ] && fail "no announce reached the tracker"
    sleep 0.1
done
echo "router: announce delivered"

status=$(run status) || fail "status failed"
case $status in
*connected*) ;;
*) fail "status does not report a connected router: $status" ;;
esac

for want in "SESSION CREATE" "STREAM FORWARD" "NAMING LOOKUP" "STREAM CONNECT"; do
    grep -q "$want" "$work/sam.log" || fail "the bridge never saw a $want"
done
echo "router: session, forward, lookup and connect all exercised"

# Waited for, not grepped for: the router forwards an inbound connection on its
# own clock, so this is an event to wait on like the announce above. Asserting it
# had already happened is a race, and it lost.
waited=0
while ! grep -q "dialled the forward port" "$work/sam.log" 2>/dev/null; do
    waited=$((waited + 1))
    [ "$waited" -gt 300 ] && fail "the router's inbound forward was never accepted"
    sleep 0.1
done
echo "router: inbound forward accepted"

run remove "$info_hash" --data >/dev/null || fail "could not remove the torrent"

# Ask the daemon to leave, so the shutdown path is traced too. Bounded rather
# than a bare `wait`: under --trace this pid is strace's, and a tracer that
# declines to exit would hang the script rather than fail it.
kill -TERM "$daemon_pid" 2>/dev/null
waited=0
while kill -0 "$daemon_pid" 2>/dev/null; do
    waited=$((waited + 1))
    if [ "$waited" -gt 100 ]; then
        kill -KILL "$daemon_pid" 2>/dev/null
        break
    fi
    sleep 0.1
done
daemon_pid=""
# strace flushes its output file as the tracee dies; give it that moment.
[ "$trace" = yes ] && sleep 1

if [ "$trace" = yes ]; then
    echo "router: --- post-init syscalls ---"
    python3 - "$work/trace.txt" <<'PY' || exit 1
"""Split the trace at the seccomp() that installs the filter and report the half
after it — which is exactly the set the allowlist must permit."""
import re, sys
from collections import Counter

lines = open(sys.argv[1], errors="replace").read().splitlines()
call = re.compile(r"^(?:\[pid\s+\d+\]\s*|\d+\s+)?([a-z0-9_]+)\(")

cut = next((i for i, l in enumerate(lines) if "seccomp(SECCOMP_SET_MODE_FILTER" in l), None)
if cut is None:
    print("router: FAIL: the filter was never installed in this trace", file=sys.stderr)
    sys.exit(1)

seen, eperm = Counter(), Counter()
for line in lines[cut + 1:]:
    m = call.match(line)
    if not m:
        continue
    seen[m.group(1)] += 1
    if re.search(r"=\s*-1\s+EPERM", line):
        eperm[m.group(1)] += 1

for name, n in sorted(seen.items()):
    print(f"  {name:20} {n:7}")
print(f"  ({len(seen)} distinct, filter installed at trace line {cut})")

if eperm:
    print(
        "router: FAIL: the allowlist refused syscalls the daemon actually makes: "
        + ", ".join(f"{n} x{c}" for n, c in sorted(eperm.items())),
        file=sys.stderr,
    )
    print(
        "  Add them to ALLOWED in crates/cloved/src/sandbox.rs, with a reason, "
        "or stop making them.",
        file=sys.stderr,
    )
    sys.exit(1)
print("router: the allowlist covers every syscall the daemon made")
PY
fi

echo "router: ok"
