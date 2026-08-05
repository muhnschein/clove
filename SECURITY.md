# Security Policy

clove exists to move data over I2P and nothing else. A bug that puts traffic,
or a user's identity, anywhere but I2P is the most serious class of defect this
project has. This file says how to report one.

## Reporting a vulnerability

**Use GitHub's private vulnerability reporting on this repository**. That is the only reporting channel. Do not open a public issue for a suspected vulnerability.

Please include the clove version or commit, the I2P router and version, what you observed, and a reproduction if you have one. A vague report of a real problem is still worth sending; we would rather
chase a hunch than miss a leak.

## What counts as a vulnerability

**Leak-class — the highest severity here.** Anything that causes clove to
communicate outside I2P, or that ties an I2P identity to a network identity:

- Any traffic to a non-loopback address other than the configured SAM bridge.
- Any DNS lookup, or any resolution path outside SAM naming.
- Contacting a non-I2P tracker or peer, however the address arrived.
- Leaking the client's destination, or a peer's, to somewhere it does not
  belong — including logs, error messages, or the local API.
- A way to defeat the `i2pnet` boundary from engine code.

**Also in scope:**

- Remote crashes, hangs, or unbounded memory growth triggered by a peer,
  tracker, `.torrent`, magnet, or resume file. Parsers here are hostile-input
  surfaces by assumption.
- Local API authentication bypass, or disclosure of the API token or the
  client's destination key to another local user.
- The destination key, or any part of the SAM `DESTINATION=` blob behind it,
  reaching a tracker, a peer, a log or the control API. Only the public
  destination on the front of that blob may ever leave the process
  (`docs/PROTOCOL.i2p-bt` §5.1c).
- State-file corruption that survives a restart, or resume data that causes
  clove to trust unverified pieces.
- Path traversal from torrent file names.

**Out of scope:**

- Weaknesses in I2P itself, or in the router. Report those to the router
  project; we will help you route it if you are unsure.
- Attacks that require an already-compromised machine or the daemon's own user
  account. The API token protects against other local users, not against
  someone who can read the data directory.
- Deanonymisation inherent to running a public BitTorrent swarm — that your
  destination is visible to peers you connect to is how the protocol works.
- Setting `i_know_sam_is_remote yes` and then observing that traffic to a
  remote SAM bridge is unprotected. That is documented, and the option is
  deliberately hard to type.

## Design guarantees a report can hold us to

These are enforced in the codebase and checked in CI, not merely intended:

- Only the `i2pnet` crate may open a socket. The engine crates cannot: it is
  enforced by `clippy` type bans and by `ci/check-net-deps.sh`, which fails the
  build if a socket-capable dependency appears without being allowlisted.
- The engine has no IP or port vocabulary at all. Peers are 32-byte I2P
  destination hashes end to end.
- The only outbound IP socket is to the SAM bridge, which must be loopback
  unless the documented override is set.
- Announce URLs that are not I2P URLs are dropped when the torrent is parsed —
  never contacted, never logged beyond a count.
- State files are written temp-then-rename, so a crash cannot corrupt them.
- After initialisation the daemon holds a `seccomp` **allowlist**: a syscall it
  does not need returns `EPERM`, whether or not anyone thought to name it. The
  permitted set is measured from a traced run rather than guessed, and the test
  suite performs that workload under the live filter.
- **One peer destination cannot take over a torrent.** It may hold at most a
  couple of concurrent connections, so it can neither fill the peer table nor
  become the only availability the piece picker can see.
- **Peers advertised over PEX cannot crowd out a tracker's.** The known-peer set
  is capped, and at the cap a PEX-learned entry is evicted to make room for a
  destination from a tracker, an inbound connection or the operator. PEX itself
  evicts nothing.
- **Foreign text is scrubbed where it becomes a terminal's input**, in the
  daemon's log lines and in `clove`'s output alike: control characters and
  bidirectional overrides out of a torrent, a tracker or a SAM bridge cannot
  forge a log line or rewrite what the rest of the screen says. The stored value
  and the API's JSON keep the real name.

If you find a way to violate one of these, that is a vulnerability by
definition, even without a demonstrated exploit.