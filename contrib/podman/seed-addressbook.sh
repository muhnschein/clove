#!/bin/sh
# Give a router an address-book entry it does not have yet, taken from a
# router that does.
#
# Usage: contrib/podman/seed-addressbook.sh [options] [host ...]
#        (or: make router-addressbook)
#
# WHY THIS EXISTS
#
# A magnet's `tr=` is almost always an address-book *name* — `tracker2.postman
# .i2p` — and names and b32 addresses resolve through entirely different
# machinery (PROTOCOL.i2p-bt §5.5). A b32 goes to the netDb, which any router
# with peers can do. A name goes to the local address book, which a router
# fills from an HTTP subscription *over I2P*, and which is therefore empty
# until the router has been up long enough to fetch one.
#
# emissary ships a default subscription and gets there on its own eventually.
# "Eventually" is the problem: a live run started minutes after boot fails
# every announce with
#
#     resolving tracker tracker2.postman.i2p:
#       protocol error: `router error: KeyNotFound`
#
# and, because a failed lookup is negative-cached with a doubling hold (R6),
# the symptom is not a stream of errors but a client that goes quiet. That is
# a router-setup problem wearing a clove-shaped costume, and it cost a whole
# emissary run in the 2026-07-28 matrix.
#
# So: ask a router that already knows the name, and write the answer into the
# one that does not. Nothing is hardcoded — a destination baked into this file
# would be a lie the day postman rotates keys, and there is no way to check it
# from here. The source router is the authority.
#
# WHAT IT WRITES
#
# emissary keeps its address book at <base>/addressbook/addresses, one
# `hostname=<52-char b32 label>` per line, read **once at startup** — which is
# why this restarts the router afterwards rather than leaving you to wonder
# why nothing changed.
#
# The label is RFC 4648 base32 (lowercase, unpadded) of SHA-256 over the
# destination's bytes, which is the same derivation `i2pnet::addr::to_b32`
# performs; the pipeline below is checked against it in
# `docs/LIVE-TESTING.md` §7b.
set -eu

usage() {
    cat <<'USAGE'
usage: contrib/podman/seed-addressbook.sh [options] [host ...]

  --from NAME   router to resolve the names with (default i2pd) — it must
                already know them
  --sam-port N  explicit SAM port for --from; overrides its default
  --to NAME     router to write the address book into (default emissary)
  --dry-run     resolve and print, change nothing
  --help        this

Hosts default to the ones the live tiers need. Both routers must be running;
the destination router is restarted so it reloads its address book.
USAGE
}

FROM=i2pd
FROM_PORT=""
TO=emissary
DRY_RUN=no
HOSTS=""

while [ $# -gt 0 ]; do
    case "$1" in
        --from) FROM="${2:?--from needs a value}"; shift ;;
        --sam-port) FROM_PORT="${2:?--sam-port needs a value}"; shift ;;
        --to)   TO="${2:?--to needs a value}"; shift ;;
        --dry-run) DRY_RUN=yes ;;
        --help|-h) usage; exit 0 ;;
        -*) echo "unknown option $1 (try --help)" >&2; exit 2 ;;
        *)  HOSTS="$HOSTS $1" ;;
    esac
    shift
done

# The one name every live tier needs, because it is the tracker in the magnets
# the swarm tier is run with. Kept short on purpose: this is a "get going"
# helper, not a substitute for the router's own subscription.
[ -n "$HOSTS" ] || HOSTS="tracker2.postman.i2p"

sam_port_of() {
    case "$1" in
        i2pd)     echo 7656 ;;
        java)     echo 7666 ;;
        emissary) echo 7676 ;;
        *) echo "unknown router '$1' (i2pd | java | emissary)" >&2; exit 2 ;;
    esac
}

container_of() {
    case "$1" in
        i2pd)     echo systemd-i2pd ;;
        java)     echo systemd-i2p-java ;;
        emissary) echo systemd-emissary ;;
        *) echo "unknown router '$1'" >&2; exit 2 ;;
    esac
}

for tool in nc openssl base32 base64 timeout; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "seed-addressbook: $tool is needed and not installed." >&2
        exit 1
    }
done

if [ "$DRY_RUN" = no ] && ! command -v podman >/dev/null 2>&1; then
    echo "seed-addressbook: podman is needed to write the address book." >&2
    echo "  Use --dry-run to resolve the names without touching a router." >&2
    exit 1
fi

if [ "$DRY_RUN" = no ] && [ "$TO" != emissary ]; then
    # i2pd and Java I2P both have their own address-book machinery and a
    # console to drive it; writing their files from outside would be guessing
    # at layouts this script cannot verify.
    echo "seed-addressbook: only emissary is supported as --to." >&2
    echo "  i2pd and Java I2P manage their address books through their consoles." >&2
    exit 2
fi

[ -n "$FROM_PORT" ] || FROM_PORT=$(sam_port_of "$FROM")
ADDRESSES=/var/lib/emissary/addressbook/addresses

if [ "$DRY_RUN" = no ]; then
    TO_CONTAINER=$(container_of "$TO")
    podman container exists "$TO_CONTAINER" 2>/dev/null || {
        echo "seed-addressbook: container $TO_CONTAINER is not there — start it first:" >&2
        echo "    make router-up ROUTER=$TO" >&2
        exit 1
    }
fi

