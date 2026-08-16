//! The torrent coordinator: peer connections wired to the picker, choker,
//! and storage — the Q5 sync thread-per-peer model in practice.
//!
//! Each connection runs two threads over one `i2pnet` stream (hence
//! [`I2pStream::split`], which hands back independent read and write halves
//! of the same connection): a reader that blocks on incoming
//! messages and a writer that drains a bounded outgoing queue. Shared
//! torrent state (picker, choker, the peer table) lives behind one mutex;
//! handlers compute their outgoing messages while holding it, then release
//! it *before* sending so a slow writer can never stall the whole torrent.
//! Releasing the lock is only half of that: the send itself never blocks
//! either, because the messages one peer's reader thread produces are often
//! addressed to *other* peers, and waiting on a peer that has stopped reading
//! would park the thread serving somebody honest. A queue that will not take a
//! message means a dead connection, so the peer is dropped instead.
//!
//! Peer-connection lifecycle (the explicit state machine SCOPE §9 asks for):
//!
//! ```text
//!   Dialing/Accepting --handshake ok--> Handshaken
//!   Handshaken --exchange bitfields--> Active
//!   Active: choke/interest transitions drive request flow
//!     - peer Unchoke  => fill request pipeline (rarest-first / sequential)
//!     - Piece in      => write, verify on completion, broadcast Have, refill
//!     - peer Interested + choker grants a slot => Unchoke, serve Requests
//!   any state --read error / protocol violation--> Closed (peer removed,
//!     its availability withdrawn, its in-flight blocks released)
//! ```
//!
//! The four choke/interest booleans on a peer are the sub-state of `Active`
//! above, not a state machine smeared across flags: BEP 3 defines them as
//! four independent bits and all sixteen combinations are reachable, so
//! modelling them as anything else would obscure the protocol rather than
//! clarify it. See the waiver on `Peer` below.
//!
//! This module is generic over the `i2pnet` traits, so the same code runs
//! against the mock network in CI and a real router in production; peer
//! acquisition (dialling, accepting) belongs to [`crate::swarm`].

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use i2pnet::{DestHash, I2pClose, I2pStream};

use crate::bitfield::{self, Bitfield};
use crate::budget::{PeerBudget, PeerSlot};
use crate::choker::{Choker, PeerSnapshot};
use crate::extension::{self, I2P_PEX, UT_METADATA};
use crate::metadata::{self, METADATA_PIECE_LEN, MetadataMessage};
use crate::metainfo::MetaInfo;
use crate::pex::PexMessage;
use crate::picker::{Mode, Picker};
use crate::storage::Storage;
use crate::wire::{self, BLOCK_LEN, BlockRequest, Extensions, Handshake, Message};

/// Outstanding block requests a peer may have in flight before we wait for
/// data. Config-tunable later (R5).
pub const PIPELINE_DEPTH: usize = 16;

/// The extended-message ids clove advertises (peers send these ids back to
/// us). Fixed, since we control both ends by default; a peer tells us its
/// own ids in its handshake and we use those when sending to it.
const OUR_PEX_ID: u8 = 1;
const OUR_METADATA_ID: u8 = 2;

/// How often a choke round is reconsidered (BEP 3's customary ten seconds).
///
/// Rounds are periodic in the protocol, not event-driven: the optimistic slot
/// only rotates if `plan` is called again. Tunable per torrent (R5) via
/// [`Torrent::set_choke_interval`].
pub const DEFAULT_CHOKE_INTERVAL: Duration = Duration::from_secs(10);

/// How often a keep-alive goes out to a peer we have said nothing else to.
///
/// BEP 3's convention is every two minutes, and clients drop a connection that
/// has been silent for a few — so this is the interval that stops *other* peers
/// hanging up on us. It matters more here than on clearnet: an I2P connection
/// costs tunnel setup, so being dropped and redialling is expensive, and a
/// leecher waiting on a rare piece or a seeder nobody is requesting from can
/// legitimately have nothing else to send for a long time.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(100);

/// How long a block may stay requested before it is offered to somebody else.
///
/// BEP 3 has no acknowledgement and no cancellation the peer must honour, so a
/// request that is dropped — because the peer is overloaded, lost the piece,
/// or simply chose not to answer — is indistinguishable from one still in
/// flight. Without a deadline it is *permanent*: the block stays in the peer's
/// pipeline and in the picker's in-flight set, so it is never asked for again
/// and its piece can never complete.
///
/// Live, that stalled a download at 48% with every one of its 43 outstanding
/// pieces about three-quarters finished and none finishing, while per-peer
/// throughput bled away as pipeline slots filled with requests nobody owed
/// (`docs/PROTOCOL.i2p-bt` §4.7).
///
/// Generous, because a re-request is wasted bandwidth if the first answer was
/// merely slow: an I2P round trip is seconds, and a peer serving many others
/// is entitled to take its time. Short enough that a stuck piece resolves in
/// about a minute rather than never.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Consecutive maintenance rounds in which a peer may let requests expire
/// while delivering nothing before it is dropped.
///
/// Freeing the blocks is what unsticks the *download*; dropping the peer is
/// what unsticks the *slot*. With `max_peers` at 50 and a live swarm offering
/// well over a hundred destinations, a connection that has owed us blocks for
/// three rounds running and produced none is worth more as a free slot. It
/// stays in `known_peers` and the dial sweep may pick it up again after its
/// retry backoff, which is the right outcome if it was only having a bad
/// minute.
pub const REQUEST_STRIKES: u32 = 3;

/// How long a peer may say nothing at all before we drop it.
///
/// Three missed keep-alives at the interval above. Generous on purpose: tunnel
/// latency and a router under load are both normal, and disconnecting a healthy
/// peer costs more than waiting out a dead one.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a peer has to complete the BEP 3 handshake, in either direction.
///
/// The handshake is the first thing that happens on a connection and an I2P
/// round trip is slow, so this is generous — but it is *finite*, which the
/// dialled side's was not. `i2pnet`'s dial clears a stream's timeouts once the
/// router has answered, so [`Torrent::attach`] read the peer's 68 bytes on a
/// socket with none, and a peer that accepted the stream and then said nothing
/// blocked its caller for the life of the process. On the swarm's dial sweep
/// that was one silent peer stalling every subsequent dial for that torrent —
/// permanently, and invisibly, since the sweep simply never came back.
///
/// The bound is per read, not per handshake: a slow peer dribbling its 68
/// bytes is fine, a silent one is not.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often [`Torrent::spawn_maintenance`] wakes to do the periodic work.
///
/// Fine enough that the intervals above are honoured to within a tick, coarse
/// enough to be free.
pub const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5);

/// Largest number of peer destinations one torrent will remember.
///
/// `known_peers` grows from tracker replies and from peer exchange, and PEX is
/// peer-controlled: 512 destinations per message with no limit on messages. The
/// cap is what stops one peer filling memory and pointing the dial sweep at
/// thousands of destinations that will each cost a tunnel and a timeout.
///
/// A cap alone is not enough, because *which* entries it keeps decides whether a
/// flood can shut real peers out. At the cap, a destination from a tracker, an
/// inbound connection or the operator displaces one learned over PEX, and PEX
/// displaces nothing — so a flood can fill the free space but cannot take space
/// from a real peer.
pub const MAX_KNOWN_PEERS: usize = 1024;

/// Concurrent connections one I2P destination may hold on one torrent.
///
/// A destination is a peer's identity, and nothing about `BitTorrent` needs the
/// same peer twice. Nothing used to stop it either: the dial sweep skips
/// destinations it is already connected to, but the inbound path had no such
/// check, so a single destination could dial us over and over and take every
/// slot in the table — and with it, through the shared [`PeerBudget`], slots
/// belonging to every other torrent. That is not merely resource exhaustion: a
/// peer holding every slot is the only peer we talk to, its piece set is the
/// only availability rarest-first can see, and honest peers cannot get in at
/// all.
///
/// **Two, not one.** One is the honest ceiling and two is the forgiving one,
/// because a *legitimate* second connection is ordinary here:
///
/// - both sides dial at once — a routine `BitTorrent` race, and on I2P a slow one,
///   since a dial takes seconds to resolve a leaseSet; and
/// - a connection we still believe in that the peer has already given up on
///   (its side torn down without a FIN that reached us) blocks the peer's
///   reconnect until our idle timeout expires, five minutes later.
///
/// At one, both cases cost a real peer a connection. At two, an attacker gets
/// two slots out of `max_peers` instead of all of them, which is the whole point
/// — the cap does not have to be tight to stop a monopoly, only finite.
pub const MAX_CONNECTIONS_PER_DEST: usize = 2;

/// Outgoing message queue depth per peer before the writer applies
/// backpressure. Bounded — no unbounded channels in the engine (SCOPE §4).
///
/// Deep enough that no honest peer reaches it: we queue at most
/// [`PIPELINE_DEPTH`] requests, one `have` per completed piece, and one block
/// per request the peer made — and a peer's own pipeline is what bounds that
/// last one. A queue this full means the peer has stopped reading, which
/// [`Shared::on_message`] treats as a dead connection rather than something to
/// wait on.
const OUTGOING_QUEUE: usize = 256;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Cross-check the torrent's own bookkeeping against the picker's, in debug
/// builds only (SCOPE §9). The picker validates itself; what it cannot see is
/// whether the peer table agrees with it, and a mismatch there is exactly how
/// a download stalls: blocks counted as in flight that no peer will ever
/// deliver, because the peer that owed them went away without releasing them.
#[cfg(debug_assertions)]
fn debug_check_state(state: &State) {
    state.picker.check_invariants();

    // The picker must never believe more blocks are owed than peers actually
    // owe: a count held for a block no peer will deliver is never handed out
    // again, which is how a download stalls one block short.
    //
    // The reverse is legitimate and common. When a piece completes, set_have
    // drops its block accounting entirely, while peers keep their entries for
    // that piece until the outstanding responses arrive or they disconnect —
    // so peers can hold entries the picker has already forgotten.
    let peer_in_flight: u64 = state.peers.iter().map(|p| p.in_flight.len() as u64).sum();
    let picker_in_flight = state.picker.in_flight_total();
    assert!(
        picker_in_flight <= peer_in_flight,
        "picker believes {picker_in_flight} blocks are in flight but peers owe only \
         {peer_in_flight}: a request was leaked and will never be re-offered"
    );

    // Availability must be exactly what the peer table says: one count per
    // connected peer holding the piece. Anything else means a `have` was
    // counted twice, or a piece set was replaced without withdrawing the old
    // one — which quietly distorts rarest-first for the whole torrent, and is
    // invisible from the outside because nothing else reads these numbers.
    let num_pieces = state.picker.have_field().len();
    for index in 0..num_pieces {
        let holders = state.peers.iter().filter(|p| p.has.has(index)).count();
        assert_eq!(
            state.picker.availability(index) as usize,
            holders,
            "piece {index}: availability disagrees with the peer table"
        );
    }

    // Peer ids are handed out from a counter and must stay unique: two peers
    // sharing an id would make every lookup ambiguous.
    for (i, peer) in state.peers.iter().enumerate() {
        assert!(
            peer.id < state.next_id,
            "peer id {} was never issued",
            peer.id
        );
        assert!(
            !state.peers[i + 1..].iter().any(|other| other.id == peer.id),
            "duplicate peer id {}",
            peer.id
        );
    }
}

/// No-op outside debug builds: release stays lean.
#[cfg(not(debug_assertions))]
#[inline]
fn debug_check_state(_state: &State) {}

