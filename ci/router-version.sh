#!/bin/sh
# Print a router's version, best-effort. Silent and unsuccessful if it cannot.
#
# Usage: ci/router-version.sh <i2pd|java|emissary>
#
# LIVE-TESTING §6.3 wants a version against every recorded result — "i2pd 2.58"
# rather than "i2pd" — because a behaviour that changes between releases is the
# kind of thing that table exists to catch. Asking the operator to type it is
# how three routers came to be compared in one sitting with one version among
# them, so ask the router instead.
#
# There is no uniform way to do that. SAM will not say: `HELLO REPLY VERSION=`
# is the SAM protocol's version, not the router's. Each router publishes its
# own somewhere different, and Java I2P does not publish one a container can
# reach at all — so that falls back to naming the image it was built from,
# which is a weaker answer but a true one, and for a `:latest` tag it is
# arguably the more useful of the two.
#
# Best-effort throughout: an unreachable container, a missing podman, a
# changed flag all mean "no version", never a failure. Callers keep their own
# fallback (`--router-version`, or saying it was not recorded).
set -eu

router="${1:-}"
case "$router" in
    i2pd)     container=systemd-i2pd ;;
    java)     container=systemd-i2p-java ;;
    emissary) container=systemd-emissary ;;
    *)
        echo "usage: $0 <i2pd|java|emissary>" >&2
        exit 2
        ;;
esac

command -v podman >/dev/null 2>&1 || exit 1
podman container exists "$container" 2>/dev/null || exit 1

# The same probes ci/live-report.sh has used for its router-context block;
# shared rather than duplicated so the two reports cannot drift into
# disagreeing about what version was under test.
version=""
case "$router" in
    i2pd)
        version=$(podman exec "$container" i2pd --version 2>/dev/null | head -1 || true)
        ;;
    emissary)
        version=$(podman exec "$container" emissary-cli --version 2>/dev/null | head -1 || true)
        ;;
    java)
        version=$(podman inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' \
            "$container" 2>/dev/null || true)
        case "$version" in
            "" | "<no value>") version="" ;;
            *) version="Java I2P $version" ;;
        esac
        ;;
esac

# Nothing named itself: fall back to the image, which at least pins the build.
# A digest is not a version, so it is labelled as what it is rather than
# dressed up as one — §6.3's whole complaint is about results that claim more
# than they know.
if [ -z "$version" ]; then
    image=$(podman inspect -f '{{.ImageName}}' "$container" 2>/dev/null || true)
    digest=$(podman inspect -f '{{.Image}}' "$container" 2>/dev/null || true)
    [ -n "$image" ] || exit 1
    version="$router (image $image${digest:+ @ $(printf '%s' "$digest" | cut -c1-19)})"
fi

# One line, no stray whitespace: this goes into a report header and a table.
printf '%s\n' "$version" | tr -s ' ' | sed 's/^ *//; s/ *$//'
