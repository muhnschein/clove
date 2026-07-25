#!/bin/sh
# Turn on the SAM bridge for a router that does not ship it reachable.
#
# Usage: contrib/podman/enable-sam.sh <java|emissary>
#        (or: make router-sam-enable ROUTER=java)
#
# i2pd needs nothing here — its quadlet passes --sam.enabled=true on the
# command line and it is done. The other two keep their SAM settings in a
# config file the router writes on first boot, so there is nothing to edit
# until it has booted once. That is why this is a separate step and not a
# quadlet setting:
#
#   Java I2P  ships the SAM bridge as a client app with startOnLoad=false.
#             The image's entrypoint fixes SAM's bind address but leaves it
#             switched off, so a stock container answers on the console port
#             and refuses 7656.
#   emissary  writes router.toml with [sam] bound to loopback *inside* the
#             container, which PublishPort cannot forward to.
#
# Both edits are idempotent: run this again after a config reset, or if you
# are not sure whether it took.
set -eu

router="${1:-}"
case "$router" in
    java)     unit=i2p-java;  container=systemd-i2p-java ;;
    emissary) unit=emissary;  container=systemd-emissary ;;
    i2pd)
        echo "enable-sam: i2pd enables SAM from its quadlet's Exec line; nothing to do." >&2
        exit 0
        ;;
    *)
        echo "usage: $0 <java|emissary|i2pd>" >&2
        exit 2
        ;;
esac

if ! podman container exists "$container" 2>/dev/null; then
    echo "enable-sam: container $container is not there — start it first:" >&2
    echo "    make router-up ROUTER=$router" >&2
    exit 1
fi

case "$router" in
java)
    # The persisted copy lives in clients.config.d/, split out of the install
    # clients.config on first boot. Its exact filename embeds the class name
    # and varies by release, so find it rather than guess.
    cfg=$(podman exec "$container" sh -c \
        "find /i2p/.i2p/clients.config.d -type f -name '*SAMBridge*' 2>/dev/null | head -n 1") || cfg=""
    if [ -z "$cfg" ]; then
        echo "enable-sam: no SAMBridge entry in /i2p/.i2p/clients.config.d yet." >&2
        echo "  The router splits clients.config on its first boot; give it a minute" >&2
        echo "  and try again. 'podman logs $container' shows how far it has got." >&2
        exit 1
    fi
    echo "enable-sam: enabling the SAM bridge in $cfg"
    # startOnLoad may be absent as well as false; handle both.
    podman exec "$container" sh -c "
        if grep -q '^clientApp\.0\.startOnLoad=' '$cfg'; then
            sed -i 's/^clientApp\.0\.startOnLoad=.*/clientApp.0.startOnLoad=true/' '$cfg'
        else
            printf 'clientApp.0.startOnLoad=true\n' >> '$cfg'
        fi"
    ;;
emissary)
    cfg=/var/lib/emissary/router.toml
    if ! podman exec "$container" test -f "$cfg"; then
        echo "enable-sam: $cfg does not exist yet — emissary writes it on first boot." >&2
        echo "  Give it a minute; 'podman logs $container' shows how far it has got." >&2
        exit 1
    fi
    echo "enable-sam: binding [sam] to 0.0.0.0 in $cfg"
    # Set host inside the [sam] table only. awk rather than sed: the key name
    # is not unique across tables, and rebinding the wrong service would take
    # the router off the network in a way that looks like a clove bug.
    podman exec "$container" sh -c "
        awk '
            /^\[/ { section = \$0 }
            section == \"[sam]\" && /^[[:space:]]*host[[:space:]]*=/ { next }
            { print }
            section == \"[sam]\" && !done { print \"host = \\\"0.0.0.0\\\"\"; done = 1 }
        ' '$cfg' > '$cfg.new' && mv '$cfg.new' '$cfg'"
    ;;
esac

echo "enable-sam: restarting $unit"
systemctl --user restart "$unit"
echo "enable-sam: done. Check it answers:  make router-wait ROUTER=$router"
