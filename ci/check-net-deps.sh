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
ALLOW='yosemite'

# Known socket-capable / net-stack crates. Extend freely: a false positive
# costs one allowlist review; a false negative costs the whole point.
DENY='socket2 mio tokio tokio-util async-std smol async-io async-net polling
hyper hyper-util reqwest ureq curl curl-sys isahc attohttpc minreq
openssl openssl-sys native-tls rustls quinn h2 h3
trust-dns-resolver hickory-resolver hickory-proto dns-lookup
libp2p igd natpmp'

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

[ "$status" -eq 0 ] && echo "check-net-deps: ok"
exit "$status"