/// A running torrent: owns the shared state and the peer threads.
pub struct Torrent {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct Shared {
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    /// The client-wide ceiling on concurrent peer connections. Each attached
    /// peer holds one slot for exactly as long as it is in the table.
    budget: Arc<PeerBudget>,
    /// Lifetime bytes served to peers this run.
    uploaded: std::sync::atomic::AtomicU64,
    /// Lifetime payload bytes received this run (counted when a solicited
    /// block is written, before verification).
    downloaded: std::sync::atomic::AtomicU64,
    /// Peers this run that reached *us* — accepted through the router's
    /// `STREAM FORWARD` rather than dialed by us.
    ///
    /// The inbound path is the half of `PROTOCOL.i2p-bt` §2.5 that no
    /// router-free test can reach: it needs a remote router to resolve our
    /// leaseSet and open a stream to it. One non-zero reading against a
    /// public swarm settles that, and settles it more convincingly than the
    /// loopback test does, because the peer that dialed us is somebody else's
    /// router doing it for its own reasons.
    inbound: std::sync::atomic::AtomicU64,
    storage: Arc<Storage>,
    num_pieces: u32,
    max_frame: u32,
    /// Raw `info` dictionary bytes, for serving BEP 9 metadata to magnet
    /// peers. Empty if unknown (a synthetic test torrent).
    ///
    /// Shared with the registry's [`MetaInfo`](crate::metainfo::MetaInfo),
    /// not copied.
    raw_info: Arc<[u8]>,
    state: Mutex<State>,
    done: Mutex<bool>,
    done_cv: Condvar,
}

struct State {
    picker: Picker,
    choker: Choker,
    /// When the last choke round ran, so rounds stay periodic.
    last_choke_round: Instant,
    /// How long between choke rounds.
    choke_interval: Duration,
    /// How long we may leave a peer with nothing from us before a keep-alive.
    keepalive_interval: Duration,
    /// How long a peer may say nothing before we drop it.
    idle_timeout: Duration,
    /// How long a requested block may go unanswered (R5 tunable).
    request_timeout: Duration,
    peers: Vec<Peer>,
    next_id: u64,
    /// Peer destinations we know about (from connections, PEX, or the
    /// tracker), for peer exchange and future dialing, each with where we
    /// heard it — see [`Source`].
    known_peers: HashMap<DestHash, Source>,
    /// How many destinations we first heard of from a peer's `i2p_pex`
    /// message rather than from a tracker or an inbound connection.
    ///
    /// Counted because there is otherwise no way to observe PEX acquisition
    /// from outside the engine: `known_peers` grows from three sources at
    /// once, so watching it climb during a live run proves nothing about which
    /// one was responsible. This number is only ever bumped on the PEX path.
    pex_learned: u64,
    /// Announces attempted, and why the last one failed if it did.
    ///
    /// A torrent with no peers has exactly one interesting question — did the
    /// tracker answer? — and the answer used to live only in a discarded
    /// `Err(_)` inside the announce loop. A live run sat at "downloading, 0
    /// peers" for ten minutes with nothing anywhere saying why.
    announces_ok: u32,
    announces_failed: u32,
    last_announce_error: Option<String>,
}

/// Where a known destination came from, which is what decides whether it may
/// be evicted to make room for another.
///
/// The distinction exists because one of these sources is a stranger's word and
/// the rest are not. `known_peers` is capped, and the cap used to refuse *new*
/// entries once full rather than evicting — which sounds conservative and is the
/// opposite. PEX carries 512 destinations per message with no limit on messages,
/// so two messages from the first peer to connect filled the set with addresses
/// of its choosing, and from then until a restart no tracker reply could add a
/// single real peer to that torrent. The dial sweep would spend every wave on
/// the flood.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    /// A peer told us about it over `i2p_pex`. Unverified, unbounded in supply,
    /// and therefore the first thing to go.
    Pex,
    /// A tracker returned it, a peer dialled us from it, the operator asked for
    /// it, or we are connected to it. Not something a stranger can displace.
    Trusted,
}

impl State {
    /// Remember a destination we could dial, up to [`MAX_KNOWN_PEERS`].
    ///
    /// At the cap, a [`Source::Trusted`] destination displaces a
    /// [`Source::Pex`] one; PEX never displaces anything, so a flood can fill
    /// the free space but cannot take space from a real peer. When every entry
    /// is trusted there is nothing worth evicting and the new one is refused.
    ///
    /// A trusted sighting of a destination we first heard over PEX upgrades it,
    /// so a peer we actually reached stops being eviction fodder. Nothing
    /// downgrades an entry.
    ///
    /// Returns whether this destination was new to us.
    fn remember_peer_from(&mut self, dest: DestHash, source: Source) -> bool {
        if let Some(existing) = self.known_peers.get_mut(&dest) {
            if source == Source::Trusted {
                *existing = Source::Trusted;
            }
            return false;
        }
        if self.known_peers.len() >= MAX_KNOWN_PEERS {
            if source == Source::Pex {
                return false;
            }
            // Any PEX entry will do; which one is not worth deciding, and an
            // arbitrary victim is one less thing for a flood to game.
            let victim = self
                .known_peers
                .iter()
                .find(|(_, held)| **held == Source::Pex)
                .map(|(dest, _)| *dest);
            let Some(victim) = victim else {
                return false;
            };
            self.known_peers.remove(&victim);
        }
        self.known_peers.insert(dest, source);
        true
    }
}

// The four flags are the canonical BEP 3 per-connection choke/interest
// state; modelling them as anything other than four booleans would obscure
// the protocol, so the excessive-bools and field-name lints are waived here.
#[allow(clippy::struct_excessive_bools, clippy::struct_field_names)]
struct Peer {
    id: u64,
    /// This connection's slot in the client-wide [`PeerBudget`].
    ///
    /// Held rather than read: dropping it with the rest of the entry is what
    /// returns the slot, so the budget cannot drift from the peer table on any
    /// of the paths that remove a peer — idle timeout, protocol violation,
    /// pause, session teardown, or a reader thread that panicked.
    _slot: PeerSlot,
    /// The peer's I2P destination, for peer exchange.
    dest: DestHash,
    out: SyncSender<Message>,
    /// Ends the connection, from whichever thread drops the peer.
    ///
    /// Dropping `out` is not enough on its own: it wakes a writer *waiting* for
    /// the next message, but not one already blocked inside a write to a peer
    /// that has stopped reading — which is precisely the peer we most want
    /// gone. Without this the table entry went away while the connection, its
    /// two threads and its descriptor stayed, and the freed slot let the same
    /// peer come straight back and do it again.
    closer: Arc<dyn I2pClose + Send + Sync>,
    /// Their piece set.
    has: Bitfield,
    /// They are choking us.
    peer_choking: bool,
    /// We are choking them.
    we_choke: bool,
    /// They are interested in us.
    they_interested: bool,
    /// We are interested in them.
    we_interested: bool,
    /// Blocks we have requested from them, as (piece, block), each with the
    /// moment it was asked for so [`DEFAULT_REQUEST_TIMEOUT`] can expire it.
    in_flight: HashMap<(u32, u32), Instant>,
    /// Consecutive maintenance rounds in which this peer let requests expire
    /// without delivering a block. Reset by any block that arrives; at
    /// [`REQUEST_STRIKES`] the peer is dropped.
    strikes: u32,
    /// When we last queued anything for them, for keep-alive timing.
    last_sent: Instant,
    /// When we last heard anything from them, for the idle timeout.
    last_seen: Instant,
    /// Bytes served to them, the choker's ranking signal.
    uploaded: u64,
    /// The message id the peer listens on for `i2p_pex`, once it handshakes.
    pex_id: Option<u8>,
    /// The message id the peer listens on for `ut_metadata`.
    metadata_id: Option<u8>,
}

/// A running maintenance tick (see [`Torrent::spawn_maintenance`]).
///
/// Dropping it stops the thread, so an owner cannot forget to; there is nothing
/// to join, because the tick holds no state anyone waits on.
pub struct Maintenance {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A message queued to a specific peer, collected under the lock and sent
/// after it is released. The peer's id travels with it because the sender is
/// often *another* peer's reader thread — a broadcast `have`, a choke round —
/// and it needs to know whose connection to drop if the queue will not take it.
type Outgoing = (u64, SyncSender<Message>, Message);

impl Torrent {
    /// Start a torrent over `storage`, whose currently-verified pieces are
    /// `initial_have` (empty for a fresh leech, full for a seed). `mode`
    /// selects rarest-first or sequential picking.
    #[must_use]
    pub fn new(
        meta: &MetaInfo,
        storage: Arc<Storage>,
        initial_have: &Bitfield,
        mode: Mode,
        peer_id: [u8; 20],
    ) -> Arc<Torrent> {
        Torrent::with_budget(
            meta,
            storage,
            initial_have,
            mode,
            peer_id,
            PeerBudget::unlimited(),
        )
    }

    /// [`new`](Torrent::new), drawing its peer connections from a
    /// [`PeerBudget`] shared with the other torrents of the same client.
    ///
    /// What the daemon uses: the ceiling that matters is on concurrent streams
    /// against one SAM session, so it belongs to the client rather than to any
    /// torrent. A torrent built with [`new`](Torrent::new)
    /// gets an unlimited budget and behaves as it did before there was one.
    #[must_use]
    pub fn with_budget(
        meta: &MetaInfo,
        storage: Arc<Storage>,
        initial_have: &Bitfield,
        mode: Mode,
        peer_id: [u8; 20],
        budget: Arc<PeerBudget>,
    ) -> Arc<Torrent> {
        let num_pieces = meta.pieces.len().try_into().unwrap_or(u32::MAX);
        let mut picker = Picker::new(num_pieces, meta.piece_length, meta.total_length, mode);
        for index in initial_have.iter_present() {
            picker.set_have(index);
        }
        // Frame ceiling must cover the largest message: a bitfield, a piece
        // block, or a ut_metadata data message (16 KiB + bencode/extension
        // overhead). 256 bytes of slack covers the headers.
        let max_frame = u32::try_from(bitfield::byte_len(num_pieces))
            .unwrap_or(u32::MAX)
            .max(BLOCK_LEN)
            .max(u32::try_from(METADATA_PIECE_LEN).unwrap_or(u32::MAX))
            .saturating_add(256);
        let shared = Arc::new(Shared {
            info_hash: meta.info_hash.0,
            peer_id,
            budget,
            uploaded: std::sync::atomic::AtomicU64::new(0),
            downloaded: std::sync::atomic::AtomicU64::new(0),
            inbound: std::sync::atomic::AtomicU64::new(0),
            storage,
            num_pieces,
            max_frame,
            raw_info: Arc::clone(&meta.raw_info),
            state: Mutex::new(State {
                picker,
                choker: Choker::default(),
                last_choke_round: Instant::now(),
                choke_interval: DEFAULT_CHOKE_INTERVAL,
                keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                request_timeout: DEFAULT_REQUEST_TIMEOUT,
                peers: Vec::new(),
                next_id: 0,
                known_peers: HashMap::new(),
                pex_learned: 0,
                announces_ok: 0,
                announces_failed: 0,
                last_announce_error: None,
            }),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        });
        // A torrent that starts complete (a pure seed) is immediately done.
        if lock(&shared.state).picker.is_complete() {
            *lock(&shared.done) = true;
        }
        Arc::new(Torrent {
            shared,
            threads: Mutex::new(Vec::new()),
        })
    }

    /// Whether every piece is verified.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        lock(&self.shared.state).picker.is_complete()
    }

    /// Block until the download completes or `timeout` elapses; returns
    /// whether it completed.
    pub fn wait_complete(&self, timeout: Duration) -> bool {
        let done = lock(&self.shared.done);
        let (done, _) = self
            .shared
            .done_cv
            .wait_timeout_while(done, timeout, |d| !*d)
            .unwrap_or_else(PoisonError::into_inner);
        *done
    }

    /// Note peers we could dial (from the tracker, or seeded in tests). They
    /// join the set advertised over peer exchange.
    ///
    /// The set is capped at [`MAX_KNOWN_PEERS`]; past that, new destinations are
    /// dropped rather than remembered. Peers we are actually talking to are
    /// already in the set, so the cap costs candidates, never connections.
    pub fn add_peers(&self, peers: &[DestHash]) {
        let mut st = lock(&self.shared.state);
        for &p in peers {
            st.remember_peer_from(p, Source::Trusted);
        }
    }

