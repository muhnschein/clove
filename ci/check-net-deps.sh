#!/bin/sh
# Layer-1 no-clearnet CI gate (docs/SCOPE.md §5).
#
# Fails if any crate known to be socket-capable appears in Cargo.lock
# without being on the allowlist. This backs up the clippy.toml type ban:
# clippy catches our code touching sockets, this catches a dependency
# smuggling socket capability into the tree.
#
# Maintained by hand, reviewed with DEPENDENCIES.md. When a legitimately
# needed crate trips this, add it to ALLOW with a matching
# DEPENDENCIES.md entry in the same commit.
#
# `--self-test` checks the manifest scanner against manifests written for the
# occasion, and nothing else. The scanner is the half of this gate with a way
# to be wrong quietly — it reads TOML with awk — so it gets a test that fails
# loudly instead. See SCOPE §9: a check nobody can regress is a check nobody
# can trust.
set -eu

# Print every line belonging to a `rustix` dependency declaration in $1,
# whichever of TOML's three spellings it uses:
#
#   rustix = "1"
#   rustix = { version = "1", features = ["fs", "net"] }
#   rustix = { version = "1", features = [      <- and the same across lines
#       "fs",
#       "net",
#   ] }
#   [dependencies.rustix]                       <- or as its own table
#   features = ["fs", "net"]
#
# The multi-line inline table is not a hypothetical: crates/cloved/Cargo.toml
# has been written that way since rustix gained its third feature, and the
# grep this replaced only ever looked at the one line the crate name is on.
# It reported "ok" on a manifest with `"net"` two lines below it — which is
# the entire failure this gate exists to catch, passing quietly for as long
# as the manifest has had that shape.
rustix_declaration() {
    awk '
        # A table header closes whatever table we were in, and opens one we
        # care about if it names rustix ([dependencies.rustix], or the same
        # under a [target."cfg(...)"] prefix).
        /^[[:space:]]*\[/ {
            in_table = ($0 ~ /\.rustix\][[:space:]]*$/)
            next
        }
        # A `rustix = ...` key opens an inline declaration, which runs until
        # its braces balance — one line, or twenty.
        !in_inline && /^[[:space:]]*rustix[[:space:]]*=/ {
            in_inline = 1
            depth = 0
        }
        in_inline {
            print
            depth += gsub(/\{/, "{")
            depth -= gsub(/\}/, "}")
            if (depth <= 0) in_inline = 0
            next
        }
        in_table { print }
    ' "$1"
}

# Whether $1 turns on rustix's `net` feature.
enables_rustix_net() {
    rustix_declaration "$1" | grep -q '"net"'
}

self_test() {
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    status=0

    check() {
        # $1 name, $2 expected (yes/no), $3 manifest text
        printf '%s\n' "$3" > "$tmp/Cargo.toml"
        if enables_rustix_net "$tmp/Cargo.toml"; then got=yes; else got=no; fi
        if [ "$got" != "$2" ]; then
            echo "FAIL: self-test '$1': expected $2, got $got" >&2
            status=1
        fi
    }

    check "inline, net on" yes \
'[dependencies]
rustix = { version = "1", default-features = false, features = ["fs", "net"] }'

    check "inline, net off" no \
'[dependencies]
rustix = { version = "1", default-features = false, features = ["fs", "std"] }'

    # The shape the workspace actually uses.
    check "multi-line inline, net on" yes \
'[dependencies]
rustix = { version = "1", default-features = false, features = [
    "fs",
    "net",
    "std",
] }'

    check "multi-line inline, net off" no \
'[dependencies]
rustix = { version = "1", default-features = false, features = [
    "fs",
    "process",
    "std",
] }'

    check "own table, net on" yes \
'[target."cfg(unix)".dependencies.rustix]
version = "1"
features = ["fs", "net"]'

    check "own table, net off" no \
'[target."cfg(unix)".dependencies.rustix]
version = "1"
features = ["fs"]'

    # A *different* dependency asking for a feature called "net" is not
    # rustix asking for one, and must not fail the build.
    check "another crate has a net feature" no \
'[dependencies]
rustix = { version = "1", default-features = false, features = ["fs"] }
somecrate = { version = "1", features = ["net"] }'

    # Nor does a table that ended before the feature list began.
    check "table closed before the net feature" no \
'[dependencies.rustix]
version = "1"
features = ["fs"]

[dependencies.somecrate]
features = ["net"]'

    check "no rustix at all" no \
'[dependencies]
sha1 = "0.11"'

    [ "$status" -eq 0 ] && echo "check-net-deps: self-test ok"
    return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
    exit
fi

lock="${1:-Cargo.lock}"
[ -f "$lock" ] || { echo "check-net-deps: $lock not found (run cargo build first)" >&2; exit 1; }

# Exact crate names permitted to have socket capability.
ALLOW='rustix'

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

# Allowlisting a crate says "we reviewed its presence", which is not the same
# as "it cannot open a socket here". `rustix` is the case that makes the
# difference visible: it is in the tree for `openat`, and its networking is one
# word in a Cargo.toml away. So check the feature, not just the name — an
# allowlist entry would have sat quietly through exactly the change it exists
# to catch.
for manifest in Cargo.toml crates/*/Cargo.toml fuzz/Cargo.toml; do
    [ -f "$manifest" ] || continue
    if enables_rustix_net "$manifest"; then
        echo "FAIL: $manifest enables rustix's 'net' feature (see DEPENDENCIES.md)" >&2
        status=1
    fi
done

[ "$status" -eq 0 ] && echo "check-net-deps: ok"
exit "$status"
