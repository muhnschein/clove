#!/bin/sh
# Layer-1 no-clearnet CI gate (docs/SCOPE.md §5).
#
# Fails if any crate known to be socket-capable appears in Cargo.lock
# without being on the allowlist. This backs up the clippy.toml type ban:
# clippy catches our code touching sockets, this catches a dependency
# smuggling socket capability into the tree.
#
# Maintained by hand, reviewed with DEPENDENCIES.md. When a legitimately
# needed crate trips this (e.g. yosemite), add it to ALLOW with a matching
# DEPENDENCIES.md entry in the same commit.
set -eu

lock="${1:-Cargo.lock}"
[ -f "$lock" ] || { echo "check-net-deps: $lock not found (run cargo build first)" >&2; exit 1; }

# Exact crate names permitted to have socket capability.
ALLOW='yosemite rustix'

# Known socket-capable / net-stack crates. Extend freely: a false positive
# costs one allowlist review; a false negative costs the whole point.
DENY='socket2 mio tokio tokio-util async-std smol async-io async-net polling
hyper hyper-util reqwest ureq curl curl-sys isahc attohttpc minreq
openssl openssl-sys native-tls rustls quinn h2 h3
trust-dns-resolver hickory-resolver hickory-proto dns-lookup
libp2p igd natpmp rustix'

status=0
for crate in $DENY; do
    if grep -q "^name = \"$crate\"$" "$lock"; then
        allowed=no
        for ok in $ALLOW; do
            [ "$crate" = "$ok" ] && allowed=yes
        done
        if [ "$allowed" = no ]; then
            echo "FAIL: socket-capable crate '$crate' is in $lock but not allowlisted (see ci/check-net-deps.sh, DEPENDENCIES.md)" >&2
            status=1
        fi
    fi
done

# Every crate in the lockfile must be one somebody has looked at.
#
# The denylist above can only catch crates we already knew to name. This
# catches the rest: any crate name in Cargo.lock that is not in
# ci/deps-allowed.txt fails, so a new dependency cannot enter the tree —
# directly or three levels down — without a commit that says so. That is the
# check that would have had to be passed by a *renamed* or newly published
# networking crate, which is exactly the gap the denylist leaves.
allowlist="$(dirname "$0")/deps-allowed.txt"
if [ -f "$allowlist" ]; then
    known=$(sed 's/#.*//' "$allowlist" | tr -d '\r' | awk 'NF')
    for crate in $(sed -n 's/^name = "\(.*\)"$/\1/p' "$lock" | sort -u); do
        found=no
        for ok in $known; do
            [ "$crate" = "$ok" ] && { found=yes; break; }
        done
        if [ "$found" = no ]; then
            echo "FAIL: '$crate' is in $lock but not in ci/deps-allowed.txt; add it there with a DEPENDENCIES.md entry in the same commit" >&2
            status=1
        fi
    done
else
    echo "check-net-deps: $allowlist not found" >&2
    status=1
fi

# Allowlisting a crate says "we reviewed its presence", which is not the same
# as "it cannot open a socket here". `rustix` is the case that makes the
# difference visible: it is in the tree for `openat`, and its networking is one
# word in a Cargo.toml away. So check the feature, not just the name — an
# allowlist entry would have sat quietly through exactly the change it exists
# to catch.
for manifest in Cargo.toml crates/*/Cargo.toml; do
    [ -f "$manifest" ] || continue
    if grep -n '^rustix' "$manifest" | grep -q '"net"'; then
        echo "FAIL: $manifest enables rustix's 'net' feature (see DEPENDENCIES.md)" >&2
        status=1
    fi
done

[ "$status" -eq 0 ] && echo "check-net-deps: ok"
exit "$status"