    /// Set how long this torrent waits between choke rounds (R5 tunable).
    pub fn set_choke_interval(&self, interval: Duration) {
        lock(&self.shared.state).choke_interval = interval;
    }

    /// Set how long a peer may go without hearing anything from us before a
    /// keep-alive is sent (R5 tunable).
    pub fn set_keepalive_interval(&self, interval: Duration) {
        lock(&self.shared.state).keepalive_interval = interval;
    }

    /// Set how long a peer may say nothing before it is dropped (R5 tunable).
    pub fn set_idle_timeout(&self, timeout: Duration) {
        lock(&self.shared.state).idle_timeout = timeout;
    }

    /// Set how long a requested block may go unanswered before it is offered
    /// to another peer (R5 tunable). See [`DEFAULT_REQUEST_TIMEOUT`].
    pub fn set_request_timeout(&self, timeout: Duration) {
        lock(&self.shared.state).request_timeout = timeout;
    }

    /// Set what each piece is worth (`0` skip, `1` normal, `2` high), as
    /// derived from the user's per-file choice by
    /// [`MetaInfo::piece_priorities`](crate::metainfo::MetaInfo::piece_priorities).
    ///
    /// Takes effect on the next pick, and re-checks completion: dropping the
    /// only pieces a torrent was still missing finishes it, and a torrent that
    /// has quietly become complete needs to say so rather than wait for a block
    /// that is no longer coming.
    pub fn set_piece_priorities(&self, per_piece: &[u8]) {
        lock(&self.shared.state)
            .picker
            .set_piece_priorities(per_piece);
        self.shared.check_done();
    }

    /// Bytes of the pieces we want and do not hold — an announce's `left`.
    #[must_use]
    pub fn bytes_left(&self) -> u64 {
        lock(&self.shared.state).picker.bytes_left()
    }

    /// Flush this torrent's files to disk, and report the piece set that is
    /// durable once it returns.
    ///
    /// The pair is the point: a caller that fsyncs and then asks what was held
    /// can race a piece completing in between and record it as durable when it
    /// is not. Read after the sync, under no lock the writers need, so the
    /// answer is a set the disk has certainly caught up with.
    ///
    /// # Errors
    ///
    /// Any filesystem error flushing the torrent's files.
    pub fn sync_storage(&self) -> std::io::Result<Bitfield> {
        // Snapshot first, sync second, and return the *earlier* set: anything
        // that completed during the sync may or may not have been included, and
        // under-claiming costs a re-verify while over-claiming costs the thing
        // this whole mechanism exists to prevent.
        let before = self.have();
        self.shared.storage.sync_all()?;
        Ok(before)
    }

    /// Start the periodic work this torrent needs: keep-alives to peers we have
    /// nothing else to say to, dropping peers that have gone silent, and choke
    /// rounds.
    ///
    /// All three are things a torrent owes the swarm on a clock rather than in
    /// response to traffic — a connection with nothing moving on it is exactly
    /// when a keep-alive matters and exactly when a choke round should be
    /// reconsidering its slots. `period` is how often the thread wakes;
    /// [`DEFAULT_MAINTENANCE_INTERVAL`] in production, something short in tests.
    ///
    /// Dropping the returned handle stops the thread within one period, and the
    /// thread also exits on its own if the torrent is dropped first, so neither
    /// can outlive the other by more than a tick.
    #[must_use]
    pub fn spawn_maintenance(&self, period: Duration) -> Maintenance {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        // Weak, so a forgotten handle cannot keep the torrent (and its open
        // files) alive for the life of the process.
        let shared = Arc::downgrade(&self.shared);
        std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(period);
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                shared.maintain();
            }
        });
        Maintenance { stop }
    }

    /// The peer destinations this torrent currently knows about.
    #[must_use]
    pub fn known_peers(&self) -> Vec<DestHash> {
        lock(&self.shared.state)
            .known_peers
            .keys()
            .copied()
            .collect()
    }

    /// Record the outcome of one announce, for `clove show` to report.
    ///
    /// The announcer is the only thing that knows whether a torrent's lack of
    /// peers is a tracker problem, and it is a background thread whose errors
    /// nobody was reading.
    pub fn note_announce(&self, outcome: Result<(), String>) {
        let mut st = lock(&self.shared.state);
        match outcome {
            Ok(()) => {
                st.announces_ok = st.announces_ok.saturating_add(1);
                st.last_announce_error = None;
            }
            Err(e) => {
                st.announces_failed = st.announces_failed.saturating_add(1);
                st.last_announce_error = Some(e);
            }
        }
    }

    /// Announces that succeeded, announces that failed, and the last failure
    /// reason — the answer to "why does this torrent have no peers".
    #[must_use]
    pub fn announce_status(&self) -> (u32, u32, Option<String>) {
        let st = lock(&self.shared.state);
        (
            st.announces_ok,
            st.announces_failed,
            st.last_announce_error.clone(),
        )
    }

    /// How many peers this run reached us inbound, through the router's
    /// `STREAM FORWARD`, rather than being dialed by us.
    ///
    /// Cumulative for the run, not a count of live connections: a peer that
    /// connected and left still proves the inbound path carried a stream,
    /// which is the fact worth keeping (`PROTOCOL.i2p-bt` §2.5).
    #[must_use]
    pub fn inbound_peers(&self) -> u64 {
        self.shared
            .inbound
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many peer destinations we first learned from an `i2p_pex` message.
    ///
    /// "Peers learned via `i2p_pex` beyond the tracker's set" was previously
    /// only checkable by reading a packet capture or trusting a hunch. A
    /// non-zero value here is that claim, made checkable.
    #[must_use]
    pub fn pex_learned(&self) -> u64 {
        lock(&self.shared.state).pex_learned
    }

    /// The client-wide connection budget this torrent draws on.
    ///
    /// For callers deciding whether to *start* work — a dial sweep sizing its
    /// wave, an acceptor about to answer a handshake. What they read from it
    /// is advisory; the claim inside `attach` is what actually enforces the
    /// ceiling.
    #[must_use]
    pub fn budget(&self) -> &Arc<PeerBudget> {
        &self.shared.budget
    }

    /// The peers currently attached (handshaken, threads running). The swarm
    /// runner uses this to skip live peers when sweeping `known_peers` for
    /// dial candidates.
    #[must_use]
    pub fn connected_peers(&self) -> Vec<DestHash> {
        lock(&self.shared.state)
            .peers
            .iter()
            .map(|p| p.dest)
            .collect()
    }

    /// A snapshot of our verified piece set, for progress display and resume
    /// persistence.
    #[must_use]
    pub fn have(&self) -> Bitfield {
        lock(&self.shared.state).picker.have_field().clone()
    }

    /// How many connected peers hold piece `index`.
    ///
    /// The number rarest-first steers by, exposed because nothing outside the
    /// engine could otherwise see it — which is how a peer inflating it went
    /// unnoticed. Also the honest answer to "why is this piece not moving".
    #[must_use]
    pub fn availability(&self, index: u32) -> u32 {
        lock(&self.shared.state).picker.availability(index)
    }

    /// Switch piece-selection mode on a running torrent (SCOPE §3's
    /// per-torrent sequential flag). Takes effect on the next pick; nothing
    /// in flight is cancelled.
    pub fn set_mode(&self, mode: Mode) {
        lock(&self.shared.state).picker.set_mode(mode);
    }

    /// Disconnect every attached peer: each is removed from the peer table
    /// (withdrawing its availability and releasing its in-flight blocks) and
    /// its outgoing queue is dropped, so its writer thread exits, closes the
    /// connection, and the reader blocked on it returns. Both threads and the
    /// descriptor are reclaimed; nothing is left parked.
    pub fn disconnect_all(&self) {
        let ids: Vec<u64> = lock(&self.shared.state)
            .peers
            .iter()
            .map(|p| p.id)
            .collect();
        for id in ids {
            self.shared.remove_peer(id);
        }
    }

    /// How many of this torrent's peer threads are still running.
    ///
    /// Two per attached connection, a reader and a writer. Exposed because
    /// "the peer table is empty" and "the threads that served it are gone" are
    /// different claims, and only the first one was ever checkable — which is
    /// how a reader parked forever on a dropped peer went unnoticed through
    /// two rounds of fixing it.
    #[must_use]
    pub fn live_threads(&self) -> usize {
        lock(&self.threads)
            .iter()
            .filter(|handle| !handle.is_finished())
            .count()
    }

    /// This torrent's info-hash — its identity on trackers, the wire, and in
    /// the inbound demux.
    #[must_use]
    pub fn info_hash(&self) -> [u8; 20] {
        self.shared.info_hash
    }

    /// Our peer id on the wire and in announces.
    #[must_use]
    pub fn peer_id(&self) -> [u8; 20] {
        self.shared.peer_id
    }

    /// Bytes (uploaded, downloaded) since this engine instance started —
    /// what announces report; lifetime totals are the registry's resume data
    /// plus these deltas.
    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.shared.uploaded.load(Ordering::Relaxed),
            self.shared.downloaded.load(Ordering::Relaxed),
        )
    }

    /// Our side of the BEP 3 handshake.
    fn our_handshake(&self) -> Handshake {
        Handshake {
            info_hash: self.shared.info_hash,
            peer_id: self.shared.peer_id,
            // Advertise the BEP 10 extension protocol (i2p_pex, ut_metadata).
            // Fast (BEP 6) stays off until its semantics are wired.
            extensions: Extensions {
                extended: true,
                fast: false,
            },
        }
    }

    /// Perform the initiator-side handshake on a dialed `stream` (with
    /// `remote` the peer's known destination) and, on success, register the
    /// peer and spawn its reader/writer threads: write ours, read theirs.
    ///
    /// # Errors
    ///
    /// Handshake I/O failure or an info-hash mismatch (wrong torrent).
    pub fn attach<S: I2pStream + 'static>(
        &self,
        mut stream: S,
        remote: DestHash,
    ) -> std::io::Result<()> {
        // Bound the exchange. Best-effort: backends with no timeout of their
        // own ignore it, which is why this is not a guarantee — but the SAM
        // backend's streams are loopback sockets clove owns, so there it is a
        // real one. See [`HANDSHAKE_TIMEOUT`] for what it costs not to have.
        let _ = stream.set_timeouts(Some(HANDSHAKE_TIMEOUT));
        stream.write_all(&self.our_handshake().encode())?;
        let mut buf = [0u8; wire::HANDSHAKE_LEN];
        stream.read_exact(&mut buf)?;
        let theirs = Handshake::parse(&buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if theirs.info_hash != self.shared.info_hash {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer handshaked a different torrent",
            ));
        }
        // Back to blocking for the connection proper: a peer legitimately sits
        // quiet between messages, and cutting that off is the keep-alive and
        // idle-timeout work's job, not a socket option's.
        let _ = stream.set_timeouts(None);
        self.finish_attach(stream, remote, &theirs)
    }

    /// Attach an **accepted** peer whose handshake was already read by the
    /// inbound demux (that read is how the torrent was identified — Q4: one
    /// destination serves every torrent). Validates the info-hash, replies
    /// with our handshake, then registers the peer as [`attach`] does.
    ///
    /// # Errors
    ///
    /// An info-hash mismatch (mis-routed peer) or handshake-reply I/O failure.
    ///
    /// [`attach`]: Torrent::attach
    pub fn attach_accepted<S: I2pStream + 'static>(
        &self,
        mut stream: S,
        remote: DestHash,
        theirs: &Handshake,
    ) -> std::io::Result<()> {
        if theirs.info_hash != self.shared.info_hash {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer handshaked a different torrent",
            ));
        }
        // The demux bounded the read of *their* handshake; bound our reply
        // too, so a peer that connects and then stops reading cannot hold this
        // thread open by never draining 68 bytes.
        let _ = stream.set_timeouts(Some(HANDSHAKE_TIMEOUT));
        stream.write_all(&self.our_handshake().encode())?;
        let _ = stream.set_timeouts(None);
        // Counted here, not after `finish_attach`: what this number is
        // evidence *of* is that a remote router resolved our leaseSet and
        // carried a handshake in both directions, and that is now true. A
        // later failure to split the stream or spawn its threads is a local
        // problem and would not make the transport claim any less settled.
        self.shared
            .inbound
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.finish_attach(stream, remote, theirs)
    }

    /// Post-handshake common path: split the stream, register the peer, queue
    /// the opening messages, spawn the reader/writer threads.
    fn finish_attach<S: I2pStream + 'static>(
        &self,
        stream: S,
        remote: DestHash,
        theirs: &Handshake,
    ) -> std::io::Result<()> {
        // The client-wide ceiling, claimed before this connection costs
        // anything more. This is the authoritative check: the dial sweep and
        // the demux both look at `available()` first to avoid pointless work,
        // but that read is advisory and two torrents can reach here at the
        // same instant believing the same slot is free. Only one claim wins.
        let Some(slot) = self.shared.budget.claim() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "no room in the client's peer budget",
            ));
        };
        // Before the split, while the whole stream is still in hand: the two
        // halves are about to belong to threads that block on them, and
        // `remove_peer` needs a way to end the connection that does not.
        let closer = Arc::new(stream.closer()?);
        // Handshake done duplex; now split into independent halves so the
        // reader and writer run on separate threads (Q5 sync model).
        let (reader, writer) = stream.split()?;
        let (tx, rx) = sync_channel::<Message>(OUTGOING_QUEUE);

        // Registration can refuse: one destination may hold only
        // [`MAX_CONNECTIONS_PER_DEST`] connections here. Refusing returns the
        // budget slot and drops both stream halves, which closes the
        // connection — the same outcome the dialling side sees from any other
        // refusal.
        let Some(id) = self.shared.register_peer(tx.clone(), closer, remote, slot) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "this destination already holds its share of connections",
            ));
        };

        // Announce our piece set, then our extension handshake if the peer
        // speaks BEP 10.
        let bitfield = {
            let st = lock(&self.shared.state);
            Message::Bitfield(st.picker.have_field().as_bytes().to_vec())
        };
        let _ = tx.try_send(bitfield);
        if theirs.extensions.extended {
            let _ = tx.try_send(Message::Extended {
                id: 0,
                payload: self.shared.our_extension_handshake(),
            });
        }

        // A thread the OS will not give us ends this connection, not the
        // torrent: drop the peer so its budget slot and table entry go back.
        let writer_handle = match spawn_writer(writer, rx) {
            Ok(handle) => handle,
            Err(e) => {
                self.shared.remove_peer(id);
                return Err(e);
            }
        };
        let reader_handle = match spawn_reader(Arc::clone(&self.shared), id, reader) {
            Ok(handle) => handle,
            Err(e) => {
                // Removing the peer drops the sender, which ends the writer.
                self.shared.remove_peer(id);
                return Err(e);
            }
        };
        let mut threads = lock(&self.threads);
        // Reap the handles of peers that have already gone. Two per connection
        // accumulate otherwise, and a long-lived torrent sees a lot of churn.
        threads.retain(|handle| !handle.is_finished());
        threads.push(writer_handle);
        threads.push(reader_handle);
        Ok(())
    }
}

