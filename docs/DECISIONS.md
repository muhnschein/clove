# Decisions

These memos in **documented, reversible defaults**: the
`i2pnet` trait boundary and per-module error/enum discipline keep every one
of them swappable, and any that fails in practice gets revisited *with
evidence* rather than re-litigated up front. Revisiting one is a normal PR,
not a scope change — but the burden of proof is on the change.

## Q1 — Tracker traffic session: shared with peer traffic

Tracker announces use the same stream subsession and destination as peer
traffic. This is what i2psnark does and what trackers expect (announced
identity must match peer identity), and one subsession is less supervision
state. Revisit only if a live swarm shows tracker streams starving
under peer load (QoS) — a separate subsession *on the same destination*
remains possible without engine changes.

## Q2 — Resume format: bencode

We hand-roll a bencode codec (torrent files require it, and §9 wants
hostile-input control over it). Reusing it for resume data means zero new
dependencies, no serde, and exactly one hostile-input parser to harden and
fuzz instead of two. Every resume file carries an integer `version` key from
day one; the format spec lives in `STATE-FORMAT.md`. Policy per SCOPE §3:
newer clove reads older state; older clove refuses newer state cleanly.

## Q3 — Fast extension (BEP 6) in v1

i2psnark supports it, it measurably improves swarm behavior (allowed-fast
pieces during choke, precise have-all/have-none), and it is cheap next to
BEP 10 which we need regardless. Wire-codec work, no architectural cost.

## Q4 — Identity: single client identity + global ephemeral flag

One persisted destination keypair per client (stable identity across
restarts), plus one global `ephemeral` config flag that skips persistence.
Per-torrent transient identities are v2+: they multiply session topology
(one PRIMARY session each) and supervision state for a niche benefit.

## Q5 — Concurrency: synchronous thread-per-peer

Blocking I/O, one thread per peer connection, dedicated worker threads for
disk and hashing, bounded channels between them. Most auditable option and
entirely viable at I2P scale (50–200 peers; tunnel latency dwarfs thread
cost). **De-risked externally:** yosemite 0.7.0 ships a first-class `sync`
cargo feature (alongside `tokio`/`smol`), so no async runtime enters the
dependency tree at all.

## Q6 — HTTP API server: hand-rolled HTTP/1.1

We control both ends (our CLI, local socket), need a tiny subset (GET/POST,
JSON bodies, token header, unix socket first), and the opentracker project
shows how a few hundred careful lines beat a framework's transitive closure.
Same reasoning covers the *client* side (tracker announces over I2P
streams): one shared minimal HTTP/1.1 implementation in `clove-core`.

## Q7 — Wire identity: peer-ID prefix `-CV0001-`, client string `clove/2026.08`

Azureus-style prefix `CV`, which does not collide with anything in the
informal BEP 20 registry (CT/CD/CB etc. are taken; CV is free as of this
writing). Version digits track releases.