# Running clove in a loopback-only network namespace

This is the non-systemd form of Layer 3 (`docs/SCOPE.md` §5): put clove in a
network namespace that has *no route anywhere*, so the kernel makes clearnet
traffic impossible regardless of what the process tries. clove is already
I2P-only by construction — this is defence in depth, and clove runs correctly
with or without it.

The problem to solve: clove must reach the router's SAM bridge, and a namespace
with only `lo` cannot reach the host's `127.0.0.1`. Two ways to bridge that gap,
in order of preference.

## Option A — put the router in the namespace too (simplest, strongest)

Run i2pd *and* clove in one namespace. They talk over that namespace's own
loopback; the router's own transports reach the internet through the veth pair,
and clove has no route of its own to anywhere but `lo`.

```sh
# Create the namespace and a veth pair for the router's use.
sudo ip netns add clove
sudo ip link add veth-clove type veth peer name veth-host
sudo ip link set veth-clove netns clove
sudo ip addr add 10.77.0.1/30 dev veth-host
sudo ip link set veth-host up
sudo ip netns exec clove ip addr add 10.77.0.2/30 dev veth-clove
sudo ip netns exec clove ip link set veth-clove up
sudo ip netns exec clove ip link set lo up
sudo ip netns exec clove ip route add default via 10.77.0.1

# Forward for the namespace (adjust the uplink interface).
sudo sysctl -w net.ipv4.ip_forward=1
sudo iptables -t nat -A POSTROUTING -s 10.77.0.0/30 -o eth0 -j MASQUERADE

# Router first, then clove — both inside, talking over 127.0.0.1.
sudo ip netns exec clove sudo -u i2pd i2pd --sam.enabled=true &
sudo ip netns exec clove sudo -u "$USER" cloved
```

clove's own view of the world is a loopback interface plus a veth it has no
reason to use. Even a hypothetical leak has nowhere to go except through the
router.

## Option B — clove alone, SAM over a unix socket

If the router must stay on the host, give clove a namespace with **only `lo`**
and no veth at all, and pass the SAM bridge in as a unix socket. Nothing routes
out of that namespace, by construction.

```sh
sudo ip netns add clove-only
sudo ip netns exec clove-only ip link set lo up

# Relay the host's SAM port onto a unix socket the namespace can see through
# a bind mount. socat is the usual tool; run it on the host side.
socat UNIX-LISTEN:/run/clove/sam.sock,fork,mode=0600 TCP:127.0.0.1:7656 &

sudo ip netns exec clove-only sudo -u "$USER" cloved -c /etc/clove/clove.conf
```

with:

```
# /etc/clove/clove.conf
sam_address /run/clove/sam.sock
```

Note that clove accepts a unix-socket `sam_address` in its configuration, but
the bundled `yosemite` SAM backend currently connects to `127.0.0.1:<port>`
only (`docs/PROTOCOL.i2p-bt` §2.1). Until that is lifted, **Option A is the one
that works today**; Option B is documented because it is the stronger shape and
the configuration side is already in place.

## Verifying the lock

From inside the namespace, everything except loopback should fail:

```sh
# Expect: no route to host / network unreachable.
sudo ip netns exec clove-only curl -sS --max-time 5 https://example.com
# Expect: the router answers.
sudo ip netns exec clove ss -ltnp | grep 7656
```

A stronger check is to watch the host: with clove running inside the namespace,
`ss -tnp` on the host should show no clove socket to any non-loopback address,
ever. If it does, that is a leak-class bug — please report it
(`SECURITY.md`).

## Teardown

```sh
sudo ip netns del clove
sudo ip link del veth-host 2>/dev/null
sudo iptables -t nat -D POSTROUTING -s 10.77.0.0/30 -o eth0 -j MASQUERADE
```