fn spawn_writer<W: std::io::Write + I2pClose + Send + 'static>(
    mut writer: W,
    rx: Receiver<Message>,
) -> std::io::Result<JoinHandle<()>> {
    peer_thread().spawn(move || {
        while let Ok(msg) = rx.recv() {
            if wire::write_message(&mut writer, &msg).is_err() {
                break;
            }
        }
        // The connection is over — either the peer was removed from the table
        // (which drops the sender this loop waits on) or a write failed — so
        // close it, in both directions, before this thread ends.
        //
        // This is what reclaims the *reader*. Both halves are one connection,
        // and dropping this one leaves the descriptor open because the reader
        // holds the other, so a removed peer's reader used to sit in a
        // blocking read for the life of the process: a thread, a descriptor
        // and a router-side stream each, leaked on every pause, idle drop and
        // session teardown. `disconnect_all` had a comment admitting as much.
        writer.close();
    })
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    shared: Arc<Shared>,
    id: u64,
    mut reader: R,
) -> std::io::Result<JoinHandle<()>> {
    peer_thread().spawn(move || {
        // One frame buffer for the life of the connection; see
        // `wire::read_frame_into`.
        let mut body = Vec::new();
        while wire::read_frame_into(&mut reader, shared.max_frame, &mut body).is_ok() {
            match Message::parse(&body) {
                Ok(msg) => shared.on_message(id, &msg),
                Err(_) => break, // protocol violation: drop the peer
            }
        }
        shared.remove_peer(id);
    })
}

/// Stack size for a peer's reader and writer threads.
///
/// Two per connection and up to `peer_limit` connections is several hundred
/// threads, and the default 2 MiB each is most of a gigabyte of address space.
/// Neither thread recurses over anything a peer controls — the deepest call is
/// `bencode`'s decoder, capped at [`MAX_DEPTH`](crate::bencode::MAX_DEPTH) —
/// so this is far more than either needs and eight times less than the default.
const PEER_STACK_BYTES: usize = 256 * 1024;

/// A builder for the threads that serve one peer connection.
fn peer_thread() -> std::thread::Builder {
    std::thread::Builder::new().stack_size(PEER_STACK_BYTES)
}

impl Shared {
    /// Add the peer to the table and return its id, or `None` when `dest`
    /// already holds [`MAX_CONNECTIONS_PER_DEST`] connections here.
    ///
    /// The check lives *inside* the lock that pushes the entry, because that is
    /// the only place it can be right: two inbound connections from one
    /// destination can be in `finish_attach` at the same instant, and a count
    /// taken before the lock would let both through. Same reasoning as the
    /// budget's compare-exchange, one level up.
    ///
    /// Taking `slot` by value is what returns it: on refusal the slot is dropped
    /// here, so the budget cannot leak a slot to a connection that was never
    /// registered.
    fn register_peer(
        &self,
        out: SyncSender<Message>,
        closer: Arc<dyn I2pClose + Send + Sync>,
        dest: DestHash,
        slot: PeerSlot,
    ) -> Option<u64> {
        let mut st = lock(&self.state);
        if st.peers.iter().filter(|p| p.dest == dest).count() >= MAX_CONNECTIONS_PER_DEST {
            return None;
        }
        let id = st.next_id;
        st.next_id += 1;
        st.remember_peer_from(dest, Source::Trusted);
        st.peers.push(Peer {
            id,
            _slot: slot,
            dest,
            out,
            closer,
            has: Bitfield::empty(self.num_pieces),
            peer_choking: true,
            we_choke: true,
            they_interested: false,
            we_interested: false,
            in_flight: HashMap::new(),
            strikes: 0,
            last_sent: Instant::now(),
            last_seen: Instant::now(),
            uploaded: 0,
            pex_id: None,
            metadata_id: None,
        });
        Some(id)
    }

    /// Our BEP 10 handshake payload: advertise `i2p_pex` and `ut_metadata`, and
    /// the metadata size if we hold the info dictionary.
    fn our_extension_handshake(&self) -> Vec<u8> {
        let mut ids = std::collections::BTreeMap::new();
        ids.insert(I2P_PEX.to_owned(), OUR_PEX_ID);
        ids.insert(UT_METADATA.to_owned(), OUR_METADATA_ID);
        let metadata_size = (!self.raw_info.is_empty()).then_some(self.raw_info.len());
        extension::Handshake {
            ids,
            metadata_size,
            client: Some("clove/0.1".to_owned()),
        }
        .encode()
    }

    fn remove_peer(&self, id: u64) {
        let mut closer = None;
        {
            let mut st = lock(&self.state);
            if let Some(pos) = st.peers.iter().position(|p| p.id == id) {
                let peer = st.peers.swap_remove(pos);
                st.picker.remove_bitfield(&peer.has);
                for (piece, block) in peer.in_flight.into_keys() {
                    st.picker.block_failed(piece, block);
                }
                closer = Some(peer.closer);
            }
            debug_check_state(&st);
        }
        // Outside the lock, because it wakes two threads that will want it:
        // the writer to run its own close and end, the reader to fall out of
        // `read_frame` and deregister. Idempotent, so the writer closing again
        // on its way out costs nothing.
        //
        // Removing the entry is not what reclaims the connection — this is.
        // The distinction cost two rounds of fixing the same leak: dropping
        // `out` only reaches a writer that is idle, and the peer worth dropping
        // is usually the one whose queue is full because it stopped reading.
        if let Some(closer) = closer {
            closer.close();
        }
    }

    /// Handle one message: mutate state under the lock, collect outgoing
    /// messages, then send them after releasing it.
    fn on_message(&self, id: u64, msg: &Message) {
        let mut out: Vec<Outgoing> = Vec::new();
        {
            let mut st = lock(&self.state);
            let now = Instant::now();
            // Anything at all from a peer — a keep-alive included — is proof it
            // is still there, which is what the idle timeout measures.
            if let Some(peer) = st.peers.iter_mut().find(|p| p.id == id) {
                peer.last_seen = now;
            }
            self.handle(&mut st, id, msg, &mut out);
            // A choke round is due on a clock, not on traffic, so the
            // maintenance tick is what normally runs it. This second check
            // costs one comparison per message and keeps the choker honest for
            // an embedder driving `Torrent` without a tick of its own.
            if st.last_choke_round.elapsed() >= st.choke_interval {
                st.last_choke_round = now;
                run_choker(&mut st, &mut out);
            }
            record_sent(&mut st, &out, now);
            // Every peer message can move piece accounting; check the whole
            // picture while the lock is still held and the state is settled.
            debug_check_state(&st);
        }
        self.send_all(out);
        self.check_done();
    }

    /// The periodic work, one tick's worth: drop peers that have gone silent,
    /// keep-alive the ones we have nothing else to say to, and run a choke round
    /// if one is due.
    fn maintain(&self) {
        let mut out: Vec<Outgoing> = Vec::new();
        let mut idle: Vec<u64> = Vec::new();
        {
            let mut st = lock(&self.state);
            let now = Instant::now();
            let (keepalive, timeout) = (st.keepalive_interval, st.idle_timeout);
            for peer in &st.peers {
                if now.duration_since(peer.last_seen) >= timeout {
                    idle.push(peer.id);
                } else if now.duration_since(peer.last_sent) >= keepalive {
                    out.push((peer.id, peer.out.clone(), Message::KeepAlive));
                }
            }
            maintain_requests(&mut st, now, &mut idle, &mut out);
            if st.last_choke_round.elapsed() >= st.choke_interval {
                st.last_choke_round = now;
                run_choker(&mut st, &mut out);
            }
            record_sent(&mut st, &out, now);
            debug_check_state(&st);
        }
        // Outside the lock: remove_peer takes it, and so does the send path.
        for id in idle {
            self.remove_peer(id);
        }
        self.send_all(out);
    }

    /// Hand every collected message to its peer's writer, dropping any peer
    /// whose queue will not take it.
    ///
    /// `try_send`, never `send`: the calling thread usually belongs to some
    /// *other* peer — a reader that completed a piece, or the maintenance tick —
    /// and a peer that has stopped reading fills its socket and then its queue.
    /// Blocking here would park that thread behind it, which is one silent peer
    /// stalling the whole torrent. A full queue means the peer is not reading
    /// (see [`OUTGOING_QUEUE`]); a closed one means its writer is already gone.
    /// Either way the connection is finished, and dropping it returns its
    /// in-flight blocks to the picker.
    fn send_all(&self, out: Vec<Outgoing>) {
        for (id, tx, msg) in out {
            if tx.try_send(msg).is_err() {
                self.remove_peer(id);
            }
        }
    }