# Ask $FROM's SAM bridge to resolve one name, and print the full base64
# destination it answers with.
#
# The two commands cannot be pipelined: a SAM bridge reads its handshake with
# one read and does not keep whatever followed it in the same packet, so a
# `NAMING LOOKUP` sent alongside `HELLO` is simply lost. Hence the sleep, and
# hence `timeout`, so a router that never answers costs seconds rather than
# the terminal.
sam_lookup() {
    {
        printf 'HELLO VERSION MIN=3.1 MAX=3.3\n'
        sleep 1
        printf 'NAMING LOOKUP NAME=%s\n' "$1"
        sleep 4
    } | timeout 20 nc 127.0.0.1 "$FROM_PORT" 2>/dev/null || true
}

# base32(SHA-256(destination)), lowercase and unpadded — the b32 label, and
# exactly what i2pnet::addr::to_b32 derives. The `tr` turns I2P's base64
# alphabet ('-' and '~') back into the standard one so `base64 -d` accepts it.
b32_label() {
    printf '%s' "$1" \
        | tr -- '-~' '+/' \
        | base64 -d 2>/dev/null \
        | openssl dgst -sha256 -binary \
        | base32 \
        | tr -d '=' \
        | tr 'A-Z' 'a-z'
}

echo "seed-addressbook: resolving with $FROM (SAM 127.0.0.1:$FROM_PORT)"
resolved=""
failed=""
for host in $HOSTS; do
    reply=$(sam_lookup "$host")
    # The *NAMING REPLY* line, not the whole conversation: the handshake that
    # precedes it says `HELLO REPLY RESULT=OK`, so a check against everything
    # received matches on the handshake and calls every lookup a success —
    # including `RESULT=KEY_NOT_FOUND`. Caught by testing this against a fake
    # bridge, and it would have written a hash of the string "KEY_NOT_FOUND"
    # into the address book as if it were postman.
    naming=$(printf '%s' "$reply" | tr -d '\r' | grep -m1 '^NAMING REPLY' || true)
    case "$naming" in
        *"RESULT=OK"*) ;;
        *)
            # The router's own word is the diagnosis: KEY_NOT_FOUND means
            # *this* router's address book does not have it either, which is a
            # different problem from the bridge being unreachable.
            echo "  $host: not resolved — ${naming:-no NAMING REPLY from 127.0.0.1:$FROM_PORT}" >&2
            failed="$failed $host"
            continue
            ;;
    esac
    dest=$(printf '%s' "$naming" | sed -n 's/.*VALUE=\([^ ]*\).*/\1/p' | head -1)

    # Check the *destination*, not the label. A destination is at least 387
    # bytes — 256 of public key, 128 of signing key, 3 of certificate header
    # (i2pnet::addr::destination_len) — and anything shorter is not one. The
    # label cannot be checked instead however tempting it looks: SHA-256 is
    # always 32 bytes, so it is always 52 base32 characters, and a hash of the
    # word "nonsense" is as well-formed as a hash of postman's destination.
    # Also caught by testing; an entry that resolves to the wrong place is
    # worse than one that is missing.
    dest_len=$(printf '%s' "$dest" | tr -- '-~' '+/' | base64 -d 2>/dev/null | wc -c | tr -d ' ')
    if [ "${dest_len:-0}" -lt 387 ]; then
        echo "  $host: $FROM answered with ${dest_len:-0} bytes, which is not a destination" >&2
        failed="$failed $host"
        continue
    fi
    label=$(b32_label "$dest")
    echo "  $host -> $label.b32.i2p"
    resolved="$resolved$host=$label
"
done

if [ -z "$resolved" ]; then
    echo "seed-addressbook: nothing resolved; the address book is unchanged." >&2
    echo "  Does $FROM know these names? Its own address book has to have been" >&2
    echo "  fetched first — give a freshly reseeded router time, or check its" >&2
    echo "  console." >&2
    exit 1
fi

if [ "$DRY_RUN" = yes ]; then
    echo "seed-addressbook: --dry-run, so $TO was not touched. Would have written:"
    printf '%s' "$resolved" | sed 's/^/  /'
    [ -z "$failed" ] || { echo "seed-addressbook: not resolved:$failed" >&2; exit 1; }
    exit 0
fi

# Merge rather than overwrite: whatever the router has fetched for itself is
# worth more than this handful, and clobbering it would trade one missing name
# for hundreds.
existing=$(podman exec "$TO_CONTAINER" sh -c "cat $ADDRESSES 2>/dev/null" || true)
kept=$(printf '%s' "$existing" | grep -v '^[[:space:]]*$' || true)
for host in $HOSTS; do
    kept=$(printf '%s\n' "$kept" | grep -v "^$host=" || true)
done

printf '%s\n%s' "$kept" "$resolved" \
    | grep -v '^[[:space:]]*$' \
    | podman exec -i "$TO_CONTAINER" sh -c \
        "mkdir -p $(dirname $ADDRESSES) && cat > $ADDRESSES"

count=$(podman exec "$TO_CONTAINER" sh -c "wc -l < $ADDRESSES" | tr -d ' ')
echo "seed-addressbook: $ADDRESSES now has $count entr$([ "$count" = 1 ] && echo y || echo ies)"

# emissary reads the file once, when it builds its AddressBookManager. Without
# this the entries are on disk and invisible, which is the most confusing
# possible outcome.
echo "seed-addressbook: restarting $TO so it reloads the address book"
systemctl --user restart "$TO"
echo "seed-addressbook: done. Check it answers:  make router-wait ROUTER=$TO"

[ -z "$failed" ] || {
    echo "seed-addressbook: not resolved:$failed" >&2
    exit 1
}
