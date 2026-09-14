#!/bin/sh
# The container seccomp profile for cloved: generate it, or check it still fits.
#
# Layer 2 is the daemon restricting *itself*, after it has finished starting up
# (crates/cloved/src/sandbox.rs). A container runtime can do something the
# daemon cannot do for itself: refuse a syscall from the first instruction,
# before the daemon has read its config or opened a file, and refuse it for
# every process in the container — including the `clove` the health check runs.
# That is this profile, and it is the container's answer to the systemd unit's
# `SystemCallFilter=` (contrib/systemd/system/clove.service, Layer 3).
#
# It is measured, not reasoned at: the binaries are driven through a full run
# against the fake SAM bridge — start-up, session, announce, a peer, every CLI
# command — under strace, and every syscall either of them makes goes on the
# list. To that it adds the daemon's own post-init allowlist — read out of
# sandbox.rs rather than restated, so the two layers cannot drift apart — and a
# short reserved list for what no trace can reach, each entry with a reason.
# This is the procedure `docs/SCOPE.md` §5 describes for the daemon's own
# allowlist, one layer out.
#
# Usage:
#   ci/seccomp-profile.sh --write   # regenerate contrib/container/seccomp/cloved.json
#   ci/seccomp-profile.sh --check   # fail if the binaries need something it lacks
#
# Point it at the build you intend to ship — the syscalls a binary makes are a
# property of its libc, not of the source (musl's `thread::sleep` is
# `nanosleep`, glibc's is `clock_nanosleep`):
#
#   CLOVE_BIN_DIR=target/x86_64-unknown-linux-musl/release ci/seccomp-profile.sh --check
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
bindir="${CLOVE_BIN_DIR:-$root/target/debug}"
cloved="$bindir/cloved"
clove="$bindir/clove"
profile="$root/contrib/container/seccomp/cloved.json"

mode=""
case "${1:-}" in
--write) mode=write ;;
--check) mode=check ;;
*)
    echo "usage: ci/seccomp-profile.sh --write|--check" >&2
    exit 2
    ;;
esac

[ -x "$cloved" ] && [ -x "$clove" ] || {
    echo "seccomp-profile: no cloved/clove in $bindir (cargo build --workspace)" >&2
    exit 1
}

for tool in python3 strace; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "seccomp-profile: SKIP ($tool is not installed)"
        exit 0
    }
done

# Two sources besides the trace.
#
# The first is the daemon's own post-init allowlist, read out of
# crates/cloved/src/sandbox.rs rather than restated here. That list is the set
# the daemon is *permitted* to make once it has restricted itself, and a
# container profile that refused one of them would kill the daemon on a path
# this fixture does not reach — the pwrite64 of a block that arrives, the
# fallocate of `preallocate yes`. Reading it here is also what keeps the two
# layers from drifting apart: teach the daemon a new syscall and this profile
# gains it on the next --write.
daemon_allowlist() {
    sed -n 's/.*libc::SYS_\([a-z0-9_]*\).*/\1/p' "$root/crates/cloved/src/sandbox.rs" \
        | sort -u
}

# The second is what neither the trace nor that list can give: the half of the
# process that runs *before* the daemon's filter exists, and what the runtime
# needs around it. Each of these is here because leaving it out breaks
# something, not because it might one day be handy.
#
# The `access` family is the clearest example of why this list is not
# guesswork. The published image is static musl and never asks: there is no
# loader to check /etc/ld.so.preload. A locally built glibc image asks before
# it reaches `main`, spelled `access` on x86-64 and `faccessat` where that
# syscall does not exist — so a profile measured only against what we ship
# would refuse an image somebody built from the same tree.
RESERVED='
arch_prctl
set_tid_address
prlimit64
access
faccessat
faccessat2
flock
chmod
fchmod
fchmodat
seccomp
landlock_create_ruleset
landlock_add_rule
landlock_restrict_self
execve
execveat
exit
exit_group
'

# And a third: syscalls that are not clove's at all.
#
# A container profile is installed by the runtime, in the process that is
# about to *become* the container — and that process goes on living for a
# moment first. runc and crun check they have not been reparented, write to
# the start fifo, close the file descriptors they no longer want, and only
# then execve. That code is Go, so its runtime is also still scheduling
# underneath: netpoll waits on epoll, the collector calls madvise, sysmon
# sleeps. Every one of those lands on this filter.
#
# Refusing them does not produce a clear error — the init dies before the
# daemon exists, and the runtime reports something about a network namespace
# it could not bind-mount, which is what the container job first saw. Nothing
# below is reachable by a compromised cloved that could not already do worse:
# no capability, credential, mount or namespace call is here.
RUNTIME='
getppid
epoll_create1
epoll_ctl
epoll_pwait
epoll_pwait2
close_range
dup3
pipe2
readlinkat
membarrier
rt_sigtimedwait
sched_getaffinity
getrandom
uname
'