    #[allow(clippy::too_many_lines)] // one dispatch table; splitting hurts readability
    fn handle(&self, st: &mut State, id: u64, msg: &Message, out: &mut Vec<Outgoing>) {
        let Some(idx) = st.peers.iter().position(|p| p.id == id) else {
            return;
        };
        match msg {
            // No-ops here: keep-alive, a cancel we serve synchronously so
            // never have queued, and fast-extension messages (BEP 6 off).
            Message::KeepAlive
            | Message::Cancel(_)
            | Message::RejectRequest(_)
            | Message::SuggestPiece(_)
            | Message::AllowedFast(_) => {}
            Message::Extended {
                id: ext_id,
                payload,
            } => {
                self.on_extended(st, idx, *ext_id, payload, out);
            }
            Message::Choke => {
                let peer = &mut st.peers[idx];
                peer.peer_choking = true;
                // Outstanding requests are dropped by a choking peer (no
                // fast extension here); release them for re-picking.
                let dropped: Vec<(u32, u32)> =
                    peer.in_flight.drain().map(|(block, _at)| block).collect();
                for (piece, block) in dropped {
                    st.picker.block_failed(piece, block);
                }
            }
            Message::Unchoke => {
                st.peers[idx].peer_choking = false;
                fill_requests(st, idx, out);
            }
            Message::Interested => {
                st.peers[idx].they_interested = true;
                run_choker(st, out);
            }
            Message::NotInterested => {
                st.peers[idx].they_interested = false;
                run_choker(st, out);
            }
            Message::Have(piece) => {
                // Count the piece only when the bit actually changes. A peer
                // that repeats a `have` — or spams one — would otherwise
                // inflate that piece's availability for good, because leaving
                // withdraws what its bitfield says exactly once, and
                // rarest-first would steer the whole torrent by it.
                if *piece < self.num_pieces && !st.peers[idx].has.has(*piece) {
                    st.peers[idx].has.set(*piece);
                    st.picker.add_single(*piece);
                    update_interest(st, idx, out);
                }
            }
            Message::Bitfield(bytes) => {
                if let Ok(field) = Bitfield::from_bytes(bytes, self.num_pieces) {
                    Self::replace_piece_set(st, idx, field);
                    update_interest(st, idx, out);
                }
            }
            Message::HaveAll => {
                Self::replace_piece_set(st, idx, Bitfield::full(self.num_pieces));
                update_interest(st, idx, out);
            }
            Message::HaveNone => {
                Self::replace_piece_set(st, idx, Bitfield::empty(self.num_pieces));
                update_interest(st, idx, out);
            }
            Message::Request(req) => self.serve_request(st, idx, *req, out),
            Message::Piece {
                index,
                begin,
                block,
            } => {
                self.on_block(st, idx, *index, *begin, block, out);
            }
        }
    }

    /// Swap in a peer's piece set, withdrawing what the old one contributed to
    /// availability first. State-only, so an associated function.
    ///
    /// BEP 3 sends the piece set once, right after the handshake, but nothing
    /// stops a peer repeating it or following it with have-all/have-none.
    /// Adding the new set without subtracting the old one leaks the difference
    /// permanently: a peer could announce every piece and then have-none, and
    /// the torrent would go on believing those copies exist.
    fn replace_piece_set(st: &mut State, idx: usize, field: Bitfield) {
        let old = std::mem::replace(&mut st.peers[idx].has, field);
        st.picker.remove_bitfield(&old);
        st.picker.add_bitfield(&st.peers[idx].has);
    }

    /// Route a BEP 10 extended message: id 0 is the handshake, otherwise the
    /// id is one we advertised (`i2p_pex` or `ut_metadata`).
    fn on_extended(
        &self,
        st: &mut State,
        idx: usize,
        ext_id: u8,
        payload: &[u8],
        out: &mut Vec<Outgoing>,
    ) {
        match ext_id {
            0 => {
                let Ok(hs) = extension::Handshake::parse(payload) else {
                    return;
                };
                st.peers[idx].pex_id = hs.id_for(I2P_PEX);
                st.peers[idx].metadata_id = hs.id_for(UT_METADATA);
                // Now that we know their pex id, send them the peers we know.
                Self::send_pex(st, idx, out);
            }
            OUR_PEX_ID => {
                if let Ok(pex) = PexMessage::parse(payload) {
                    for dest in pex.added {
                        if st.remember_peer_from(dest, Source::Pex) {
                            st.pex_learned = st.pex_learned.saturating_add(1);
                        }
                    }
                }
            }
            OUR_METADATA_ID => {
                if let Ok(MetadataMessage::Request { piece }) = MetadataMessage::parse(payload) {
                    self.serve_metadata(st, idx, piece, out);
                }
            }
            _ => {} // an id we never advertised
        }
    }

    /// Send `idx`'s peer the destinations we know (minus its own), if it
    /// supports peer exchange. State-only, so an associated function.
    fn send_pex(st: &mut State, idx: usize, out: &mut Vec<Outgoing>) {
        let Some(pex_id) = st.peers[idx].pex_id else {
            return;
        };
        let peer_dest = st.peers[idx].dest;
        // Never build a message our own parser would reject: `PexMessage::parse`
        // treats more than MAX_PEX_PEERS destinations as spam and drops the
        // whole thing, so an uncapped send would silently stop working for
        // exactly the busy torrents peer exchange is for.
        let added: Vec<DestHash> = st
            .known_peers
            .keys()
            .copied()
            .filter(|&d| d != peer_dest)
            .take(crate::pex::MAX_PEX_PEERS)
            .collect();
        let msg = PexMessage {
            added,
            dropped: Vec::new(),
        };
        if msg.is_empty() {
            return;
        }
        out.push((
            st.peers[idx].id,
            st.peers[idx].out.clone(),
            Message::Extended {
                id: pex_id,
                payload: msg.encode(),
            },
        ));
    }

    /// Serve one metadata piece (or reject it) in response to a request.
    fn serve_metadata(&self, st: &mut State, idx: usize, piece: u32, out: &mut Vec<Outgoing>) {
        let Some(metadata_id) = st.peers[idx].metadata_id else {
            return;
        };
        let total = self.raw_info.len();
        let start = piece as usize * METADATA_PIECE_LEN;
        let reply = if self.raw_info.is_empty() || start >= total {
            MetadataMessage::Reject { piece }
        } else {
            let end = (start + METADATA_PIECE_LEN).min(total);
            MetadataMessage::Data {
                piece,
                total_size: u32::try_from(total).unwrap_or(u32::MAX),
                data: self.raw_info[start..end].to_vec(),
            }
        };
        out.push((
            st.peers[idx].id,
            st.peers[idx].out.clone(),
            Message::Extended {
                id: metadata_id,
                payload: reply.encode(),
            },
        ));
    }

    /// A peer sent us a block: validate, persist, advance the picker, and on
    /// piece completion verify, announce, and refill.
    fn on_block(
        &self,
        st: &mut State,
        idx: usize,
        index: u32,
        begin: u32,
        block: &[u8],
        out: &mut Vec<Outgoing>,
    ) {
        if !begin.is_multiple_of(BLOCK_LEN) || index >= self.num_pieces {
            return;
        }
        let block_no = begin / BLOCK_LEN;
        if block.len() as u64 != u64::from(st.picker.block_len(index, block_no)) {
            return;
        }
        let was_requested = st.peers[idx].in_flight.remove(&(index, block_no)).is_some();
        // Any block at all is proof this peer is still working for us, which
        // is what the strike count measures. Cleared even for a late or
        // duplicate block: the peer answered, just not usefully.
        st.peers[idx].strikes = 0;
        // A block for a piece we already hold is a duplicate the endgame asked
        // for: another peer answered first and the piece verified. Writing it
        // would put this peer's bytes over verified ones, and for a piece of
        // more than one block nothing would re-verify afterwards — so a peer
        // that answers late with rubbish would silently corrupt a finished
        // piece we go on to announce and serve.
        if !was_requested || st.picker.has(index) {
            // Unsolicited, already satisfied, or late; ignore the payload but
            // still try to keep the pipeline full below.
        } else if self.storage.write_block(index, begin, block).is_ok() {
            self.downloaded
                .fetch_add(block.len() as u64, std::sync::atomic::Ordering::Relaxed);
            if !st.picker.block_received(index, block_no) {
                return;
            }
            // Piece complete: verify from disk before trusting it.
            match self.storage.verify_piece(index) {
                Ok(true) => {
                    st.picker.set_have(index);
                    for peer in &st.peers {
                        out.push((peer.id, peer.out.clone(), Message::Have(index)));
                    }
                }
                _ => st.picker.reset_piece(index),
            }
        }
        fill_requests(st, idx, out);
    }

    /// Serve a block if we are unchoking this peer and hold the piece.
    fn serve_request(
        &self,
        st: &mut State,
        idx: usize,
        req: BlockRequest,
        out: &mut Vec<Outgoing>,
    ) {
        // The request has to lie inside the piece it names. Storage only
        // bounds a read against the whole torrent, so a range that runs off
        // the end of one piece reads into the next — bytes we may not hold and
        // certainly have not verified as part of *this* piece.
        let piece_end = u64::from(req.begin) + u64::from(req.length);
        if st.peers[idx].we_choke
            || req.length == 0
            || req.length > BLOCK_LEN
            || piece_end > u64::from(st.picker.piece_len(req.index))
            || !st.picker.has(req.index)
        {
            return;
        }
        if let Ok(data) = self.storage.read_block(req.index, req.begin, req.length) {
            self.uploaded
                .fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let peer = &mut st.peers[idx];
            peer.uploaded += data.len() as u64;
            out.push((
                peer.id,
                peer.out.clone(),
                Message::Piece {
                    index: req.index,
                    begin: req.begin,
                    block: data,
                },
            ));
        }
    }

    fn check_done(&self) {
        if lock(&self.state).picker.is_complete() {
            let mut done = lock(&self.done);
            if !*done {
                *done = true;
                self.done_cv.notify_all();
            }
        }
    }
}

/// Note that each peer named in `out` has had something queued for it, so the
/// keep-alive clock starts again for them.
///
/// Done once over the batch rather than at each `out.push` site: queuing is what
/// matters for the clock, and there are a dozen places that queue.
fn record_sent(st: &mut State, out: &[Outgoing], now: Instant) {
    for (id, _, _) in out {
        if let Some(peer) = st.peers.iter_mut().find(|p| p.id == *id) {
            peer.last_sent = now;
        }
    }
}

/// Recompute whether we are interested in this peer and send the transition
/// if it changed. Called after learning what they have. State-only, so a
/// free function rather than a method.
fn update_interest(st: &mut State, idx: usize, out: &mut Vec<Outgoing>) {
    let peer = &st.peers[idx];
    let want = peer.has.iter_present().any(|p| !st.picker.has(p));
    if want && !peer.we_interested {
        st.peers[idx].we_interested = true;
        out.push((
            st.peers[idx].id,
            st.peers[idx].out.clone(),
            Message::Interested,
        ));
        // If they are already unchoking us we can request right away.
        if !st.peers[idx].peer_choking {
            fill_requests(st, idx, out);
        }
    } else if !want && peer.we_interested {
        st.peers[idx].we_interested = false;
        out.push((
            st.peers[idx].id,
            st.peers[idx].out.clone(),
            Message::NotInterested,
        ));
    }
}

