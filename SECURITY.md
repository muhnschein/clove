# Security Policy

clove exists to move data over I2P and nothing else. A bug that puts traffic,
or a user's identity, anywhere but I2P is the most serious class of defect this
project has. This file says how to report one and what to expect back. It is
published before the first release, deliberately, so it is never retrofitted
after the first report.

## Reporting a vulnerability

**Use GitHub's private vulnerability reporting on this repository** — the
*Security* tab, *Report a vulnerability*. That is the only reporting channel,
deliberately: one channel that is authenticated, private by construction, and
cannot be misdelivered beats a published address nobody watches and a PGP key
nobody can verify. Do not open a public issue for a suspected vulnerability.

If you cannot use GitHub at all, open a public issue containing **no details** —
just that you have something to report — and you will be given somewhere private
to send it.

Please include the clove version or commit, the router and version
(i2pd / Java I2P / emissary), what you observed, and a reproduction if you have
one. A vague report of a real problem is still worth sending; we would rather
chase a hunch than miss a leak.

## What to expect

- **Acknowledgement within 7 days.** GitHub notifies the maintainer directly, so
  silence past that means something went wrong — nudge the advisory thread.
- An assessment — whether we agree it is a vulnerability, and its severity —
  within 14 days.
- We aim to fix and release within 90 days of the report. Leak-class bugs
  (below) are treated as drop-everything work.
- Credit in the release notes unless you ask otherwise. We will not name you
  without your consent.
- Coordinated disclosure: we ask that you hold public details until a fix is
  released or 90 days have passed, whichever comes first. If a fix is taking
  longer than that, we will tell you why rather than go quiet.

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
- Local API authentication bypass, or token disclosure to another local user.
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

If you find a way to violate one of these, that is a vulnerability by
definition, even without a demonstrated exploit.

## Releases

Release tags are signed. The signing key's fingerprint is published with the
first signed release and does not change without an announcement; verify the tag
before building anything you intend to run.

Security fixes ship as a normal release with the issue described plainly in
the release notes. We do not quietly slip fixes into unrelated commits.
