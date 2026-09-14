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

| Router runs | How | What owns the namespace |
| --- | --- | --- |
| In a pod, beside clove | Podman Quadlet ([`quadlet/`](quadlet)), or a Kubernetes Pod | the pod |
| In a sibling container | `docker run --network=container:i2pd …`, or compose's `network_mode: "service:i2pd"` | the router's container |
| On the host | `docker run --network=host …` | the host |

That third column is the difference between the first two rows. Sharing a
namespace is not the same as depending on a container: in a pod the infra
container holds the namespace and clove and the router are peers in it, each
restartable on its own. `network_mode: "service:i2pd"` makes clove's network a
property of the i2pd *container*, so restarting the router takes clove's
namespace with it. Compose has no way to say the first thing, which is why the
Quadlet units below are the recommended arrangement and `compose.yaml` is the
Docker-shaped compromise.

## Quick start: a podman pod, under systemd

[`quadlet/`](quadlet) is a pod with i2pd and clove in it, as five systemd
units, and it wants **Podman 5.0 or newer** — `.pod` units and `Pod=` in a
`[Container]` arrived there. Rootless:

```console
$ mkdir -p ~/.config/containers/systemd ~/.config/containers/seccomp
$ cp contrib/container/quadlet/* ~/.config/containers/systemd/
$ cp contrib/container/seccomp/cloved.json ~/.config/containers/seccomp/
$ sed -i 's|/etc/containers/seccomp|'"$HOME"'/.config/containers/seccomp|' \
    ~/.config/containers/systemd/cloved.container
$ systemctl --user daemon-reload
$ systemctl --user start i2pd cloved
$ podman exec cloved clove status
```

Starting the two containers brings the pod up with them; there is no
`systemctl enable` step, because Quadlet units are generated and their
`[Install]` section is what starts them at boot. For a rootless pod that
should survive logout, `loginctl enable-linger "$USER"`.

CI runs podman's own Quadlet generator over these units where the runner has
one new enough, and skips below 5.0 — which is where GitHub's runners are
today. What that older generator does confirm is that every key here except
`Pod=` is one podman knows; the pod wiring itself is checked by the version
that has it, whenever the runner image catches up.

System-wide is the same with `/etc/containers/systemd`, no `sed`, and no
`--user`. Nothing in the pod wants a real uid on the host, so rootless is the
better default.

`router` reads `waiting-for-router` until i2pd has built its tunnels, which
takes a couple of minutes on a cold start, and `connected` after that. Neither
container depends on the other: there is no `Requires=`, and clove treats an
absent router as a state to wait in rather than a reason to fail.

## Quick start: docker compose

[`compose.yaml`](compose.yaml) is the same two containers with clove inside
i2pd's namespace, for hosts that have Docker rather than Podman — with the
lifecycle coupling described above.

```console
$ docker compose -f contrib/container/compose.yaml up -d
$ docker compose -f contrib/container/compose.yaml exec clove clove status
```

## Quick start, against a router on the host

```console
$ docker run -d --name clove \
    --network=host \
    --read-only --cap-drop=ALL --security-opt=no-new-privileges \
    --security-opt seccomp=contrib/container/seccomp/cloved.json \
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
- **Layer 3** — the systemd unit's confinement — has a counterpart here: a
  read-only root filesystem, no capabilities, no new privileges, and a syscall
  filter of clove's own (below), all set by `quadlet/cloved.container` and
  `compose.yaml`. One piece of it cannot be reproduced at all, though.
  `IPAddressDeny=any` locks the service to loopback, and the namespace clove
  shares here is the router's, which *must* reach the clearnet for the router
  to work. Layer 1 still holds — the belt is there, the braces are not.

If you want the clearnet lock, run clove under the systemd unit in
`contrib/systemd/system/` rather than in a container.

## The seccomp profile

[`seccomp/cloved.json`](seccomp/cloved.json) is a syscall filter for the
container, and it is not the same thing as the daemon's own. The daemon's
(Layer 2) is narrower and starts late: it covers one process from the moment
initialisation finishes. This one starts at the first instruction — before
clove has read its config, let alone restricted itself — and covers every
process in the container, the `clove` of a health check or a `podman exec`
included. It is the container's answer to `SystemCallFilter=` in the systemd
unit, down to answering `EPERM` the way that unit does.

It allows a little over a hundred syscalls. Docker's default profile allows
around three hundred and fifty.

It is measured rather than argued for.
[`ci/seccomp-profile.sh`](../../ci/seccomp-profile.sh) drives both binaries
through a full run against the fake SAM bridge — start-up, session, naming
lookup, announce, a peer, every CLI command — under `strace`, and takes
everything either of them called. To that it adds the daemon's own post-init
allowlist, read out of `crates/cloved/src/sandbox.rs` rather than restated, so
that teaching the daemon a new syscall widens this profile too instead of
leaving a filter that kills it on a path no fixture reaches. A short reserved
list covers what neither can give: the loader placing TLS, the sandbox
installing itself, the runtime's `execve`.

A dozen or so of them are not clove's at all. A container profile is
installed by the runtime, in the process that is about to *become* the
container — and that process lives a moment longer before it execs: runc and
crun check they have not been reparented, resolve paths without following a
symlink out of the container, write to the start fifo and close the
descriptors they are done with, all in Go, whose own runtime is still
scheduling underneath. Refuse `getppid`, `epoll_pwait` or `openat2` and the
container dies before clove exists, with the runtime reporting something
about a network namespace it could not bind-mount. None of them is a
capability, a credential, a mount or a namespace call.

That list came from the container job rather than from reasoning: when the
image fails to start, the job reruns the same profile with `SCMP_ACT_LOG` in
place of `SCMP_ACT_ERRNO` and reads the kernel's audit records back, so the
log names each refused syscall instead of leaving a dead container and a
message about namespaces.

```console
$ CLOVE_BIN_DIR=target/x86_64-unknown-linux-musl/release \
    ci/seccomp-profile.sh --write     # regenerate
$ ci/seccomp-profile.sh --check       # fail if the binaries outgrew it
```

Point it at the build you ship: the syscalls a binary makes are a property of
its libc, not of the source. CI runs `--check` against the musl binaries and
starts the image under this profile on every pull request, which are two
different questions — whether the list still covers what the code does, and
whether a container runtime agrees.

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