/// Give up on blocks nobody answered, on peers that keep not answering, and
/// top every surviving pipeline back up.
///
/// **Expiry.** Three things happen per expired request, and all three matter:
///
/// - the block goes back to the picker, so its piece can finish somewhere
///   else — without this a piece stalls short of complete, permanently;
/// - the peer's pipeline slot is freed, so it can be given work it might
///   actually do — without this a peer's capacity bleeds away request by
///   request until it asks for nothing while still holding a connection;
/// - the peer earns a strike, and at [`REQUEST_STRIKES`] its slot goes back
///   to the swarm.
///
/// **Topping up.** Every eligible peer is refilled, not just the ones that
/// timed out. [`fill_requests`] otherwise runs only when a block arrives or a
/// peer unchokes, so a peer whose pipeline drains at a moment when the picker
/// has nothing to offer — because every remaining block is in flight with
/// somebody else — is never asked again. It stays connected, interested,
/// unchoked and permanently idle, and no event exists that would wake it: the
/// one that would is a block arriving on the connection that has nothing
/// outstanding.
///
/// That is a second way to stall, independent of the first and reachable
/// without a single misbehaving peer — a slow peer holding the last few blocks
/// is enough. It is also why freeing the expired blocks above is not on its
/// own sufficient: the honest peers that could take them have long since gone
/// quiet.
fn maintain_requests(st: &mut State, now: Instant, idle: &mut Vec<u64>, out: &mut Vec<Outgoing>) {
    let deadline = st.request_timeout;
    for idx in 0..st.peers.len() {
        let expired: Vec<(u32, u32)> = st.peers[idx]
            .in_flight
            .iter()
            .filter(|(_, requested_at)| now.duration_since(**requested_at) >= deadline)
            .map(|(block, _)| *block)
            .collect();
        if !expired.is_empty() {
            for block in &expired {
                st.peers[idx].in_flight.remove(block);
                st.picker.block_failed(block.0, block.1);
            }
            let peer = &mut st.peers[idx];
            peer.strikes = peer.strikes.saturating_add(1);
            if peer.strikes >= REQUEST_STRIKES {
                // Queued for removal by the caller, which does it outside the
                // lock; whatever it still holds goes back to the picker there.
                // No point topping up a peer we are about to drop.
                idle.push(peer.id);
                continue;
            }
        }
        fill_requests(st, idx, out);
    }
}

/// Top up a peer's request pipeline from the picker.
fn fill_requests(st: &mut State, idx: usize, out: &mut Vec<Outgoing>) {
    if st.peers[idx].peer_choking || !st.peers[idx].we_interested {
        return;
    }
    let space = PIPELINE_DEPTH.saturating_sub(st.peers[idx].in_flight.len());
    if space == 0 {
        return;
    }
    let has = st.peers[idx].has.clone();
    let requests = st.picker.pick(&has, space);

    // In endgame the picker deliberately hands the same block to more than
    // one peer. It has no per-peer view, though, so it can also hand a block
    // back to the peer that already owes it — a wasted request, and a count
    // the picker would never see settled, since the peer answers once. Drop
    // those and give the count straight back.
    let mut duplicates = Vec::new();
    let peer = &mut st.peers[idx];
    let now = Instant::now();
    for req in requests {
        let block = req.begin / BLOCK_LEN;
        if peer.in_flight.insert((req.index, block), now).is_none() {
            out.push((peer.id, peer.out.clone(), Message::Request(req)));
        } else {
            duplicates.push((req.index, block));
        }
    }
    for (index, block) in duplicates {
        st.picker.block_failed(index, block);
    }
}

/// Run a choke round and enqueue the resulting choke/unchoke messages.
fn run_choker(st: &mut State, out: &mut Vec<Outgoing>) {
    let snapshots: Vec<PeerSnapshot> = st
        .peers
        .iter()
        .map(|p| PeerSnapshot {
            id: p.id,
            interested: p.they_interested,
            rate: p.uploaded,
            unchoked: !p.we_choke,
        })
        .collect();
    let decision = st.choker.plan(&snapshots);
    for id in decision.unchoke {
        if let Some(peer) = st.peers.iter_mut().find(|p| p.id == id) {
            peer.we_choke = false;
            out.push((peer.id, peer.out.clone(), Message::Unchoke));
        }
    }
    for id in decision.choke {
        if let Some(peer) = st.peers.iter_mut().find(|p| p.id == id) {
            peer.we_choke = true;
            out.push((peer.id, peer.out.clone(), Message::Choke));
        }
    }
}

/// Frame ceiling for the metadata-fetch handshake flow: a `ut_metadata` data
/// message is one 16 KiB piece plus small header/extension overhead.
const METADATA_FRAME: u32 = 16 * 1024 + 256; // METADATA_PIECE_LEN + overhead

/// Longest a metadata fetch may take before the peer is treated as stalling.
///
/// Covers the exchange end to end — our handshake, theirs, and the assembly —
/// not just the part after both handshakes. The half before them is the
/// cheaper half to stall in, because it needs the peer to say nothing at all.
const METADATA_DEADLINE: Duration = Duration::from_secs(120);

/// Per-read/write socket bound for a metadata stream.
///
/// [`METADATA_DEADLINE`] is only consulted between frames, so it cannot end a
/// thread parked inside one. A peer that accepts the stream and then sends
/// nothing — or stops half-way through a frame's length prefix — is stopped by
/// this and nothing else.
const METADATA_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Frames a peer may spend before sending its extension handshake.
///
/// Generous, because a peer legitimately opens with a bitfield and a have or
/// two; finite, because the loop that waits for the handshake ignores whatever
/// else arrives, and a peer that never handshakes is otherwise free to keep
/// sending it.
const METADATA_GREETING_FRAMES: u32 = 64;

/// Frames of slack per metadata piece, over the one useful reply each.
///
/// The exchange needs a bound of its own because nothing else can end it: a
/// peer that re-sends a piece we already hold, or sends one of the wrong
/// length, makes no progress and costs nothing, so "read until complete" is a
/// loop a peer can keep us in for as long as it cares to. A read timeout
/// underneath us would eventually notice — SAM streams take one now — but the
/// bound belongs here regardless: a peer that answers promptly and uselessly
/// never trips a timeout at all.
const METADATA_FRAME_SLACK: u32 = 8;

/// Read frames until the peer's extension handshake arrives, and take the
/// `ut_metadata` id and metadata size out of it.
///
/// Bounded like the assembly loop that follows it: "ignore anything else until
/// the handshake arrives" is an invitation to send anything else forever, and a
/// peer taking it up is indistinguishable from a merely chatty one until you
/// count. The budget is frames *and* a deadline because those catch different
/// peers — one that floods cheap frames, and one that sends a few very slowly.
fn await_extension_handshake<S: std::io::Read>(
    stream: &mut S,
    deadline: std::time::Instant,
) -> std::io::Result<(u8, usize)> {
    let invalid = |m: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
    let mut frames = METADATA_GREETING_FRAMES;
    loop {
        if frames == 0 {
            return Err(invalid("peer sent frame after frame without handshaking"));
        }
        frames -= 1;
        if std::time::Instant::now() >= deadline {
            return Err(invalid("peer did not finish its handshake in time"));
        }
        let body = wire::read_frame(stream, METADATA_FRAME)?;
        if let Ok(Message::Extended { id: 0, payload }) = Message::parse(&body) {
            let hs =
                extension::Handshake::parse(&payload).map_err(|_| invalid("bad ext handshake"))?;
            return match (hs.id_for(UT_METADATA), hs.metadata_size) {
                (Some(mid), Some(size)) => Ok((mid, size)),
                _ => Err(invalid("peer does not serve metadata")),
            };
        }
        // Ignore anything else (bitfield, etc.) until the handshake arrives.
    }
}

