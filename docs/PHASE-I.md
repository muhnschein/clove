# Phase I — Speaking Transmission

**Status:** Complete. The surface, the loopback listener it needs, the tests
and the manuals have all landed. What remains is out-of-CI: the GUI client
sign-off in §8.

`PHASE-H.md` made many torrents survivable and gave them a view worth
watching. What it did not do — and said so in §11 — is let anything *other
than `clove`* drive the daemon. This is that, and it is the last thing
between clove and a client somebody would use all day without being the
person who wrote it.

Nothing here relaxes `SCOPE.md` §9. Zero new crates. The two things it does
trade are named in §5 rather than left for a reader to discover.

---

## 1. Why this rather than a UI

`SCOPE.md` §2 defers a web UI indefinitely and §9's dependency budget is why.
`PHASE-F.md` §6 rejected a TUI framework on the same grounds, and
`DECISIONS.md` S2 later allowed a hand-rolled TUI precisely because it cost
nothing. Each time the answer has been the same: the interface is worth having,
the tree it would drag in is not.

Transmission's RPC is the way out of that trade. It is a protocol, not a
library. Implementing it costs a file; what it buys is every frontend that
already speaks it — tremc, transgui, Transdroid, Flood, Torrent Control, and
the \*arr download-client interface — none of which clove has to build, ship,
or keep working.

`SCOPE.md` §3 already anticipated it: *"Explicitly not compatible with the
Transmission/qBittorrent APIs in v1 (compat shim is a v2 candidate — worth it
for \*arr-style tooling, not worth the constraint now)."* `DECISIONS.md` **S3**
is the amendment that moves it, with the evidence and the reversal condition.

Three things clove already had are why this was cheap rather than a project:

- a hand-rolled HTTP/1.1 server (`clove-core::http`, Q6),
- a hostile-input-hardened JSON parser *and* encoder (`clove-core::json`),
- a registry whose operations map almost one-to-one onto the methods.

## 2. The rule that keeps §5 intact

Transmission's model is IP-shaped: `peers[].address` and `.port`, `peersFrom`,
`peer-port`, `port-test`, `blocklist-*`. `SCOPE.md` §5 Layer 1 forbids IP
vocabulary in every crate but `i2pnet`, enforced by `clippy.toml`'s
`disallowed_types` and by `ci/check-net-deps.sh`.

**The surface is a presentation layer and never constructs a type.** Everything
IP-shaped is a string or a constant. `peer-port` is `0` because I2P has no
ports; `dht-enabled`, `lpd-enabled`, `utp-enabled` and `blocklist-enabled` are
`false` because none of those exists here; `port-test` answers `false` because
there is nothing to open.

Both gates therefore pass **unchanged**, and that is the test of the design
rather than a happy accident: if landing this had required editing either one,
it would have meant IP vocabulary had entered the engine and the approach was
wrong.

## 3. The rule that keeps it honest

*No field is invented that a client would act on.*

Reporting a real `rateDownload` is compatibility. Fabricating a plausible
`peers[].port` would be a bug. Where clove has no answer there are exactly two
allowed moves — a **documented constant** that is true of every I2P torrent, or
**omission** — and never a plausible-looking number.

The load-bearing case is `peers`, the per-peer array. This is not a preference:
`SECURITY.md` puts *"leaking the client's destination, or a peer's, to somewhere
it does not belong — including logs, error messages, or the local API"* in scope
as a **vulnerability**, and `registry.rs` carries a test
(`a_peer_that_fails_is_not_named_in_the_recorded_error`) whose last assertion is
that a peer's address never reaches the API. Populating this array is that leak,
on request, by design. So the field is omitted: `peersConnected` is
real and is what list views render, and a GUI's peer tab shows nothing, which
reads as "no data" rather than as something being wrong.

*Trigger for revisiting:* an operator who wants their own node's peer table can
have one behind an explicit opt-in — it is their view of their own daemon —
but it is a separate decision with a separate argument, not a corner of this.

Also omitted: `pieces` (a large bitfield polled often, needed by nothing) and
`magnetLink`, `comment`, `creator` (clove does not retain them).