work=$(mktemp -d)
sam_pid=""
daemon_pid=""
cleanup() {
    [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null
    [ -n "$sam_pid" ] && kill "$sam_pid" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

XDG_DATA_HOME="$work/data"
XDG_RUNTIME_DIR="$work/run"
XDG_CONFIG_HOME="$work/config"
export XDG_DATA_HOME XDG_RUNTIME_DIR XDG_CONFIG_HOME
mkdir -p "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/clove"

fail() {
    echo "seccomp-profile: FAIL: $*" >&2
    cat "$work/daemon.log" >&2 2>/dev/null
    exit 1
}

# The same fixture ci/router.sh uses: a free loopback port for the bridge and a
# one-piece torrent with an I2P tracker, so the announce path runs.
sam_port=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
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

python3 "$root/ci/fake-sam.py" "$sam_port" "$info_hash" "$work/announce.req" 2>"$work/sam.log" &
sam_pid=$!
sleep 0.5

# -D for the reason ci/router.sh gives: the daemon stays this shell's direct
# child, so the TERM below reaches it and the trace runs to its last syscall.
strace -D -f -qq -o "$work/trace-daemon.txt" "$cloved" >"$work/daemon.log" 2>&1 &
daemon_pid=$!

waited=0
while ! grep -q "router connected" "$work/daemon.log" 2>/dev/null; do
    waited=$((waited + 1))
    [ "$waited" -gt 200 ] && fail "the daemon never reported a connected router"
    sleep 0.1
done

# Every CLI command, because the profile covers every process in the container
# and `podman exec clove list` is one of them — as is the health check.
n=0
cli() {
    n=$((n + 1))
    timeout 20 strace -f -qq -o "$work/trace-cli-$n.txt" \
        "$clove" "$@" >/dev/null 2>&1 || true
}
cli status
cli add "$work/t.torrent"
cli list
cli list --json
cli show 1
cli pause 1
cli resume 1
cli verify 1
cli sequential 1 on
cli priorities 1 1
cli seed-ratio 1 2.0
cli completions bash
cli add "magnet:?xt=urn:btih:$info_hash"
cli remove --all
cli status

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
sleep 1

# The whole life of both binaries, not the half after the daemon's own filter:
# a container profile is in force from the first instruction.
observed=$(cat "$work"/trace-*.txt | python3 -c '
import re, sys
call = re.compile(r"^(?:\[pid\s+\d+\]\s*|\d+\s+)?([a-z0-9_]+)\(")
names = {m.group(1) for m in map(call.match, sys.stdin) if m}
# strace reports these as syscalls; they are its own bookkeeping, not calls.
names -= {"exited", "killed", "resumed", "detached", "unfinished"}
print("\n".join(sorted(names)))
')

allowed=$(printf '%s\n%s\n%s\n%s\n' \
    "$observed" "$(daemon_allowlist)" "$RESERVED" "$RUNTIME" \
    | grep -v '^$' | sort -u)

if [ "$mode" = write ]; then
    mkdir -p "$(dirname "$profile")"
    {
        cat <<'HEAD'
{
  "_comment": [
    "Container seccomp profile for cloved. Generated by ci/seccomp-profile.sh",
    "from a traced run of the binaries against ci/fake-sam.py, plus the",
    "RESERVED entries that script explains. Do not hand-edit: run",
    "`ci/seccomp-profile.sh --write` against the build you ship.",
    "Everything not listed returns EPERM, which is what the systemd unit's",
    "SystemCallErrorNumber=EPERM does one layer out. The daemon's own filter",
    "(Layer 2) answers ENOSYS and is narrower still; this one has to cover",
    "start-up, the CLI and the health check as well."
  ],
  "defaultAction": "SCMP_ACT_ERRNO",
  "defaultErrnoRet": 1,
  "architectures": [
    "SCMP_ARCH_X86_64",
    "SCMP_ARCH_AARCH64"
  ],
  "syscalls": [
    {
      "action": "SCMP_ACT_ALLOW",
      "names": [
HEAD
        printf '%s\n' "$allowed" | awk '
            { names[NR] = $0 }
            END {
                for (i = 1; i <= NR; i++)
                    printf "        \"%s\"%s\n", names[i], (i < NR ? "," : "")
            }'
        cat <<'TAIL'
      ]
    }
  ]
}
TAIL
    } > "$profile"
    echo "seccomp-profile: wrote $profile ($(printf '%s\n' "$allowed" | wc -l) syscalls)"
    exit 0
fi

# --check: the profile has to cover everything the binaries actually did. An
# entry it carries that this run did not need is not a failure — RESERVED is
# most of them, and the rest are paths a fixture does not reach.
printf '%s\n' "$observed" > "$work/observed.txt"
missing=$(
    python3 - "$profile" "$work/observed.txt" <<'PROF'
import json, sys
profile = json.load(open(sys.argv[1]))
allowed = set()
for rule in profile["syscalls"]:
    if rule["action"] == "SCMP_ACT_ALLOW":
        allowed.update(rule["names"])
observed = set(open(sys.argv[2]).read().split())
print("\n".join(sorted(observed - allowed)))
PROF
)

if [ -n "$missing" ]; then
    echo "seccomp-profile: FAIL: the profile would refuse calls the binaries make:" >&2
    printf '  %s\n' $missing >&2
    echo "  Regenerate it: CLOVE_BIN_DIR=$bindir ci/seccomp-profile.sh --write" >&2
    exit 1
fi
echo "seccomp-profile: the profile permits every call the binaries made"