/// Fetch and verify a torrent's `info` dictionary from one peer over BEP 9
/// (`ut_metadata`) — the magnet bootstrap. Blocking and sequential, so it
/// runs on the dialing thread against a duplex stream before the full peer
/// connection (and storage/picker) exist.
///
/// The reassembled bytes are checked against `info_hash` inside the
/// assembler, so a peer cannot serve a different torrent.
///
/// Bounded end to end by `METADATA_DEADLINE` (120s), with
/// `METADATA_IO_TIMEOUT` (30s) underneath it so a peer that says nothing at
/// all is bounded too.
///
/// # Errors
///
/// Handshake failure, a peer that does not offer metadata, a rejected
/// piece, verification failure, the deadline passing, or any I/O error.
pub fn fetch_metadata<S: I2pStream>(
    mut stream: S,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
) -> std::io::Result<MetaInfo> {
    let invalid = |m: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);

    // One clock for the whole exchange, started before the first byte rather
    // than after the extension handshake. A peer that accepts the stream and
    // then stops — before our handshake, between it and theirs, or part-way
    // through a frame — used to park this thread for good, and because the
    // fetch walks candidate peers one at a time, that peer alone was enough to
    // stop a magnet ever resolving.
    let deadline = std::time::Instant::now() + METADATA_DEADLINE;
    let _ = stream.set_timeouts(Some(METADATA_IO_TIMEOUT));

    let ours = Handshake {
        info_hash,
        peer_id,
        extensions: Extensions {
            extended: true,
            fast: false,
        },
    };
    stream.write_all(&ours.encode())?;
    let mut buf = [0u8; wire::HANDSHAKE_LEN];
    stream.read_exact(&mut buf)?;
    let theirs = Handshake::parse(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if theirs.info_hash != info_hash {
        return Err(invalid("peer handshaked a different torrent"));
    }
    if !theirs.extensions.extended {
        return Err(invalid("peer does not speak the extension protocol"));
    }

    // Advertise ut_metadata and send our handshake.
    let mut ids = std::collections::BTreeMap::new();
    ids.insert(UT_METADATA.to_owned(), OUR_METADATA_ID);
    let our_ext = extension::Handshake {
        ids,
        metadata_size: None,
        client: Some("clove/0.1".to_owned()),
    };
    wire::write_message(
        &mut stream,
        &Message::Extended {
            id: 0,
            payload: our_ext.encode(),
        },
    )?;

    let (their_meta_id, total_size) = await_extension_handshake(&mut stream, deadline)?;

    let mut asm =
        metadata::MetadataAssembler::new(total_size).map_err(|_| invalid("bad metadata size"))?;
    for piece in 0..asm.num_pieces() {
        let req = MetadataMessage::Request { piece };
        wire::write_message(
            &mut stream,
            &Message::Extended {
                id: their_meta_id,
                payload: req.encode(),
            },
        )?;
    }
    let mut frames = asm
        .num_pieces()
        .saturating_mul(METADATA_FRAME_SLACK)
        .saturating_add(32);
    while !asm.is_complete() {
        if frames == 0 {
            return Err(invalid(
                "peer sent frame after frame without completing the metadata",
            ));
        }
        frames -= 1;
        if std::time::Instant::now() >= deadline {
            return Err(invalid("peer did not finish serving the metadata in time"));
        }
        let body = wire::read_frame(&mut stream, METADATA_FRAME)?;
        let Ok(Message::Extended { id, payload }) = Message::parse(&body) else {
            continue;
        };
        if id != OUR_METADATA_ID {
            continue; // peers reply using the id we advertised
        }
        match MetadataMessage::parse(&payload) {
            Ok(MetadataMessage::Data { piece, data, .. }) => {
                let _ = asm.add_piece(piece, &data);
            }
            Ok(MetadataMessage::Reject { .. }) => {
                return Err(invalid("peer rejected a metadata piece"));
            }
            _ => {}
        }
    }
    let bytes = asm
        .finish(info_hash)
        .ok_or_else(|| invalid("metadata failed info-hash verification"))?;
    MetaInfo::from_info_dict(&bytes)
        .map_err(|_| invalid("fetched metadata is not a valid info dict"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::{FileEntry, InfoHash};
    use i2pnet::mock::MockNet;
    use i2pnet::{I2pDialer, I2pListener};
    use sha1::{Digest, Sha1};
    use std::io::{Read as _, Write as _};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static C: AtomicU32 = AtomicU32::new(0);
            let n = C.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("clove-torrent-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn meta_for(files: Vec<FileEntry>, piece_length: u32, content: &[u8]) -> MetaInfo {
        let total: u64 = files.iter().map(|f| f.length).sum();
        assert_eq!(total, content.len() as u64);
        let pieces: Vec<[u8; 20]> = content
            .chunks(piece_length as usize)
            .map(|c| Sha1::digest(c).into())
            .collect();
        MetaInfo {
            info_hash: InfoHash([0x33; 20]),
            name: "demo".into(),
            piece_length,
            pieces: pieces.into(),
            files,
            total_length: total,
            private: true,
            trackers: vec![],
            skipped_trackers: 0,
            raw_info: Arc::from([].as_slice()),
        }
    }

    /// Build a real single-file torrent via bencode+parse, so `raw_info` is
    /// the genuine info-dict bytes (needed to serve BEP 9 metadata).
    fn real_meta(content: &[u8]) -> MetaInfo {
        use crate::bencode::{Value, encode};
        use std::collections::BTreeMap;
        let pieces: Vec<u8> = content
            .chunks(BLOCK_LEN as usize)
            .flat_map(|c| <[u8; 20]>::from(Sha1::digest(c)))
            .collect();
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), Value::Bytes(b"demo".to_vec()));
        info.insert(b"piece length".to_vec(), Value::Int(i64::from(BLOCK_LEN)));
        info.insert(b"pieces".to_vec(), Value::Bytes(pieces));
        info.insert(
            b"length".to_vec(),
            Value::Int(i64::try_from(content.len()).unwrap()),
        );
        let mut root = BTreeMap::new();
        root.insert(b"info".to_vec(), Value::Dict(info));
        MetaInfo::parse(&encode(&Value::Dict(root))).unwrap()
    }

    /// A three-file torrent, one piece per file, so a file maps to exactly one
    /// piece and "which file was skipped" is readable off the wire.
    fn three_file_meta(content: &[u8]) -> MetaInfo {
        let each = u64::from(BLOCK_LEN);
        let files = ["a.bin", "b.bin", "c.bin"]
            .iter()
            .map(|name| FileEntry {
                path: vec!["demo".into(), (*name).into()],
                length: each,
            })
            .collect();
        meta_for(files, BLOCK_LEN, content)
    }

    /// Attach a raw peer that claims everything and unchokes, then collect
    /// every piece index the engine asks it for within `window`.
    fn indices_requested(
        net: &MockNet,
        torrent: &Arc<Torrent>,
        dest: DestHash,
        num_pieces: u32,
        window: Duration,
    ) -> Vec<u32> {
        let ep = net.endpoint();
        let mut peer = ep.dial(dest, Duration::from_secs(5)).unwrap();
        let ours = Handshake {
            info_hash: torrent.info_hash(),
            peer_id: *b"-XX0000-priopriopr00",
            extensions: Extensions {
                extended: false,
                fast: false,
            },
        };
        peer.write_all(&ours.encode()).unwrap();
        let mut buf = [0u8; wire::HANDSHAKE_LEN];
        peer.read_exact(&mut buf).unwrap();

        let mut full = vec![0u8; (num_pieces as usize).div_ceil(8)];
        for p in 0..num_pieces as usize {
            full[p / 8] |= 0x80 >> (p % 8);
        }
        wire::write_message(&mut peer, &Message::Bitfield(full)).unwrap();
        wire::write_message(&mut peer, &Message::Unchoke).unwrap();

        // Bounded, so a piece that is never requested ends the read instead of
        // hanging it — the absence is exactly what this asserts on.
        peer.set_timeouts(Some(Duration::from_millis(200)));
        let deadline = std::time::Instant::now() + window;
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            let Ok(body) = wire::read_frame(&mut peer, 1 << 20) else {
                continue;
            };
            if let Ok(Message::Request(req)) = Message::parse(&body)
                && !seen.contains(&req.index)
            {
                seen.push(req.index);
            }
        }
        seen
    }

    /// The engine must never ask for a piece the user set to skip, and must ask
    /// for a high-priority one first.
    ///
    /// Asserted on the wire, because everything upstream of it was already
    /// right: `clove priority` validated the vector, stored it, persisted it and
    /// reported success, `clove list` displayed it, and the manual documented
    /// `0 = skip`. The one thing nothing did was tell the engine, so a skipped
    /// file downloaded in full.
    #[test]
    fn priorities_decide_what_the_engine_asks_for() {
        let net = MockNet::new();
        let content: Vec<u8> = (0..(3 * BLOCK_LEN))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let meta = three_file_meta(&content);

        let dir = TempDir::new("prio-skip");
        let torrent = Torrent::new(
            &meta,
            Arc::new(Storage::create(&meta, &dir.0, false).unwrap()),
            &Bitfield::empty(3),
            Mode::Sequential,
            *b"-CV0001-prioprioprio",
        );
        // Skip the middle file, raise the last: with sequential selection the
        // untouched order would be 0, 1, 2, so both effects are visible.
        torrent.set_piece_priorities(&meta.piece_priorities(&[1, 0, 2]));

        let ep = net.endpoint();
        let dest = ep.dest();
        let torrent_for_accept = Arc::clone(&torrent);
        let accept = std::thread::spawn(move || {
            if let Ok((stream, from)) = ep.accept() {
                let mut buf = [0u8; wire::HANDSHAKE_LEN];
                let mut stream = stream;
                if stream.read_exact(&mut buf).is_ok()
                    && let Ok(hs) = Handshake::parse(&buf)
                {
                    let _ = torrent_for_accept.attach_accepted(stream, from, &hs);
                }
            }
        });

        let asked = indices_requested(&net, &torrent, dest, 3, Duration::from_secs(2));
        accept.join().unwrap();

        assert!(
            !asked.contains(&1),
            "the skipped file's piece was requested anyway: {asked:?}"
        );
        assert_eq!(
            asked.first(),
            Some(&2),
            "the high-priority file should be asked for first: {asked:?}"
        );
        assert!(
            asked.contains(&0),
            "the normal file must still be downloaded: {asked:?}"
        );
        // And the torrent is finished once the two wanted pieces land, rather
        // than waiting for a piece it will never ask for.
        assert_eq!(torrent.bytes_left(), 2 * u64::from(BLOCK_LEN));
    }

    /// Changing priorities on a running torrent takes effect on that torrent,
    /// not merely on the next start.
    #[test]
    fn a_live_torrent_picks_up_a_priority_change() {
        let content: Vec<u8> = (0..(3 * BLOCK_LEN))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let meta = three_file_meta(&content);
        let dir = TempDir::new("prio-live");
        let torrent = Torrent::new(
            &meta,
            Arc::new(Storage::create(&meta, &dir.0, false).unwrap()),
            &Bitfield::empty(3),
            Mode::Sequential,
            *b"-CV0001-priolivepri0",
        );

        assert_eq!(torrent.bytes_left(), 3 * u64::from(BLOCK_LEN));
        assert!(!torrent.is_complete());

        torrent.set_piece_priorities(&meta.piece_priorities(&[0, 0, 1]));
        assert_eq!(torrent.bytes_left(), u64::from(BLOCK_LEN));

        // Skipping everything finishes it: there is nothing left to wait for.
        torrent.set_piece_priorities(&meta.piece_priorities(&[0, 0, 0]));
        assert_eq!(torrent.bytes_left(), 0);
        assert!(torrent.is_complete());
    }

    /// Two instances negotiate BEP 10 and one learns a third peer via
    /// `i2p_pex`.
    #[test]
    fn peers_exchange_via_i2p_pex() {
        let net = MockNet::new();
        let content = vec![7u8; 100];
        let meta = real_meta(&content);
        let peer_id = *b"-CV0001-pexpexpexpex";

        let dir_a = TempDir::new("pex-a");
        let dir_b = TempDir::new("pex-b");
        let a = Torrent::new(
            &meta,
            Arc::new(Storage::create(&meta, &dir_a.0, false).unwrap()),
            &Bitfield::empty(1),
            Mode::RarestFirst,
            peer_id,
        );
        let b = Torrent::new(
            &meta,
            Arc::new(Storage::create(&meta, &dir_b.0, false).unwrap()),
            &Bitfield::empty(1),
            Mode::RarestFirst,
            peer_id,
        );

        // A already knows a third peer X, which B has never seen.
        let x = DestHash([0xAB; 32]);
        a.add_peers(&[x]);

        let ep_a = net.endpoint();
        let ep_b = net.endpoint();
        let a_dest = ep_a.dest();
        let b_dest = ep_b.dest();

        let a_bg = Arc::clone(&a);
        let accept = std::thread::spawn(move || {
            let (stream, from) = ep_a.accept().unwrap();
            a_bg.attach(stream, from).unwrap();
        });
        let stream = ep_b.dial(a_dest, Duration::from_secs(5)).unwrap();
        b.attach(stream, a_dest).unwrap();
        accept.join().unwrap();
        let _ = b_dest;

        // B should learn X over PEX within a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if b.known_peers().contains(&x) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            b.known_peers().contains(&x),
            "B never learned peer X via i2p_pex"
        );

        // The counter "PEX acquisition observed" is read from. It must count
        // only what PEX taught us: B also knows A, from the connection itself,
        // and A knows both B and X without either arriving over PEX.
        assert_eq!(b.pex_learned(), 1, "B learned exactly one peer over PEX");
        assert_eq!(
            a.pex_learned(),
            0,
            "A learned nothing over PEX; its peers came from add_peers and \
             from the connection"
        );
    }

    /// A magnet client fetches and verifies the info dictionary over BEP 9
    /// from a peer that holds it.
    #[test]
    fn fetches_metadata_over_bep9() {
        let net = MockNet::new();
        // Metadata spanning two 16 KiB pieces exercises reassembly.
        let content: Vec<u8> = (0..(3 * BLOCK_LEN))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let meta = real_meta(&content);
        let info_hash = meta.info_hash.0;
        assert!(!meta.raw_info.is_empty());

        // Server holds the metadata; storage is irrelevant to serving it.
        let dir = TempDir::new("meta-server");
        let server = Torrent::new(
            &meta,
            Arc::new(Storage::create(&meta, &dir.0, false).unwrap()),
            &Bitfield::empty(meta.pieces.len().try_into().unwrap()),
            Mode::RarestFirst,
            *b"-CV0001-serverserver",
        );

        let ep_s = net.endpoint();
        let ep_c = net.endpoint();
        let s_dest = ep_s.dest();

        let server_bg = Arc::clone(&server);
        let accept = std::thread::spawn(move || {
            let (stream, from) = ep_s.accept().unwrap();
            server_bg.attach(stream, from).unwrap();
        });

        let stream = ep_c.dial(s_dest, Duration::from_secs(5)).unwrap();
        let fetched = fetch_metadata(stream, info_hash, *b"-CV0001-clientclient").unwrap();
        accept.join().unwrap();

        assert_eq!(fetched.info_hash.0, info_hash);
        assert_eq!(fetched.name, "demo");
        assert_eq!(fetched.pieces, meta.pieces);
        assert_eq!(fetched.total_length, content.len() as u64);
    }

    /// A peer that answers the metadata request with an endless stream of
    /// pieces it knows we cannot use. The fetch has to give up: nothing else
    /// can end the exchange, because a piece of the wrong length costs the peer
    /// nothing and never advances the assembly.
    #[test]
    fn metadata_fetch_gives_up_on_a_peer_that_never_finishes() {
        let net = MockNet::new();
        let info_hash = [0x77u8; 20];
        let server_ep = net.endpoint();
        let server_dest = server_ep.dest();

        std::thread::spawn(move || {
            let Ok((mut stream, _from)) = server_ep.accept() else {
                return;
            };
            // Mirror the BT handshake, then advertise ut_metadata with a size
            // that needs three pieces.
            let mut buf = [0u8; wire::HANDSHAKE_LEN];
            if stream.read_exact(&mut buf).is_err() {
                return;
            }
            let ours = Handshake {
                info_hash,
                peer_id: *b"-XX0000-liarliarliar",
                extensions: Extensions {
                    extended: true,
                    fast: false,
                },
            };
            if stream.write_all(&ours.encode()).is_err() {
                return;
            }
            let mut ids = std::collections::BTreeMap::new();
            ids.insert(UT_METADATA.to_owned(), 3u8);
            let hs = extension::Handshake {
                ids,
                metadata_size: Some(40_000),
                client: Some("liar/1.0".to_owned()),
            };
            if wire::write_message(
                &mut stream,
                &Message::Extended {
                    id: 0,
                    payload: hs.encode(),
                },
            )
            .is_err()
            {
                return;
            }
            // Now answer forever with a piece of the wrong length, which the
            // assembler refuses and which therefore never completes anything.
            loop {
                let reply = MetadataMessage::Data {
                    piece: 0,
                    total_size: 40_000,
                    data: vec![0xAA; 100],
                };
                let msg = Message::Extended {
                    id: OUR_METADATA_ID,
                    payload: reply.encode(),
                };
                if wire::write_message(&mut stream, &msg).is_err() {
                    return;
                }
            }
        });

        let client_ep = net.endpoint();
        let stream = client_ep.dial(server_dest, Duration::from_secs(5)).unwrap();
        let started = std::time::Instant::now();
        let err = fetch_metadata(stream, info_hash, *b"-CV0001-clientclient")
            .expect_err("a peer that never finishes must not be waited on forever");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "took {:?} to give up",
            started.elapsed()
        );
    }

    /// The same refusal, one stage earlier: a peer that completes the
    /// `BitTorrent` handshake and then never sends an *extension* handshake,
    /// filling the wait with frames that are individually perfectly ordinary.
    ///
    /// The loop that waits for it ignores anything else by design, so a peer
    /// need only keep talking. This is the half of the exchange the frame budget
    /// and deadline did not originally cover — they began after the extension
    /// handshake, which is to say after the point a peer has to reach to be
    /// bounded at all.
    #[test]
    fn metadata_fetch_gives_up_on_a_peer_that_never_handshakes() {
        let net = MockNet::new();
        let info_hash = [0x55u8; 20];
        let server_ep = net.endpoint();
        let server_dest = server_ep.dest();

        std::thread::spawn(move || {
            let Ok((mut stream, _from)) = server_ep.accept() else {
                return;
            };
            let mut buf = [0u8; wire::HANDSHAKE_LEN];
            if stream.read_exact(&mut buf).is_err() {
                return;
            }
            let ours = Handshake {
                info_hash,
                peer_id: *b"-XX0000-quietquietqu",
                extensions: Extensions {
                    extended: true,
                    fast: false,
                },
            };
            if stream.write_all(&ours.encode()).is_err() {
                return;
            }
            // Never an extension handshake. Just `have`s, forever: valid
            // messages, cheap to send, and nothing the waiting loop reacts to.
            let mut piece = 0u32;
            loop {
                if wire::write_message(&mut stream, &Message::Have(piece)).is_err() {
                    return;
                }
                piece = piece.wrapping_add(1);
            }
        });

        let client_ep = net.endpoint();
        let stream = client_ep.dial(server_dest, Duration::from_secs(5)).unwrap();
        let started = std::time::Instant::now();
        let err = fetch_metadata(stream, info_hash, *b"-CV0001-clientclient")
            .expect_err("a peer that never handshakes must not be waited on forever");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("handshak"),
            "the error should name the stage that gave up: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "took {:?} to give up",
            started.elapsed()
        );
    }

    /// A seeder and a leecher over the mock network complete a
    /// full multi-piece, multi-file download.
    #[test]
    fn two_instances_download_over_mock() {
        let net = MockNet::new();
        // ~5 pieces across two files, last piece short.
        let content: Vec<u8> = (0..(4 * BLOCK_LEN + 500))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let split = 3 * BLOCK_LEN as usize;
        let files = vec![
            FileEntry {
                path: vec!["demo".into(), "a.bin".into()],
                length: split as u64,
            },
            FileEntry {
                path: vec!["demo".into(), "b.bin".into()],
                length: content.len() as u64 - split as u64,
            },
        ];
        let meta = meta_for(files, BLOCK_LEN, &content);

        // Seeder: storage pre-filled and verified.
        let seed_dir = TempDir::new("seed");
        let seed_storage = Arc::new(Storage::create(&meta, &seed_dir.0, false).unwrap());
        // Fill the seed piece by piece (files/piece boundaries differ, so
        // storage maps each piece's bytes to the right file spans).
        for p in 0..seed_storage.num_pieces() {
            let start = p as usize * BLOCK_LEN as usize;
            let end = (start + seed_storage.piece_len(p) as usize).min(content.len());
            seed_storage
                .write_block(p, 0, &content[start..end])
                .unwrap();
        }
        let seed_have = seed_storage.verify_all().unwrap();
        assert!(seed_have.is_full(), "seed must start complete");
        let seeder = Torrent::new(
            &meta,
            seed_storage,
            &seed_have,
            Mode::RarestFirst,
            *b"-CV0001-seedseedseed",
        );

        // Leecher: empty storage.
        let leech_dir = TempDir::new("leech");
        let leech_storage = Arc::new(Storage::create(&meta, &leech_dir.0, false).unwrap());
        let leecher = Torrent::new(
            &meta,
            Arc::clone(&leech_storage),
            &Bitfield::empty(meta.pieces.len().try_into().unwrap()),
            Mode::RarestFirst,
            *b"-CV0001-leechleechle",
        );

        // Wire them together over the mock: seeder accepts, leecher dials.
        let seed_ep = net.endpoint();
        let leech_ep = net.endpoint();
        let seed_dest = seed_ep.dest();
        let leech_dest = leech_ep.dest();

        let seeder_bg = Arc::clone(&seeder);
        let accept_thread = std::thread::spawn(move || {
            let (stream, from) = seed_ep.accept().unwrap();
            seeder_bg.attach(stream, from).unwrap();
        });

        let stream = leech_ep.dial(seed_dest, Duration::from_secs(5)).unwrap();
        leecher.attach(stream, seed_dest).unwrap();
        accept_thread.join().unwrap();
        let _ = leech_dest;

        assert!(
            leecher.wait_complete(Duration::from_secs(20)),
            "leecher did not complete the download"
        );

        // Every leeched piece is on disk and verifies against the metainfo.
        assert!(leech_storage.verify_all().unwrap().is_full());
        for p in 0..leech_storage.num_pieces() {
            let len = leech_storage.piece_len(p);
            let start = p as usize * BLOCK_LEN as usize;
            assert_eq!(
                leech_storage.read_block(p, 0, len).unwrap(),
                &content[start..start + len as usize],
                "piece {p} mismatch"
            );
        }
    }

    /// The loopback download: the same seeder/leecher exchange as the mock
    /// test, but over a **real** local router via the SAM backend — a seeder
    /// that `STREAM FORWARD`s and a leecher that dials its destination.
    ///
    /// Router-gated, so `#[ignore]`d: nothing in CI or `make test` runs it,
    /// and no target in this repo sets it up. To run it by hand, point it at a
    /// router already exposing `SAMv3` and ask for ignored tests:
    ///
    /// ```text
    /// CLOVE_SAM_PORT=7656 cargo test -p clove-core -- --ignored --nocapture
    /// ```
    ///
    /// Both destinations live on the one router, which is the harder topology:
    /// it asks the router to resolve a leaseSet it published seconds ago. A
    /// failure here is as likely to be the router as clove.
    #[test]
    #[ignore = "needs a live I2P router; set CLOVE_SAM_PORT and run with --ignored"]
    fn two_instances_download_over_sam() {
        use i2pnet::sam::{SamConfig, SamListener, SamSession};

        let port: u16 = std::env::var("CLOVE_SAM_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .expect("set CLOVE_SAM_PORT (e.g. 7656) and run with --ignored");

        // ~5 pieces, last one short. Single file keeps the test focused on the
        // transport; the mock test already covers multi-file piece mapping.
        let content: Vec<u8> = (0..(4 * BLOCK_LEN + 500))
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let files = vec![FileEntry {
            path: vec!["demo".into(), "a.bin".into()],
            length: content.len() as u64,
        }];
        let meta = meta_for(files, BLOCK_LEN, &content);

        // Seeder: storage pre-filled and verified complete.
        let seed_dir = TempDir::new("sam-seed");
        let seed_storage = Arc::new(Storage::create(&meta, &seed_dir.0, false).unwrap());
        for p in 0..seed_storage.num_pieces() {
            let start = p as usize * BLOCK_LEN as usize;
            let end = (start + seed_storage.piece_len(p) as usize).min(content.len());
            seed_storage
                .write_block(p, 0, &content[start..end])
                .unwrap();
        }
        let seed_have = seed_storage.verify_all().unwrap();
        assert!(seed_have.is_full(), "seed must start complete");
        let seeder = Torrent::new(
            &meta,
            seed_storage,
            &seed_have,
            Mode::RarestFirst,
            *b"-CV0001-seedseedseed",
        );

        // Leecher: empty storage.
        let leech_dir = TempDir::new("sam-leech");
        let leech_storage = Arc::new(Storage::create(&meta, &leech_dir.0, false).unwrap());
        let leecher = Torrent::new(
            &meta,
            Arc::clone(&leech_storage),
            &Bitfield::empty(meta.pieces.len().try_into().unwrap()),
            Mode::RarestFirst,
            *b"-CV0001-leechleechle",
        );

        // Bring up both sessions on the one router; the seeder forwards.
        let seed_session = Arc::new(
            SamSession::connect(&SamConfig {
                samv3_tcp_port: port,
                nickname: i2pnet::sam::unique_nickname("clove-it-seed"),
                ..Default::default()
            })
            .expect("seeder SAM session (is the router up with tunnels built?)"),
        );
        let seed_listener =
            SamListener::forward(Arc::clone(&seed_session)).expect("seeder STREAM FORWARD");
        let seed_dest = seed_listener.local_dest();
        let leech_session = SamSession::connect(&SamConfig {
            samv3_tcp_port: port,
            nickname: i2pnet::sam::unique_nickname("clove-it-leech"),
            ..Default::default()
        })
        .expect("leecher SAM session");

        // Seeder accepts one inbound peer for this test and attaches it.
        let seeder_bg = Arc::clone(&seeder);
        let accept = std::thread::spawn(move || {
            // The live path: skip a connection that arrived without its
            // destination header rather than treating it as the end.
            let (stream, from) = loop {
                if let Some(pair) = seed_listener.accept().expect("seeder accept") {
                    break pair;
                }
            };
            seeder_bg.attach(stream, from).expect("seeder attach");
        });

        // A fresh transient destination needs time to build tunnels and
        // publish its leaseSet before it is reachable, so an immediate dial
        // gets `CantReachPeer`. Retry through that warmup window. A failed
        // connect establishes nothing, so the seeder's single accept still
        // pairs with the one dial that succeeds.
        let deadline = std::time::Instant::now() + Duration::from_secs(240);
        let stream = loop {
            match leech_session.dial(seed_dest, Duration::from_secs(60)) {
                Ok(stream) => break stream,
                Err(e) if std::time::Instant::now() < deadline => {
                    eprintln!("leecher dial not ready yet ({e}); retrying in 5s…");
                    std::thread::sleep(Duration::from_secs(5));
                }
                Err(e) => panic!("leecher dial to seeder destination: {e}"),
            }
        };
        leecher.attach(stream, seed_dest).expect("leecher attach");
        assert!(
            leecher.wait_complete(Duration::from_secs(180)),
            "leecher did not complete the download over SAM within 180s"
        );
        accept.join().unwrap();

        // Bytes on disk match, end to end, through real tunnels.
        assert!(leech_storage.verify_all().unwrap().is_full());
        for p in 0..leech_storage.num_pieces() {
            let len = leech_storage.piece_len(p);
            let start = p as usize * BLOCK_LEN as usize;
            assert_eq!(
                leech_storage.read_block(p, 0, len).unwrap(),
                &content[start..start + len as usize],
                "piece {p} mismatch"
            );
        }
    }
}
