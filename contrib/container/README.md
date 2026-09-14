# clove in a container

The published image is `ghcr.io/muhnschein/clove`, tagged with each release
(`:2026.8.0`) and `:latest`, for `linux/amd64` and `linux/arm64`. It is built
from [`Containerfile`](Containerfile) by
[`.github/workflows/release.yml`](../../.github/workflows/release.yml) and
contains two statically linked binaries on `distroless/static` — no shell, no
package manager, no libc. `cloved` is the entrypoint; `clove` is in the same
image so that `docker exec` can drive it.

## The router has to be on loopback

clove connects to a SAM bridge at `127.0.0.1:7656`, and that is not a default
you can point elsewhere: the SAM backend builds a loopback address by
construction (Layer 1, `docs/SCOPE.md` §5), and `clove.conf` rejects a
non-loopback `sam_address` before it ever gets that far.

So a router in a *separate* network namespace is not reachable. The container
has to share one with the router:

| Router runs | How |
| --- | --- |
| In a sibling container | `docker run --network=container:i2pd …`, or compose's `network_mode: "service:i2pd"` |
| On the host | `docker run --network=host …` |
| In Kubernetes | both containers in the same Pod |

## Quick start, with a router

[`compose.yaml`](compose.yaml) brings up i2pd with SAM enabled and clove
inside its network namespace:

```console
$ docker compose -f contrib/container/compose.yaml up -d
$ docker compose -f contrib/container/compose.yaml exec clove clove status
```

`router` reads `waiting-for-router` until i2pd has built its tunnels, which
takes a couple of minutes on a cold start, and `connected` after that.

## Quick start, against a router on the host

```console
$ docker run -d --name clove \
    --network=host \
    --read-only --cap-drop=ALL --security-opt=no-new-privileges \
    -v clove-data:/var/lib/clove \
    ghcr.io/muhnschein/clove:latest
$ docker exec clove clove status
```

## Driving it

The CLI is in the image, so every command in `clove(1)` works through
`docker exec`:

```console
$ docker exec clove clove add "magnet:?xt=urn:btih:…"
$ docker exec clove clove list
$ docker exec clove clove status
```

`clove add` reads a `.torrent` file itself rather than handing the daemon a
path, so the file has to be inside the container. The state volume is the one
writable place:

```console
$ docker cp release.torrent clove:/var/lib/clove/
$ docker exec clove clove add /var/lib/clove/release.torrent
```

## What lives where

Everything is under `/var/lib/clove`, which the image declares as a volume:
torrents, resume data, the destination key, the API token, the control socket
`clove.sock`, and the downloaded data itself.

The image sets `XDG_DATA_HOME=/var/lib` and `XDG_CONFIG_HOME=/etc` and ships
no configuration file, because an empty configuration is clove's working
default — those two variables are the whole of what makes the defaults land
in container-shaped places.

A **named volume** inherits the ownership the image gives `/var/lib/clove`
(uid 65532). A **bind mount** from the host does not, so `chown 65532:65532`
the directory first — otherwise the daemon exits with an error about a
directory it cannot make private.

To configure something, mount a file at `/etc/clove/clove.conf`; both
binaries read it. Unknown keys are fatal by design, so check one before
restarting into it:

```console
$ docker run --rm -v ./clove.conf:/etc/clove/clove.conf:ro \
    ghcr.io/muhnschein/clove:latest -C
```

## What the sandbox layers come to here

- **Layer 1** — no clearnet by construction — is in the binary and is
  unaffected by any of this.
- **Layer 2** — the daemon's own Landlock and `seccomp` restriction after
  start-up — survives a container runtime, but that is a measurement rather
  than a promise. Under Docker's *default* seccomp profile, with
  `--cap-drop=ALL` and `--security-opt=no-new-privileges`, the daemon reports:

  ```
  sandbox   landlock enforced; unix-socket connects unrestricted (kernel below ABI 9); seccomp filter installed
  ```

  Both mechanisms applied; the unrestricted part is Landlock ABI 9, which
  wants Linux 7.1 and is best-effort everywhere. CI starts the image and
  prints that line on every pull request, so the claim cannot rot silently —
  but a stricter custom profile or an older kernel can still take either
  mechanism away, and the daemon degrades to a log line rather than failing.
  Read the `sandbox` field of `clove status` (or the daemon's first lines) to
  see what applied on your host. `sandbox require` in `clove.conf` turns
  anything less than both into a refusal to start.
- **Layer 3** — the systemd unit's confinement — has no equivalent here, and
  one piece of it cannot be reproduced at all: `IPAddressDeny=any` locks the
  service to loopback, and a container sharing i2pd's network namespace
  shares a namespace that *must* reach the clearnet for the router to work.
  `--read-only`, `--cap-drop=ALL` and `--security-opt=no-new-privileges`
  (all set in `compose.yaml`) cover the filesystem and privilege half.

If you want the clearnet lock, run clove under the systemd unit in
`contrib/systemd/system/` rather than in a container.

## Verifying what you pulled

The image carries SLSA provenance, attached by BuildKit at build time — what
built it, from which commit, with which arguments:

```console
$ docker buildx imagetools inspect ghcr.io/muhnschein/clove:latest \
    --format '{{ json .Provenance }}'
```

The binaries in the release tarballs are copied out of this image, so
`sha256sum` on a tarball's `bin/cloved` matches the one in the image for that
architecture.

## Building it yourself

```console
$ make container                 # docker build, for this machine
$ docker buildx build -f contrib/container/Containerfile \
    --platform linux/amd64,linux/arm64 -t clove:dev .
```

The builder stage cross-compiles from whatever architecture you are on, so
the arm64 image does not need emulation.