## 4. What it answers

`POST /transmission/rpc`, with or without a trailing slash (§7). One
`X-Transmission-Session-Id` per daemon run. Full method and field tables are in
`clove-api(7)`, which is the reference; this is the shape.

**Implemented:** `session-get`, `session-stats`, `free-space`, `torrent-get`,
`torrent-add`, `torrent-remove`, `torrent-start`, `torrent-start-now`,
`torrent-stop`, `torrent-verify`, `torrent-reannounce`, `torrent-set`,
`queue-move-top`, `port-test`.

**Refused, each with a message saying why** — never a silent success:

- **`session-set`.** clove applies its configuration at start and nothing
  rewrites it, so accepting a change and dropping it would be worse than
  refusing: a setting that silently does not stick is a bug report about a
  client. The message names `clove.conf(5)`.
- **`torrent-set-location`, `torrent-rename-path`.** One downloads directory,
  and Landlock confines the daemon to it after start.
- **`queue-move-up`/`-down`/`-bottom`.** Queue position *is* add order
  (`PHASE-H.md` §4: derived, not stored), so there is no position to write.
  `queue-move-top` is the exception and is exact — it means "run this one now",
  which is `clove start`.
- **`blocklist-update`** (peers are destinations), **`session-close`** (that is
  the service manager's job).

**`torrent-add` refuses a URL**, and this one is architectural rather than
convenience: fetching a torrent over HTTP is a clearnet request, which clove is
incapable of making (§5) and would not make if it could (§10). The message says
so, and points at `metainfo` or a magnet.

## 5. What this trades, stated rather than discovered

- **The daemon now parses JSON.** `PHASE-F.md` §2 states as a property that it
  never does — commands arrive as method + path + typed bodies. An RPC envelope
  is JSON, so that stops being true whenever this is enabled. What holds the
  risk down: the parser is `clove_core::json::parse`, already depth-capped and
  fuzzed; `MAX_REQUEST_BODY` still bounds the body; and the surface is off
  unless configured.
- **`version` reports Transmission's number.** Clients gate features on it, so
  it has to. It carries clove's own version alongside — `4.0.0 (clove 0.0.1)` —
  so that no human reading the field is deceived about what they are connected
  to, and `clove-api(7)` says the same in prose.

## 6. Authentication, and the port

Two schemes on one listener, chosen by path, **defaulting to `/v1/`**. That
default is the part that matters: an unrecognised path falls through to the
stricter scheme rather than to none, so a path added later cannot be served
unauthenticated by omission.

The Transmission scheme is HTTP Basic — password compared against the existing
API token, so there is no second secret and one thing to rotate — and then the
`409` CSRF handshake every client is built around. **In that order**, so an
unauthenticated caller cannot collect the session id, which is most of what it
is for.

**Off by default** (`transmission_rpc no`). Consistent with §3's *"Every key
exists to deviate from a sane default"*: the sane default is that only `clove`
talks to `cloved`, and a second authentication scheme should not exist on a
daemon nobody asked to expose one.

**`api_listen`, and why it has no override.** A Transmission client cannot
speak to a unix socket, so the surface needed a port.
`i2pnet::api::bind_loopback_tcp` had existed since Phase F, refusing any
non-loopback address before binding, and had never been wired to anything.
`api_listen` wires it — *beside* the unix socket, never instead of it, since
`clove` resolves the socket from the config and would have nothing to talk to.

There is deliberately no `i_know_the_api_is_remote` to match
`i_know_sam_is_remote`. A remote SAM bridge is a thing an operator might really
have; a wider API bind is not a case worth serving, because §5's answer to
reaching the daemon from elsewhere is a forwarded port — which puts the
authentication and the transport encryption in something that does both for a
living. The refusal names the `ssh -L` recipe rather than only saying no.

Both listeners are bound *before* the sandbox closes, which is the whole
discipline Layer 2 runs on: a bound listener keeps accepting through a Landlock
domain and a seccomp filter that would refuse to create it.

## 7. What running a real client found

Two defects, neither reachable from a unit test, both found within a minute of
pointing `transmission-remote` 4.0.5 at the daemon. This is the pattern the
whole of `PROTOCOL.i2p-bt` is written in, and the reason `ci/transmission.sh`
installs a real client rather than trusting curl.

- **The trailing slash.** `transmission-remote` posts to
  `/transmission/rpc/`, not `/transmission/rpc`. An exact-match route therefore
  handed a real client `/v1/`'s `missing or invalid API token` — a 401 about a
  header it has no reason to send, on a surface it could not tell it had failed
  to reach. `is_rpc_path` accepts either, with a regression test.
- **The empty tracker section.** With only `announce`, `tier` and the
  announce counters, `transmission-remote` printed *nothing at all* for
  trackers: it is the timing and scrape fields it decides a row exists from.
  They are there now and they are honest — clove keeps no announce timestamps
  and never scrapes, so those report "never" rather than implying a scrape that
  failed.

Both were invisible to curl, and to every one of the 21 unit tests that
existed at the time.

## 8. Testing

| Tier | What it proves |
|---|---|
| `crates/cloved/src/transmission.rs` unit tests | the mappings: status codes, sizes against skipped files, ETA, ratio sentinels, priorities in both directions, field selection, the two documented omissions |
| `crates/cloved/src/main.rs` `transmission_rpc` tests | the surface through the real `handle`: the auth matrix, the CSRF order, `/v1/` unaffected, add/list/act/remove, `recently-active` removals, and an adversarial envelope sweep |
| `ci/transmission.sh` (`make transmission`, in CI) | the real process: the listener binds, survives Landlock/seccomp, and answers a real HTTP client — plus `transmission-remote` end to end |

The adversarial sweep is the analogue of `tests/hostile.rs` for the one new
attack surface: the daemon parses a stranger's JSON now. `fuzz/README.md`
explains why the envelope has no fuzz target of its own and `base64` does.

**What is not done, and is the sign-off this phase waits on:** driving
**tremc**, **transgui** and **Transdroid** against a running daemon and
recording what each did, in the findings style §7 uses. `transmission-remote`
is one client and it found two defects; three GUIs will find more, and each is
a finding with a test rather than a footnote. Include what each does with the
404 on `/transmission/web/` — clove serves no web UI (`SCOPE.md` §2) and a
client that cannot survive its absence is a finding.

## 9. What this costs

**Nothing in dependencies.** `Cargo.lock` is unchanged. The one new parser —
standard-alphabet base64, needed for `torrent-add`'s `metainfo`, since clove
had only I2P's `-`/`~` alphabet — is about forty lines in `clove-core` with its
own fuzz target.

In lines it is the largest single file Phase H or I added, and `SCOPE.md` §9
makes that a review topic rather than a shrug. The argument for it: this is the
whole of clove's frontend story, replacing a web UI that §2 defers indefinitely
and that would cost several times as much to build and then to keep. It is one
file with one job, it holds no engine state, and it can be deleted in one
commit if it stops earning its place.

## 10. Not built, with triggers

- **qBittorrent's API.** The same argument would apply, and the same file could
  not serve it. *Trigger:* a frontend somebody actually wants that speaks only
  qBittorrent. Most speak both.
- **`/transmission/web/`.** Still `SCOPE.md` §2. Clients probing for it get a
  404 and should carry on; §8 checks that they do.
- **Rate limiting**, so `speed-limit-*` are reported as "no limit" rather than
  honoured. Unchanged from `PHASE-H.md` §11, same trigger.
- **Labels**, so `labels` is an empty array — the true answer, not a
  placeholder. Unchanged from `PHASE-H.md` §11.
- **Per-peer statistics.** `peersGettingFromUs`/`peersSendingToUs` are 0 and
  `peers` is absent (§3). *Trigger:* the same opt-in that would add a peer
  table, which needs its own argument about what the daemon volunteers.
- **`session-set` writing `clove.conf`.** A daemon that rewrites its own
  operator-owned configuration file is a different and much larger promise than
  this phase makes. *Trigger:* never, most likely; the config file belongs to
  whoever wrote it.
