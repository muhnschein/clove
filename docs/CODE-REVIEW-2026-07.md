# Code review — 2026-07

A read of the whole tree (16.8 kLOC of Rust across `i2pnet`, `clove-core`,
`cloved`, `clove`) looking for bugs, followed by what the testing regime would
have to look like to have caught them.

Baseline: `cargo test --workspace` was green (221 tests) at the commit reviewed,
and every finding below is against that green tree. Findings 1, 2, 3, 4 and 5
were reproduced before anything was changed.

Severity is about the consequence for a user running the daemon, not about how
hard the bug was to find.

**Status.** Everything here is fixed on this branch except two items, each with
the test that catches it. Every one of those tests was checked both ways — it
fails on the tree as reviewed and passes after the fix — in debug and in release.

Left undone, deliberately:

- **Finding 21** (whole-torrent hashing under the registry mutex). Fixing it
  means verification moves off the request path, which needs progress
  reporting, cancellation and a new torrent state. That is a feature with a
  design to agree on, not a bug fix, and doing it badly would be worse than the
  blocking `verify` it replaces.
- **Half of finding 9** (a read timeout on established peer connections). The
  handshake read is bounded now and the thread count is capped, but a blanket
  read timeout on a peer stream would disconnect healthy peers: BitTorrent
  peers legitimately sit quiet for minutes, and tolerating that needs the
  keep-alive machinery R5 describes. A naive timeout here would trade a slow
  leak for dropped connections.

---

## Critical

### 1. A late duplicate block overwrites a verified piece — silent corruption, or a debug panic

`Shared::on_block` (`crates/clove-core/src/torrent.rs:705`) writes any block a
peer owed us straight to disk:

```rust
let was_requested = st.peers[idx].in_flight.remove(&(index, block_no));
if !was_requested {
    // ignore
} else if self.storage.write_block(index, begin, block).is_ok() {
```

Nothing checks whether we already hold `index`. The picker deliberately creates
exactly this situation: in endgame it hands one block to several peers
(`picker.rs:348`, "duplicate requests … are the accepted cure for the
last-block stall"). Endgame is on for the last 32 blocks of *every* torrent, and
from the start for anything ≤ 512 KiB. So:

1. Peers A and B are both asked for block *b* of piece *p*.
2. A answers; the piece completes, verifies, `set_have(p)`, `Have` broadcast.
3. B answers later. `was_requested` is still true for B, so B's bytes are
   written over the verified piece.

Consequences, both confirmed:

- **Release build:** the on-disk piece is now whatever B sent, while `have` still
  claims it and `is_complete()` is still true. For a multi-block piece
  `block_received` returns `false`, so nothing re-verifies and *the corruption is
  never noticed* — the client reports a finished download, serves the corrupt
  piece to the swarm, and only a manual `clove verify` finds it. For a one-block
  piece it does re-verify, fails, and calls `reset_piece`, which clears the block
  progress but **not** the have bit — so the outcome is the same, plus a piece
  the picker will never re-download.
- **Debug build:** `block_received` → `progress_mut` creates block progress for a
  piece in `have`, and `Picker::check_invariants` panics with `piece 0: held
  complete yet still has block progress`, killing that peer's reader thread.

The repro prints `after B: complete=true piece_verifies=false`.

Note this needs no malice — two honest seeders and a small torrent get there —
but a malicious peer can aim it: request-and-delay is enough to corrupt a
finished download that the victim will then advertise as complete.

**Fixed.** `on_block` now skips the write and the accounting when
`st.picker.has(index)`, and still refills the pipeline. Two guards behind it:
`Picker::progress_mut` refuses to create block progress for a piece in `have`
(so `block_received` returns `false` instead of resurrecting a finished piece —
the invariant is now structurally unreachable rather than merely asserted), and
`reset_piece` gives up the have bit, so bytes that failed SHA-1 can never leave
us announcing a piece the disk cannot back.

Tests: `evil_peer::a_late_duplicate_block_cannot_corrupt_a_verified_piece` drives
the whole path — two peers offered the same blocks (asserted, not assumed), the
honest one completes the torrent, the late one answers with rubbish, and the
bytes on disk must still be the file we asked for. Plus
`picker::a_late_endgame_block_does_not_reopen_a_held_piece` and
`picker::resetting_a_held_piece_gives_up_the_have_bit` at the unit level.

### 2. One peer that stops reading stalls every other peer on the torrent

`on_message` collects outgoing messages under the lock and then sends them with
the *blocking* `SyncSender::send` (`torrent.rs:531`):

```rust
for (tx, msg) in out {
    let _ = tx.send(msg);
}
```

`out` routinely holds messages for peers *other* than the one whose reader thread
is running: the `Have` broadcast on piece completion (`on_block`), every
choke/unchoke from `run_choker`, and PEX. Each peer's queue is
`sync_channel(OUTGOING_QUEUE = 256)`, drained by a writer thread that blocks in
`write_message`.

A peer that pipelines requests and never reads its socket therefore fills its
socket buffer, then its 256-deep queue — and from then on **any** other peer's
reader thread that produces a message for it blocks in `send`, forever. There is
no timeout anywhere on that path.

Reproduced: a leecher holding half a torrent, one hostile peer that sends
`Interested` plus a flood of `Request`s and never reads, then an honest seeder
attaches. The download stops one piece later —
`honest download stalled at 5/8 pieces` — because the seeder's reader thread is
parked broadcasting `Have` to the wedged peer.

This is precisely the property `tests/evil_peer.rs` claims to hold ("A
misbehaving peer cannot deny service to an honest one"); the existing slow-loris
case only covers a peer that *says nothing*, which never fills a queue.

**Fixed.** `on_message` now uses `try_send` and drops the peer whose queue will
not take the message, which also returns its in-flight blocks to the picker. The
peer's id travels with each queued message (`Outgoing`) so the sending thread
knows whose connection to close. No honest peer reaches a full queue: we enqueue
at most `PIPELINE_DEPTH` requests, one `have` per completed piece, and one block
per request the peer itself made — 256 deep means it has stopped reading.

Test: `evil_peer::a_peer_that_stops_reading_cannot_stall_an_honest_one` — a
half-seeded torrent serving a peer that floods requests and never reads, with an
honest seeder attached afterwards that must still deliver the rest. Note the
hostile peer has to hold its connection open past the assertion deadline:
dropping it closes the queue and releases the stall, which makes the test pass
for the wrong reason (it did, on the first attempt).

### 3. An empty token file authenticates every local client

`load_or_create_token` (`cloved/src/main.rs:664`) reads
`<data_dir>/token`, trims it, and uses it as the shared secret. An empty file
yields `""`, and `constant_time_eq(b"", b"")` is `true` — the daemon's own test
asserts that. A request carrying a bare `x-clove-token:` header then passes.

Verified against the built daemon with a zero-byte token file:

```
no header      : ('HTTP/1.1 401 Unauthorized', ...)
empty token    : ('HTTP/1.1 200 OK', '{"version":"0.0.1",...}')
bare colon     : ('HTTP/1.1 200 OK', ...)
wrong token    : ('HTTP/1.1 401 Unauthorized', ...)
```

Reachable because the token is the one file written *non*-atomically — no temp,
no rename, no fsync:

```rust
let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
file.write_all(token.as_bytes())?;
```

A crash, SIGKILL, or ENOSPC between `create_new` and `write_all` leaves a 0-byte
token behind, and every later start reads it as "the empty secret". `SECURITY.md`
names "Local API authentication bypass" as in scope, so this is the project's own
definition of a vulnerability.

**Fixed.** The token is written atomically now — a `0600` temp file, fsynced,
renamed over the target, so it is never half-written and never briefly readable
by anyone else. On load, a file that is not exactly 64 hex characters is replaced
rather than trusted, with one line to the log; nothing can be holding the old
value, because it was never a complete token. And the authentication path itself
checks the shape of its own expected token before comparing, so an empty secret
authenticates nobody even if it somehow got there another way.

Tests: `an_empty_token_authenticates_nobody` (over a real socket, through
`handle`, so parsing and auth are both in the path),
`a_malformed_token_file_is_replaced_not_trusted` across five ways the file can be
useless — checking the replacement is well formed, persisted, `0600`, and leaves
no temp behind — `a_well_formed_token_file_is_left_alone`, and
`token_shape_check`. The existing test fixture's token was 32 characters; it is
now the 64 a real one has, which is what made the shape check bite.

---

## Medium

### 4. Availability leak: repeated `Have`/`Bitfield`/`HaveAll`/`HaveNone`

`Shared::handle` counts availability unconditionally while the peer's own record
is idempotent or replaced wholesale:

- `Have(p)`: `peer.has.set(p)` (idempotent) then `picker.add_single(p)` (always
  `+= 1`). N `Have`s for one piece add N; the peer's departure subtracts 1,
  because `remove_bitfield` reads the bitfield.
- `Bitfield` / `HaveAll`: `add_bitfield(&new)` then `peer.has = new`, with no
  `remove_bitfield(&old)` — a second piece-set message double-counts the first.
- `HaveNone`: replaces `has` with an empty field and withdraws nothing.

Verified: 1000 `add_single(0)` then one peer leaving leaves `availability(0) ==
999`. Effect is a permanently distorted rarest-first order for the whole torrent
— a cheap way for one peer to steer everyone's piece selection — and in a debug
build `availability[i] += 1` is an overflow panic waiting on a long-lived
connection. The debug invariant net does not cover availability, which is why it
stays silent here.

**Fixed.** A `have` counts only when the bit actually changes, and every
piece-set message (`bitfield`, `have-all`, `have-none`) now goes through one
`replace_piece_set` that withdraws the old set before adding the new one.
`have-none` also re-checks interest, which it never did. The missing cross-check
is in `debug_check_state`: availability must equal, piece by piece, the number of
connected peers holding it.

Test: `evil_peer::re_announcing_a_piece_set_does_not_distort_availability` — every
way to re-announce, then availability asserted while the peer is attached and
again after it leaves. `Torrent::availability` is now public, which is what makes
that assertion possible; the debug invariant fires inside the engine's reader
thread, where no test can see it.

### 5. Duplicate file paths in a `.torrent` alias on disk, and the torrent can never complete

`metainfo::parse_files` validates each path *component* but never checks that the
resulting paths are distinct. `Storage::create` then opens the same file twice as
two regions at different global offsets, so their writes overlap.

Verified: a two-file torrent whose entries share the path `same.bin` (16384 + 100
bytes, two pieces) parses fine, both pieces are written correctly, and
`verify_all` reports `1/2` — piece 0 was clobbered by piece 1's write. A leecher
in that state re-downloads the piece forever against every peer it meets.

The related collision — `["a"]` and `["a", "b"]` — fails loudly instead
(`create_dir_all` over a regular file), which merely makes the add fail.

**Fixed.** `parse_files` sorts the paths and rejects both cases in one scan of
neighbours (if one path is a prefix of another, everything sorting between them
shares that prefix, so a collision always lands in an adjacent pair).

Tests: `metainfo::rejects_colliding_file_paths` covers duplicates, shadowing in
both orders, and the legal cases that merely share a prefix. The invariant is
also asserted in the hostile sweep and the `metainfo` fuzz target, so no future
parser change can reintroduce it quietly.

### 6. `fetch_metadata` has no bound on frames, pieces, or time

`torrent::fetch_metadata` requests every metadata piece up front and then loops
`while !asm.is_complete()`, reading frames and ignoring anything unexpected
(`torrent.rs:947`). A peer that re-sends piece 0 forever keeps the loop alive
indefinitely — `add_piece` is a no-op for a piece already held, so the loop never
converges and never errors. There is no read timeout on the stream either (see
finding 9).

Because `try_fetch_round` walks candidate peers sequentially, one such peer in a
tracker's reply pins a daemon thread and prevents that magnet from ever
resolving.

**Fixed.** The exchange carries a frame budget (a few per metadata piece plus
slack) and a two-minute deadline; exceeding either fails the peer rather than the
round. Re-requesting only the missing pieces would need a read timeout the SAM
stream cannot provide, so the bound is what closes this.

Test: `torrent::metadata_fetch_gives_up_on_a_peer_that_never_finishes` — a peer
that advertises a three-piece metadata and then answers forever with a piece of
the wrong length.

### 7. `known_peers` is unbounded, and our own PEX messages exceed our own PEX limit

Two halves of the same oversight:

- Inbound: `on_extended` inserts every `pex.added` entry into `known_peers` with
  no cap. The per-message limit is 512, but not the number of messages, so a peer
  can grow the set without bound — and the dial sweep will then try to dial every
  entry it holds, spending a tunnel and up to `dial_timeout` on each.
- Outbound: `send_pex` packs the *entire* `known_peers` set into one message. Any
  clove peer receiving it rejects the whole thing with `TooManyPeers` once the
  set exceeds `MAX_PEX_PEERS` (512). So PEX silently stops working exactly on the
  busy torrents where it matters — a decoder refusing what its own encoder
  produces, which is the discipline `docs/STATE-FORMAT.md` argues for elsewhere.

**Fixed.** `known_peers` is capped at `MAX_KNOWN_PEERS` (new destinations are
refused past it, which keeps the peers we learned first — including everyone we
are connected to), and `send_pex` takes at most `MAX_PEX_PEERS` destinations, so
a message we send always parses under the parser we enforce on others.

Test: `evil_peer::peer_exchange_stays_within_the_limit_it_enforces` seeds 4000
peers, then requires the PEX message we emit to parse under our own
`PexMessage::parse` and the known-peer set to stay bounded.

### 8. One bad forwarded connection kills the inbound accept loop

`SamListener::accept` (`i2pnet/src/sam.rs:367`) returns `Err` for *per-connection*
problems: the destination-line read timing out, non-UTF-8, an unparseable
destination, EOF before the newline. Both consumers treat any `Err` as "the
listener is gone and will not come back":

```rust
// swarm.rs accept_loop, and InboundDemux::run
Err(_) => return,
```

So a single malformed connection to the loopback forward port stops the daemon
accepting inbound peers *at all* until the SAM session is rebuilt (up to the 30 s
health interval plus reconnect backoff). `poke_listener` demonstrates how little
it takes: connect and close.

**Fixed.** `I2pListener::accept` now returns `io::Result<Option<..>>`: `Ok(None)`
is "that connection was not usable, keep accepting" and `Err` is "the listener is
finished". Both accept loops act on the difference, and the demux checks its stop
flag against whatever came back, so teardown's poke still breaks a blocked accept.

Test: `swarm::one_unusable_connection_does_not_end_the_accept_loop` puts a
listener that reports three duds in front of a real one and requires the download
to complete anyway.

### 9. No timeouts on peer streams; a thread per inbound connection, unbounded

`ForwardedStream::set_timeouts` exists, documents itself as "worth setting on
anything that serves peers", and is called by nothing but
`bin/sam-stress.rs`. `SamStream` (outbound, via yosemite) has no timeout API at
all. So in production every peer stream blocks forever on a silent peer:

- a peer that connects and never sends a handshake parks an `InboundDemux::route`
  thread for the life of the process — and `route` spawns one thread per accepted
  connection with no cap;
- a peer that handshakes and goes quiet parks a reader thread; `disconnect_all`
  documents this ("reader threads linger inertly … reclaiming them needs the
  keep-alive/read-timeout work (R5)").

Combined, idle connections are an unbounded thread leak, which is a cheaper
denial of service than it should be.

Related: `Torrent::threads` is push-only — two `JoinHandle`s per attach, never
joined or reaped — so peer churn grows it without bound even after the threads
exit.

**Partly fixed.** `I2pStream` grew a best-effort `set_timeouts`, and
`InboundDemux::route` bounds the wait for a peer's *first* bytes with it before
restoring blocking behaviour for the connection proper. Connections waiting on a
handshake are capped at `MAX_PENDING_HANDSHAKES`, released by a guard that
survives a panic, and `Torrent::attach` reaps the handles of peers that have
already finished.

The idle timeout on an established peer connection is *not* done — see the status
note at the top: it needs keep-alives (R5) to avoid dropping healthy peers.

Test: `swarm::a_flood_of_silent_connections_does_not_wedge_the_demux`.

### 10. The CLI ignores `clove.conf`

`clove::resolve` (`clove/src/main.rs:611`) builds its configuration from an empty
string:

```rust
let config = Config::parse("", &defaults)...;
let socket = socket.unwrap_or(config.api_socket);
let token_path = config.data_dir.join("token");
```

So `data_dir` and `api_socket` from the config file are never read.
`clove(1)` says the opposite: "The socket path and the API token are read from
the same configuration the daemon uses, so `clove` normally needs no arguments
beyond a command." With a non-default `data_dir` the CLI cannot find the token at
all, and there is no flag to point it at one (`--socket` covers only half the
problem). XDG environment variables still work, which is why `ci/smoke.sh` does
not notice.

**Fixed.** `clove` reads the same configuration file `cloved` does, by the same
rule (an explicit `-c` must exist; the default path may be absent), and grew
`-c`/`--config` to say which one. `--socket` still overrides the socket alone.

Tests: `clove::configuration_decides_where_the_daemon_is` for the resolution
rules, and a new `ci/smoke.sh` section that starts a daemon on a configured
`data_dir`/`api_socket` and drives the CLI at it — the coverage whose absence let
this survive, since everything else in the smoke test runs on the XDG defaults.

---

## Low

11. **`event=completed` is re-sent on every announce after completion.**
    `AnnounceState::next_event` returns `Completed` whenever `complete` is true,
    and nothing records that it was already reported — so a seeding torrent
    reports `completed` on every periodic announce, inflating tracker snatch
    counters. Latch it in `on_success`.

    → **Fixed**: `completed` is latched in `AnnounceState`, which now takes the event that was sent. Test: `tracker::completed_is_reported_once_and_only_once`.
12. **`https://` trackers are kept at parse time and then always fail.**
    `metainfo::is_i2p_tracker` accepts `http://` and `https://`;
    `tracker::split_url` strips only `http://`. An https announce URL therefore
    survives into `MetaInfo::trackers` and then fails `build_announce` with
    `BadUrl` on every attempt, forever, with nothing user-visible saying why.
    Also, `build_announce` does not re-check `.i2p`, so it is not a second line
    of defence for a URL that arrives from somewhere other than `metainfo`.

    → **Fixed**: `is_i2p_tracker` no longer accepts `https://` (clove has no TLS stack to speak it with, so such a URL is dropped and counted like any other tracker we cannot talk to), and `build_announce` refuses anything the filter would have dropped. Test: `tracker::build_announce_refuses_what_the_filter_drops`, which asserts the two agree in both directions.
13. **`left` is under-reported.** `announce_once` computes
    `have.count() * piece_length`, which over-counts a held short final piece and
    so under-reports `left` whenever the last piece is present and others are
    missing. Use the real byte count.

    → **Fixed**: `bytes_present` sums the real length of each held piece. Test: `swarm::bytes_present_counts_a_short_last_piece_as_short`.
14. **`serve_request` does not bound a request to its piece.** It checks
    `length > BLOCK_LEN` and `picker.has(req.index)` but not
    `begin + length <= piece_len(index)`, so a peer can read across into the next
    piece — bytes we have not verified, possibly not downloaded — labelled as
    part of a piece we hold. `read_block` only bounds against the whole torrent.

    → **Fixed**: a request must lie inside the piece it names, and a zero-length one is refused. Test: `evil_peer::a_request_may_not_reach_past_its_piece`.
15. **The choker never rotates.** `run_choker` is called only from the
    `Interested`/`NotInterested` arms; there is no periodic round. So
    `Choker::plan`'s round counter — and therefore the optimistic-unchoke slot
    the module is built around, and its tests exercise — effectively never
    advances in production.

    → **Fixed**: a round runs when one is due, driven off message traffic rather than a new timer thread (with no traffic there is nothing to reconsider). `Torrent::set_choke_interval` makes the cadence tunable (R5) and testable. Test: `evil_peer::every_interested_peer_eventually_gets_a_turn`, with six interested peers and four slots.
16. **`registry::load_one` does not cross-check the resume piece count.** It
    verifies `resume.info_hash == meta.info_hash` but not
    `resume.num_pieces == meta.pieces.len()`, so a stale resume file yields a
    `Hosted.have` of a different length than the torrent (wrong `progress`,
    wrong `state`) until the next refresh overwrites it.

    → **Fixed**: the piece count is checked alongside the info-hash, and a mismatch skips the entry with a message naming both numbers. Test: `registry::a_resume_file_for_a_different_torrent_is_skipped`.
17. **`try_fetch_round` calls `peers.dedup()` on an unsorted vector**, which
    removes only *consecutive* duplicates. Sort first or use a set.

    → **Fixed**: a `distinct` helper sorts before deduplicating. Test: `cloved::peer_lists_are_deduplicated_however_they_arrive`.
18. **`build_peer_id` fails silently.** If `getrandom` fails, the peer id is the
    literal `-CV0001-............` — identical across every instance that hits
    that path. Prefer failing loudly to shipping a shared identity.

    → **Fixed**: the peer id is generated once at startup and a failure is fatal, so no instance can ship the placeholder. Test: `cloved::the_peer_id_is_random_and_labelled`.
19. **`atomic_write` does not fsync the directory.** temp + fsync + rename makes
    the write atomic against a process crash, which is what `ci/chaos.sh` tests,
    but not durable against power loss — the rename can be lost, taking the file
    with it. The module doc's "a crash mid-write never corrupts them" is true
    only for the tested case.

    → **Fixed**: the containing directory is fsynced after the rename, best-effort.
20. **`i2p_base64_decode`'s doc contract is wrong.** It claims to return `None`
    "on a truncated final group"; it does not (`"A"` yields `Some(vec![])`).
    Callers happen to check for emptiness, so nothing is broken today, and the
    asymmetry with `base32_decode` — which *is* strict about spare bits, for
    good reasons spelled out in its doc — is worth closing.

    → **Fixed**: a dangling final symbol and non-zero spare bits are both refused, matching `base32_decode`'s rule and its documented reasoning. Test: `addr::i2p_base64_refuses_what_it_cannot_have_encoded`.
21. **Whole-torrent hashing runs under the single registry mutex.**
    `add_torrent` calls `verify_all`, and `POST /v1/torrents/<ih>/verify` does the
    same, both holding `daemon.registry`. On a large torrent with data present,
    every other API request and the persist loop block for the duration.

    → **Not fixed**, deliberately: see the status note at the top.
22. **`Storage::read_block` allocates before validating.** `vec![0u8; len as
    usize]` precedes the range check in `for_each_segment`. All current callers
    bound `len`, so this is defence-in-depth, not a live bug.

    → **Fixed**: the range is checked before the buffer is allocated. Test: `storage::an_absurd_read_length_is_refused_before_it_is_allocated`.
23. **`supervisor::Supervisor` is dead code.** The tested state machine — backoff,
    jitter, `Phase`, `report_lost` — is not what runs: `spawn_sam_supervisor`
    reimplements the loop by hand and never applies jitter at all, so the
    thundering-herd protection the module exists for is not in the binary. Use it
    or delete it (the culture-of-deletion rule in SCOPE §9 would say the latter,
    if the hand-rolled loop is the one that stays).

    → **Fixed** by deletion, which is where SCOPE §9 pointed: `Supervisor`, `Phase`, `Poll` and `SessionFactory` are gone, `ReconnectPolicy` (tested, and the part the daemon actually calls) stays, and the daemon's loop now applies the jitter it was missing. `jittered` also survives a NaN roll rather than panicking inside `Duration::mul_f64`. Tests: the policy tests in `supervisor`, plus `cloved::the_jitter_roll_is_in_range_and_moves`.
---

## Testing regime

The invariant net works. Finding 1 was caught by `Picker::check_invariants` the
instant a test drove the engine into that state — the assertion is exactly right,
it had just never been reached. That is the shape of the gap: the *assertions* are
strong and the *drivers* are narrow.

### A. No test ever puts two peers on one torrent

Every engine test is one connection: `torrent.rs`'s mock download, `swarm.rs`'s
runner tests, the registry tests. `evil_peer.rs` comes closest and still
deliberately separates them — the hostile peers hit a seeder one at a time, and
the honest download that follows uses a *fresh* leecher.

That single gap hides findings 1, 2 and 4, plus the entire choker, and it is the
highest-value fix in this document:

- A multi-peer fixture: one `Torrent` instance, N honest peers and M scripted
  hostile ones attached *concurrently*, driven to completion.
- Restate the evil-peer contract to mean what it says: the honest peer must
  finish **while sharing the torrent instance with** the misbehaving one, not
  after it has been disconnected.
- A specific endgame case: two peers asked for the same block, the second
  answering after the piece verified (finding 1), asserting the piece still
  verifies afterwards.

The two tests added with the critical fixes are the start of this: both attach
two peers to one torrent, and `partly_seeded_torrent` is the missing fixture —
a torrent that serves and downloads at the same time, which is the state a real
leecher is in and the one no test covered. What is still missing is the general
case: N peers, scripted, with the hostile ones interleaved rather than staged.

### A2. Three ways these tests passed for the wrong reason

Worth recording, because each one cost a rewrite and each is a trap this suite
invites:

- **A debug assertion inside an engine thread is invisible.** The availability
  invariant fires in the peer's reader thread; that thread dies, the peer is
  never deregistered, and the test carries on none the wiser. The first version
  of the availability test passed for exactly this reason. Assertions in engine
  threads need something observable from outside to go with them — which is why
  `Torrent::availability` is now public.
- **A test can win the race against the engine.** Attacking a torrent and then
  immediately reading its peer table can observe the state *before* the engine's
  own thread registers the connection, so every subsequent assertion is
  vacuous — and fast, which is the tell. Hold the connection open and wait for
  the state you expect before asserting on it.
- **A hostile peer that gives up at the deadline proves nothing.** The
  stalled-peer test held its connection for exactly as long as the assertion
  waited, so dropping it released the stall just in time and the test passed. A
  hostile fixture has to outlast the thing it is supposed to break.

A shared thread that fails the test when any engine thread panics would close
the first of these for good. `std::panic::set_hook` plus a flag the fixtures
assert on at teardown is the cheap version.

### B. Model-based testing for the pure state machines

`Picker` and `Choker` are pure, deterministic and cheap to drive. A seeded
random-operation test — `pick`, `block_received`, `block_failed`, `set_have`,
`reset_piece`, add/remove peer, in arbitrary order, with `check_invariants` after
every step and a printable seed on failure — would have found findings 1 and 4
without a socket in sight. The same harness shape as
`addr.rs`'s `mutating_a_real_address_never_panics_and_never_lies`, applied to
state instead of bytes.

Add the invariants that are currently missing rather than wrong:

- availability equals the column sums of the connected peers' bitfields
  (finding 4);
- no piece has block progress *or* accepts a block while it is in `have`
  (finding 1 — half of this exists, but only as a panic, not as a guard);
- the have-set never loses a piece and never gains one that failed verification.

Of the invariants named below, availability-versus-the-peer-table is now in
`debug_check_state`, and "no piece has progress while it is held" is enforced
structurally rather than asserted. The random-operation harness is still missing.

### C. Fuzzing stops at `clove-core`'s parsers

`clove-fuzz` depends on `clove-core` only, so `i2pnet::addr` is unfuzzed — and
`DestHash::from_b32` / `from_b64_destination` take bytes from magnets, PEX,
tracker replies and the router on every inbound stream. The xorshift sweep in
`addr.rs` is good, but it is a fixed 20 000 mutations of two seeds, not
coverage-guided.

Three additions, in value order:

1. **A stateful wire target.** Interpret the fuzz input as a *sequence* of peer
   messages and feed it through a real `Torrent` over the mock (handshake, then
   `Message::parse` → `on_message`), with the debug invariants live. That is the
   layer findings 1 and 4 live at, and no amount of parser fuzzing reaches it.
2. **`i2pnet::addr` targets** (b32 label, b64 destination, `read_dest_line`).
3. **A `Storage` geometry target**: derive (file lengths, piece length) from the
   input, then write and verify pieces, asserting that a correct write always
   verifies. Finding 5 is a one-line assertion in that target.

Also worth adding to the fuzz matrix in CI: `pex` and `metadata` are covered by
the `extensions` target, but neither the `MetadataAssembler` nor
`magnet::torrent_bytes` → `MetaInfo::parse` round-trip is.

Done so far: the `metainfo` target and the hostile sweep now assert that an
accepted torrent's file paths are distinct and non-shadowing, which is finding 5
turned into a property. The three targets above are still to write.

### D. The mock cannot express the two faults that matter here

`MockNet` models a dead session, a black hole, a manual read stall, and bounded
buffers. It cannot express:

- **a peer that stops draining** (finding 2). My repro fakes it with a small
  capacity and a non-reading peer; a first-class `set_write_stalled` /
  `set_never_drains` fault would make it a one-liner.
- **a read timeout**. Mock reads wait forever, which is exactly why finding 9
  (no timeouts anywhere in production) is invisible in CI. Give `MockStream` an
  optional read timeout and set it in the fixtures; the missing peer-idle timeout
  then shows up as a test failure rather than a design note.

Done so far: nothing. Both of these remain the reason a peer with no read
timeout (finding 9) and a peer that stops draining (finding 2) had to be
reproduced by hand, with a small `MockNet::with_capacity` and a peer that
declines to read, rather than expressed as a fault.

### E. Nothing tests the token file's failure modes

`the_token_file_is_created_once_and_kept_private` covers the happy path only.
Add: empty, whitespace-only, short, and trailing-garbage token files, each
asserting the API still refuses requests (finding 3). And a chaos case that
SIGKILLs the daemon during *first* start — before the token exists — then
restarts and asserts an unauthenticated request is still a 401. `ci/chaos.sh`
starts its kill storm only after the daemon has answered once, so the token
always exists by then.

Done: four cases over an empty, whitespace-only, truncated, non-hex and
over-long token file, plus an auth-path test that an empty expected token
authenticates nobody. The chaos case — SIGKILL during *first* start, before the
token exists — is still to add; `ci/chaos.sh` starts its kill storm only once the
daemon has answered, so the token always exists by then.

### F. Test the supervisor that actually runs

Either `cloved` uses `supervisor::Supervisor` or the module goes (finding 23).
Whichever way it lands, the reconnect path deserves the treatment
`hostile_bridge_tests` already gives session *setup*: a fake bridge that accepts,
serves a session, then dies mid-operation, asserting that the daemon detaches,
backs off with jitter, re-attaches, and that torrents come back live.

Done: the module went, and the daemon's loop now calls the tested policy —
including the jitter it never applied. The end-to-end reconnect test above is
still missing, and it is the one that would cover the loop's own sequencing
(connect → attach → health-probe → tear down → rebuild), which no unit test
reaches.

### G. Nothing starts the daemon with a real config file *(done)*

`Config::parse` was well covered as a function, but no test wrote a `clove.conf`
with a non-default `data_dir`/`api_socket`, started `cloved -c` on it, and drove
`clove` at it. That is why finding 10 survived: the smoke test uses XDG
environment variables, which both binaries honour. `ci/smoke.sh` now has that
section.

### H. Cheap properties that would each have caught a finding here *(done)*

- Every `PexMessage` we encode parses under our own `parse` (finding 7).
- Every URL kept by `is_i2p_tracker` is accepted by `build_announce`
  (finding 12).
- A parsed `MetaInfo` has pairwise-distinct, non-prefix-colliding file paths
  (finding 5).
- `Resume` → registry → `Resume` preserves the piece count and agrees with the
  `.torrent` (finding 16).

All four exist now, as unit tests rather than generated properties: turning them
into generated ones (over arbitrary torrents, arbitrary URL lists) is the version
that keeps finding things.

### I. Two mechanical gaps

- **CI never runs the tests in release mode.** The invariant net is
  `cfg(debug_assertions)`, so the release behaviour of finding 1 — silent
  corruption instead of a panic — is never exercised anywhere. Add
  `cargo test --workspace --release` to the `test` job; it is a minute, and it is
  the configuration users run. (Every fix on this branch was checked in release
  by hand, which is not the same as a gate.)
- **Assertions can rot into no-ops unnoticed.** Add a handful of negative tests
  that deliberately corrupt state and assert the invariant *fires*
  (`#[should_panic]` on a debug-only test), so a future refactor that silently
  weakens `check_invariants` fails the build.

### J. Metrics that would make "test volume > source volume" honest

SCOPE §9 tracks test LOC as the aspiration. Two better proxies, both cheap to
add on the schedule that already runs nightly fuzzing:

- `cargo llvm-cov` on the workspace, reported (not gated) per milestone. The
  interesting number is not the total but which *modules* are dark — `torrent.rs`
  and `registry.rs` carry the most logic and the fewest direct tests.
- `cargo-mutants` over `picker`, `choker`, `bitfield` and `metainfo`. They are
  pure, fast, and the modules whose bugs are silent; a surviving mutant there is
  a missing assertion, stated precisely.

### K. Concurrency scrutiny

The torrent mutex plus per-peer channels is a small protocol with a real
invariant ("never block a foreign reader thread") that finding 2 violates. A
ThreadSanitizer run (nightly, scheduled alongside fuzzing) over the multi-peer
fixture from item A is the cheapest ongoing check; a `loom` model of the
lock/channel handoff would be the thorough one, and is probably more than this
warrants today.

---

## Appendix A: repros

Every finding has a committed test now (named under each finding above), except
the two left undone. What follows is the original throwaway reproduction of the
first few, kept because each is the shortest statement of its bug.

Drop into `crates/clove-core/tests/` and run with `cargo test -p clove-core`.
Each fails on the tree as reviewed.

### Findings 1 and 4 — endgame overwrite, availability leak

```rust
// picker: a duplicate endgame delivery for a piece we already hold trips the
// debug invariant.
#[test]
fn picker_block_received_for_a_held_piece() {
    let mut p = Picker::new(1, BLOCK_LEN, u64::from(BLOCK_LEN), Mode::RarestFirst);
    let mut peer = Bitfield::empty(1);
    peer.set(0);
    assert_eq!(p.pick(&peer, 1).len(), 1);
    assert!(p.block_received(0, 0));
    p.set_have(0);
    // A second peer's copy of the same block lands after the piece completed.
    let _ = p.block_received(0, 0);   // panics: "held complete yet still has block progress"
}

// A peer spamming `Have` for one piece inflates availability permanently.
#[test]
fn have_spam_inflates_availability() {
    let mut p = Picker::new(2, BLOCK_LEN, 2 * u64::from(BLOCK_LEN), Mode::RarestFirst);
    for _ in 0..1000 {
        p.add_single(0);
    }
    let mut one = Bitfield::empty(2);
    one.set(0);
    p.remove_bitfield(&one);          // the peer leaves
    assert_eq!(p.availability(0), 0); // fails: 999
}
```

The engine-level version of finding 1: a one-piece torrent, two raw peers that
each announce the piece and unchoke, both receiving `Request { index: 0, begin: 0
}`; peer A answers honestly (the torrent completes and the piece verifies), then
peer B answers with `vec![0xEE; len]`. Result:
`after B: complete=true piece_verifies=false`, plus the invariant panic on the
reader thread.

### Finding 2 — a peer that stops reading stalls the download

Now `evil_peer::a_peer_that_stops_reading_cannot_stall_an_honest_one`. The
original: a leecher pre-loaded with the first half of an 8-piece torrent; a
hostile peer sends `Interested` then 2000 `Request`s for piece 0 and never reads;
two seconds later an honest seeder attaches. Assert the leecher reaches 8/8
within 20 s. Actual: `honest download stalled at 5/8 pieces while a peer held its
socket`.

### Finding 5 — duplicate file paths

Now covered by `metainfo::rejects_colliding_file_paths`; the original was a
storage-level demonstration of what the parser was letting through.

```rust
// Two `files` entries with the same path, 16384 + 100 bytes, two pieces.
let meta = MetaInfo::parse(&bytes).expect("rejected");   // accepted
let st = Storage::create(&meta, &dir, false).unwrap();
st.write_block(0, 0, &content[..BLOCK_LEN as usize]).unwrap();
st.write_block(1, 0, &content[BLOCK_LEN as usize..]).unwrap();
assert!(st.verify_all().unwrap().is_full());              // fails: 1/2 verified
```

### Finding 3 — empty token

Now `an_empty_token_authenticates_nobody` and
`a_malformed_token_file_is_replaced_not_trusted`. The original, against a
daemon built from the reviewed tree:

```sh
mkdir -p data/clove run && : > data/clove/token
XDG_DATA_HOME=$PWD/data XDG_RUNTIME_DIR=$PWD/run cloved &
printf 'GET /v1/status HTTP/1.1\r\nHost: clove\r\nx-clove-token: \r\n\r\n' \
  | nc -U run/clove.sock      # 200 OK
```
