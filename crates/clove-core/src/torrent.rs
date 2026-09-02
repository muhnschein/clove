//! The torrent coordinator: peer connections wired to the picker, choker,
//! and storage — the Q5 sync thread-per-peer model in practice.
//!
//! Each connection runs two threads over one `i2pnet` stream (hence
//! [`I2pStream::split`]): a reader blocking on incoming messages, a writer
//! draining a bounded queue. Shared state (picker, choker, peer table) lives
//! behind one mutex; handlers compute outgoing messages under it and release it
//! *before* sending. Sends never block either — a reader's messages are often
//! for *other* peers — so a queue that will not take one means a dead
//! connection, and the peer is dropped.
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
//! The four choke/interest booleans on `Peer` are the sub-state of `Active`:
//! BEP 3 defines them as independent bits, all sixteen combinations reachable.
//! The module is generic over the `i2pnet` traits, so the same code runs
//! against the mock network in CI and a real router in production; peer
//! acquisition belongs to [`crate::swarm`].

use std::collections::{HashMap, HashSet};
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

/// The extended-message ids clove advertises; peers send these back to us. A
/// peer tells us its own ids in its handshake, and we use those when sending.
const OUR_PEX_ID: u8 = 1;
const OUR_METADATA_ID: u8 = 2;

/// How often a choke round is reconsidered (BEP 3's customary ten seconds).
/// Periodic, not event-driven: the optimistic slot only rotates if `plan` is
/// called again. Tunable per torrent (R5) via [`Torrent::set_choke_interval`].
pub const DEFAULT_CHOKE_INTERVAL: Duration = Duration::from_secs(10);

/// How often a keep-alive goes out to a peer we have said nothing else to.
/// BEP 3's convention is two minutes and clients hang up after a few, so this
/// is what stops *other* peers dropping us — costlier here than on clearnet,
/// where redialling does not mean building tunnels.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(100);

/// How long a block may stay requested before it is offered to somebody else.
///
/// BEP 3 has no acknowledgement, so a request the peer quietly dropped looks
/// exactly like one still in flight — and without a deadline it is *permanent*,
/// stalling its piece for good (`docs/PROTOCOL.i2p-bt` §4.7). Generous, since
/// an I2P round trip is seconds and re-requesting a merely slow answer wastes
/// bandwidth.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Consecutive maintenance rounds in which a peer may let requests expire
/// while delivering nothing before it is dropped. Freeing the blocks unsticks
/// the *download*; dropping the peer unsticks the *slot*. The destination stays
/// in `known_peers`, so the dial sweep may pick it up again after its backoff.
pub const REQUEST_STRIKES: u32 = 3;

/// How long a peer may say nothing at all before we drop it — three missed
/// keep-alives. Generous on purpose: tunnel latency and a loaded router are
/// both normal, and dropping a healthy peer costs more than waiting out a dead
/// one.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a peer has to complete the BEP 3 handshake, in either direction.
///
/// Generous, because an I2P round trip is slow — but *finite*: `i2pnet`'s dial
/// clears a stream's timeouts once the router answers, so without this a peer
/// that accepts and then says nothing stalls the swarm's whole dial sweep. The
/// bound is on the whole exchange, not per read: a peer dribbling one byte
/// inside each read's timeout used to renew it, and could hold the exchange
/// open for the timeout times the 68 bytes of a handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long one read or write of a handshake may block before the deadline
/// and the caller's stop flag are looked at again. What bounds how long a
/// pause or shutdown waits on an attach already in flight.
pub const HANDSHAKE_POLL: Duration = Duration::from_secs(1);

/// How often [`Torrent::spawn_maintenance`] wakes to do the periodic work: fine
/// enough to honour the intervals above to within a tick, coarse enough to be
/// free.
pub const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5);

/// Largest number of peer destinations one torrent will remember.
///
/// PEX is peer-controlled — 512 destinations per message, no limit on messages
/// — so the cap stops one peer pointing the dial sweep at thousands of
/// destinations that each cost a tunnel and a timeout. *Which* entries it keeps
/// matters just as much: at the cap a trusted destination displaces a
/// PEX-learned one, and PEX displaces nothing.
pub const MAX_KNOWN_PEERS: usize = 1024;

/// Concurrent connections one I2P destination may hold on one torrent.
///
/// Nothing in `BitTorrent` needs the same peer twice, and without a cap one
/// destination can take every slot in the table — and through the shared
/// [`PeerBudget`], slots belonging to every other torrent, leaving its piece
/// set the only availability rarest-first can see. **Two, not one**, because a
/// second connection is legitimately ordinary: both sides dialling at once, or
/// a reconnect after a teardown we have not noticed. The cap need not be tight,
/// only finite.
pub const MAX_CONNECTIONS_PER_DEST: usize = 2;

/// Hash failures a destination may share the blame for before it is banned.
///
/// A piece that fails SHA-1 with a single supplier is that supplier's doing,
/// and it is banned on the spot. A piece several peers contributed to cannot
/// be pinned on one of them, so each is counted as suspect instead; an honest
/// peer caught in a liar's company earns the count back on every piece it
/// helps verify, a liar never does. The bound is what keeps one peer serving
/// rubbish from making a download burn bandwidth for the life of the run
/// (`docs/PROTOCOL.i2p-bt` §4.8).
pub const SUSPICION_LIMIT: u32 = 3;

/// Outgoing message queue depth per peer before the writer applies
/// backpressure. Bounded — no unbounded channels in the engine (SCOPE §4).
/// Deep enough that no honest peer reaches it, so a queue this full means the
/// peer has stopped reading and its connection is treated as dead.
const OUTGOING_QUEUE: usize = 256;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Cross-check the torrent's bookkeeping against the picker's, debug builds
/// only (SCOPE §9). The picker validates itself; what it cannot see is whether
/// the peer table agrees, and a mismatch there is how a download stalls.
#[cfg(debug_assertions)]
fn debug_check_state(state: &State) {
    state.picker.check_invariants();

    // The picker must never believe more blocks are owed than peers owe: a
    // count for a block nobody will deliver is never handed out again. The
    // reverse is normal — `set_have` drops a completed piece's accounting
    // while peers keep their entries until their responses arrive.
    let peer_in_flight: u64 = state.peers.iter().map(|p| p.in_flight.len() as u64).sum();
    let picker_in_flight = state.picker.in_flight_total();
    assert!(
        picker_in_flight <= peer_in_flight,
        "picker believes {picker_in_flight} blocks are in flight but peers owe only \
         {peer_in_flight}: a request was leaked and will never be re-offered"
    );

    // Exactly one count per connected peer holding the piece: anything else is
    // a double-counted `have` or a piece set replaced without withdrawing the
    // old one, which quietly distorts rarest-first for the whole torrent.
    let num_pieces = state.picker.have_field().len();
    for index in 0..num_pieces {
        let holders = state.peers.iter().filter(|p| p.has.has(index)).count();
        assert_eq!(
            state.picker.availability(index) as usize,
            holders,
            "piece {index}: availability disagrees with the peer table"
        );
    }

    // Ids come from a counter: two peers sharing one makes lookups ambiguous.
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
    /// The inbound path is the half of `PROTOCOL.i2p-bt` §2.5 no router-free
    /// test can reach: it needs a remote router to resolve our leaseSet. One
    /// non-zero reading against a public swarm settles it.
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
    /// How many destinations we first heard of over `i2p_pex`. Counted
    /// separately because `known_peers` grows from three sources at once.
    pex_learned: u64,
    /// Announces attempted, and why the last one failed — the answer to the
    /// one interesting question about a torrent with no peers.
    announces_ok: u32,
    announces_failed: u32,
    last_announce_error: Option<String>,
    /// Who wrote each block of a piece not yet verified, keyed by
    /// `(piece, block)`, so a failed hash can be laid at somebody's door.
    /// Entries live from the write to the verdict; the set is bounded by the
    /// blocks of the pieces in progress, as the picker's accounting is.
    suppliers: HashMap<(u32, u32), DestHash>,
    /// Destinations refused for the rest of the run: nothing is dialled,
    /// accepted or remembered from them. See [`SUSPICION_LIMIT`].
    banned: HashSet<DestHash>,
    /// Failed pieces each destination shared the blame for, less the pieces
    /// it has since helped verify.
    suspicion: HashMap<DestHash, u32>,
    /// Peers to remove once the lock is released. A handler runs under the
    /// state lock and `remove_peer` takes it, so a handler that decides a
    /// peer must go queues it here and the caller finishes the job.
    to_drop: Vec<u64>,
}

/// Where a known destination came from, which decides whether it may be
/// evicted to make room for another. One of these sources is a stranger's word
/// and the rest are not: without the distinction, two PEX messages from the
/// first peer to connect fill `known_peers` with addresses of its choosing and
/// no tracker reply can add a real peer until a restart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    /// A peer's word over `i2p_pex`: unverified, unbounded in supply, and
    /// therefore the first thing to go.
    Pex,
    /// From a tracker, an inbound dial, the operator, or a live connection.
    /// Not something a stranger can displace.
    Trusted,
}

impl State {
    /// Remember a destination we could dial, up to [`MAX_KNOWN_PEERS`].
    ///
    /// At the cap a [`Source::Trusted`] destination displaces a
    /// [`Source::Pex`] one; PEX displaces nothing, and when every entry is
    /// trusted the new one is refused. A trusted sighting upgrades a PEX entry;
    /// nothing downgrades one. Returns whether the destination was new to us.
    fn remember_peer_from(&mut self, dest: DestHash, source: Source) -> bool {
        // A ban outlives every source: the tracker and PEX will both go on
        // naming the destination, and neither may put it back.
        if self.banned.contains(&dest) {
            return false;
        }
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
            // Any PEX entry will do, and an arbitrary victim is one less
            // thing for a flood to game.
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

    /// Refuse `dest` for the rest of the run: forget it, and queue every
    /// connection it holds for removal.
    fn ban(&mut self, dest: DestHash) {
        self.banned.insert(dest);
        self.known_peers.remove(&dest);
        self.suspicion.remove(&dest);
        let held = self.peers.iter().filter(|p| p.dest == dest).map(|p| p.id);
        self.to_drop.extend(held);
    }

    /// A piece verified: its suppliers are off the hook for it, and each one
    /// earns back one of the failures it may have been party to.
    fn credit_suppliers(&mut self, index: u32) {
        let blocks = self.picker.blocks_in_piece(index);
        for block in 0..blocks {
            if let Some(dest) = self.suppliers.remove(&(index, block))
                && let Some(count) = self.suspicion.get_mut(&dest)
            {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.suspicion.remove(&dest);
                }
            }
        }
    }

    /// A piece failed SHA-1: a sole supplier is banned outright; several
    /// share the suspicion and any that reaches [`SUSPICION_LIMIT`] is banned.
    fn blame_suppliers(&mut self, index: u32) {
        let blocks = self.picker.blocks_in_piece(index);
        let mut contributors: Vec<DestHash> = Vec::new();
        for block in 0..blocks {
            if let Some(dest) = self.suppliers.remove(&(index, block))
                && !contributors.contains(&dest)
            {
                contributors.push(dest);
            }
        }
        let culprits: Vec<DestHash> = match contributors.as_slice() {
            [] => Vec::new(),
            [only] => vec![*only],
            many => many
                .iter()
                .copied()
                .filter(|dest| {
                    let count = self.suspicion.entry(*dest).or_insert(0);
                    *count = count.saturating_add(1);
                    *count >= SUSPICION_LIMIT
                })
                .collect(),
        };
        for dest in culprits {
            self.ban(dest);
        }
    }
}

// The four flags are the canonical BEP 3 choke/interest state; anything other
// than four booleans would obscure the protocol, so the lints are waived.
#[allow(clippy::struct_excessive_bools, clippy::struct_field_names)]
struct Peer {
    id: u64,
    /// This connection's slot in the client-wide [`PeerBudget`]. Held rather
    /// than read: dropping it with the entry is what returns the slot, so the
    /// budget cannot drift from the peer table on any removal path.
    _slot: PeerSlot,
    /// The peer's I2P destination, for peer exchange.
    dest: DestHash,
    out: SyncSender<Message>,
    /// Ends the connection, from whichever thread drops the peer. Dropping
    /// `out` is not enough: it wakes a writer *waiting* for a message, not one
    /// already blocked writing to a peer that stopped reading — precisely the
    /// peer we most want gone.
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

/// A running maintenance tick (see [`Torrent::spawn_maintenance`]). Dropping
/// it stops the thread; there is nothing to join.
pub struct Maintenance {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A message queued to a specific peer, collected under the lock and sent
/// after it is released. The id travels with it because the sender is often
/// *another* peer's thread — a broadcast `have`, a choke round — and needs to
/// know whose connection to drop if the queue will not take it.
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
    /// [`PeerBudget`] shared with the client's other torrents. What the daemon
    /// uses: the ceiling that matters is on concurrent streams against one SAM
    /// session, so it belongs to the client, not a torrent.
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
        // Must cover the largest message: a bitfield, a piece block, or a
        // ut_metadata data message. 256 bytes of slack covers the headers.
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
                suppliers: HashMap::new(),
                banned: HashSet::new(),
                suspicion: HashMap::new(),
                to_drop: Vec::new(),
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
    /// join the set advertised over peer exchange, capped at
    /// [`MAX_KNOWN_PEERS`] — which costs candidates, never connections.
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
    /// Takes effect on the next pick, and re-checks completion: dropping the
    /// only pieces a torrent was missing finishes it.
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
    /// durable once it returns. The pair is the point: a caller that fsyncs and
    /// then asks what was held can race a piece completing in between and call
    /// it durable when it is not.
    ///
    /// # Errors
    ///
    /// Any filesystem error flushing the torrent's files.
    pub fn sync_storage(&self) -> std::io::Result<Bitfield> {
        // Snapshot first, sync second, return the *earlier* set: anything that
        // completed during the sync may or may not have made it, and
        // under-claiming only costs a re-verify.
        let before = self.have();
        self.shared.storage.sync_all()?;
        Ok(before)
    }

    /// Start the periodic work this torrent needs: keep-alives, dropping peers
    /// that have gone silent, and choke rounds — all owed on a clock rather than
    /// in response to traffic, since a connection with nothing moving on it is
    /// exactly when a keep-alive matters. `period` is how often the thread
    /// wakes; dropping the returned handle stops it within one period, as does
    /// dropping the torrent.
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

    /// The known destinations the dial sweep may try: [`known_peers`] less
    /// any that are banned. Banning forgets a destination, so the two agree
    /// today; the sweep consults this one so that a ban placed between a
    /// listing and a dial still holds.
    ///
    /// [`known_peers`]: Torrent::known_peers
    #[must_use]
    pub fn dial_candidates(&self) -> Vec<DestHash> {
        let st = lock(&self.shared.state);
        st.known_peers
            .keys()
            .copied()
            .filter(|dest| !st.banned.contains(dest))
            .collect()
    }

    /// Whether `dest` is refused for the rest of this run — it served a piece
    /// that failed SHA-1, alone or in enough company (see
    /// [`SUSPICION_LIMIT`]). Checked before a handshake is spent on it.
    #[must_use]
    pub fn is_banned(&self, dest: DestHash) -> bool {
        lock(&self.shared.state).banned.contains(&dest)
    }

    /// How many destinations this run has banned, for status: a torrent that
    /// keeps failing pieces and a torrent that has caught the peer doing it
    /// look the same from `downloaded` alone.
    #[must_use]
    pub fn banned_count(&self) -> usize {
        lock(&self.shared.state).banned.len()
    }

    /// Record the outcome of one announce, for `clove show` to report: the
    /// announcer is the only thing that knows whether a torrent's lack of peers
    /// is a tracker problem.
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

    /// How many peers reached us inbound this run, through the router's
    /// `STREAM FORWARD`. Cumulative, not a count of live connections: a peer
    /// that connected and left still proves the inbound path carried a stream
    /// (`PROTOCOL.i2p-bt` §2.5).
    #[must_use]
    pub fn inbound_peers(&self) -> u64 {
        self.shared
            .inbound
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many peer destinations we first learned from an `i2p_pex` message —
    /// PEX acquisition, made checkable without a packet capture.
    #[must_use]
    pub fn pex_learned(&self) -> u64 {
        lock(&self.shared.state).pex_learned
    }

    /// The client-wide connection budget this torrent draws on, for callers
    /// deciding whether to *start* work. Advisory; the claim inside `attach` is
    /// what enforces the ceiling.
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

    /// How many connected peers hold piece `index` — the number rarest-first
    /// steers by, exposed because nothing outside the engine could see it,
    /// which is how a peer inflating it went unnoticed.
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

    /// Disconnect every attached peer: each is removed from the table
    /// (withdrawing its availability, releasing its in-flight blocks) and its
    /// connection closed, so both threads and the descriptor are reclaimed.
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

    /// How many of this torrent's peer threads are still running, two per
    /// attached connection. Exposed because "the peer table is empty" and "the
    /// threads that served it are gone" are different claims, and only the
    /// first was ever checkable.
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
        stream: S,
        remote: DestHash,
    ) -> std::io::Result<()> {
        self.attach_abortable(stream, remote, &|| false)
    }

    /// [`attach`](Torrent::attach), giving up the handshake early — within
    /// [`HANDSHAKE_POLL`] — once `abort` returns true. What the dial sweep
    /// uses, so a pause or shutdown is not held behind a peer that accepted
    /// and then answers a byte at a time.
    ///
    /// # Errors
    ///
    /// As [`attach`](Torrent::attach), plus [`std::io::ErrorKind::Interrupted`]
    /// when aborted.
    pub fn attach_abortable<S: I2pStream + 'static>(
        &self,
        mut stream: S,
        remote: DestHash,
        abort: &dyn Fn() -> bool,
    ) -> std::io::Result<()> {
        if self.is_banned(remote) {
            return Err(banned());
        }
        // One deadline for the whole exchange (see [`HANDSHAKE_TIMEOUT`]).
        // Best-effort: a backend with no timeout of its own ignores it.
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        write_handshake_until(&mut stream, &self.our_handshake().encode(), deadline, abort)?;
        let theirs = read_handshake_until(&mut stream, deadline, abort)?;
        if theirs.info_hash != self.shared.info_hash {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer handshaked a different torrent",
            ));
        }
        // Back to blocking for the connection proper: a peer legitimately sits
        // quiet between messages, and that is the idle timeout's job.
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
        if self.is_banned(remote) {
            return Err(banned());
        }
        // The demux bounded the read of *their* handshake; bound our reply too,
        // so a peer that stops reading cannot hold this thread open.
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        write_handshake_until(
            &mut stream,
            &self.our_handshake().encode(),
            deadline,
            &|| false,
        )?;
        let _ = stream.set_timeouts(None);
        // Counted here, not after `finish_attach`: the claim — that a remote
        // router resolved our leaseSet and carried a handshake both ways — is
        // already true, and a later local failure does not unsettle it.
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
        // The client-wide ceiling, and the authoritative check: the dial sweep
        // and demux read `available()` first, but that read is advisory and two
        // torrents can race it. Only one claim wins.
        let Some(slot) = self.shared.budget.claim() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "no room in the client's peer budget",
            ));
        };
        // Taken before the split, while the whole stream is in hand:
        // `remove_peer` needs a way to end the connection that does not block.
        let closer = Arc::new(stream.closer()?);
        // Split into independent halves, one thread each (Q5 sync model).
        let (reader, writer) = stream.split()?;
        let (tx, rx) = sync_channel::<Message>(OUTGOING_QUEUE);

        // Registration can refuse: [`MAX_CONNECTIONS_PER_DEST`] per
        // destination, or a ban placed since the handshake. Refusing returns
        // the slot and drops both halves.
        let id = self
            .shared
            .register_peer(tx.clone(), closer, remote, slot)
            .map_err(|refusal| match refusal {
                Refusal::Banned => banned(),
                Refusal::DestFull => std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "this destination already holds its share of connections",
                ),
            })?;

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

        // A thread the OS will not give us ends the connection, not the
        // torrent: drop the peer so its slot and entry go back.
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
        // Reap handles of peers already gone; two per connection accumulate
        // otherwise, and a long-lived torrent sees a lot of churn.
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
        // The connection is over — the peer was removed (dropping the sender
        // this loop waits on) or a write failed — so close it both ways. This
        // is what reclaims the *reader*: dropping only this half leaves the
        // descriptor open and parks the reader in a blocking read for good.
        writer.close();
    })
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    shared: Arc<Shared>,
    id: u64,
    mut reader: R,
) -> std::io::Result<JoinHandle<()>> {
    peer_thread().spawn(move || {
        // One frame buffer for the life of the connection.
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

/// The next slice of a handshake exchange: how long the next read or write may
/// block, or why it may not happen at all.
///
/// `abort` is the caller's stop flag. Consulted here, between reads, so a
/// sweep told to stop is out of a half-finished handshake within
/// [`HANDSHAKE_POLL`] rather than at the peer's convenience.
fn handshake_slice(deadline: Instant, abort: &dyn Fn() -> bool) -> std::io::Result<Duration> {
    if abort() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "handshake abandoned: the swarm is stopping",
        ));
    }
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "peer did not complete the handshake in time",
        ));
    }
    Ok(left.min(HANDSHAKE_POLL))
}

/// Whether a read or write ended because its timeout ran out rather than
/// because the connection did — the one outcome worth trying again.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

/// Read a peer's 68-byte handshake by `deadline`, or until `abort` says to
/// stop. The stream's timeout is left set; the caller clears it.
///
/// # Errors
///
/// The deadline passing, the caller aborting, end of stream, a stream error, or
/// bytes that are not a handshake.
pub(crate) fn read_handshake_until<S: I2pStream>(
    stream: &mut S,
    deadline: Instant,
    abort: &dyn Fn() -> bool,
) -> std::io::Result<Handshake> {
    let mut buf = [0u8; wire::HANDSHAKE_LEN];
    let mut filled = 0;
    while filled < buf.len() {
        let slice = handshake_slice(deadline, abort)?;
        let _ = stream.set_timeouts(Some(slice));
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer hung up during the handshake",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Handshake::parse(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write our handshake by `deadline`, or until `abort` says to stop — the
/// same shape as the read, because a peer that accepts and never reads is the
/// same peer with the roles swapped.
///
/// # Errors
///
/// The deadline passing, the caller aborting, or a stream error.
fn write_handshake_until<S: I2pStream>(
    stream: &mut S,
    bytes: &[u8],
    deadline: Instant,
    abort: &dyn Fn() -> bool,
) -> std::io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let slice = handshake_slice(deadline, abort)?;
        let _ = stream.set_timeouts(Some(slice));
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => written += n,
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Stack size for a peer's reader and writer threads.
///
/// Several hundred threads at the default 2 MiB is most of a gigabyte of
/// address space, and neither thread recurses over anything a peer controls —
/// the deepest call is `bencode`'s decoder, capped at
/// [`MAX_DEPTH`](crate::bencode::MAX_DEPTH).
const PEER_STACK_BYTES: usize = 256 * 1024;

/// A builder for the threads that serve one peer connection.
fn peer_thread() -> std::thread::Builder {
    std::thread::Builder::new().stack_size(PEER_STACK_BYTES)
}

/// The error a banned destination is refused with, at every door.
fn banned() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "this destination is banned for the rest of the run",
    )
}

/// Why [`Shared::register_peer`] would not seat a connection.
enum Refusal {
    /// The destination already holds [`MAX_CONNECTIONS_PER_DEST`].
    DestFull,
    /// The destination was banned, possibly while its handshake was in flight.
    Banned,
}

impl Shared {
    /// Add the peer to the table and return its id, or the [`Refusal`]: `dest`
    /// already holds [`MAX_CONNECTIONS_PER_DEST`] connections here, or is
    /// banned.
    ///
    /// The checks live *inside* the lock that pushes the entry, the only place
    /// they can be right: two connections from one destination can be in
    /// `finish_attach` at once, and a ban can land between the handshake and
    /// here. Taking `slot` by value is what returns it on refusal, so the
    /// budget cannot leak a slot to an unregistered connection.
    fn register_peer(
        &self,
        out: SyncSender<Message>,
        closer: Arc<dyn I2pClose + Send + Sync>,
        dest: DestHash,
        slot: PeerSlot,
    ) -> Result<u64, Refusal> {
        let mut st = lock(&self.state);
        if st.banned.contains(&dest) {
            return Err(Refusal::Banned);
        }
        if st.peers.iter().filter(|p| p.dest == dest).count() >= MAX_CONNECTIONS_PER_DEST {
            return Err(Refusal::DestFull);
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
        Ok(id)
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
        // Outside the lock, because it wakes two threads that will want it;
        // idempotent, so the writer closing again on its way out is free.
        // Removing the entry is not what reclaims the connection — this is,
        // since dropping `out` only reaches a writer that is idle.
        if let Some(closer) = closer {
            closer.close();
        }
    }

    /// Handle one message: mutate state under the lock, collect outgoing
    /// messages, then send them after releasing it.
    fn on_message(&self, id: u64, msg: &Message) {
        let mut out: Vec<Outgoing> = Vec::new();
        let evict: Vec<u64>;
        {
            let mut st = lock(&self.state);
            let now = Instant::now();
            // Anything at all, keep-alives included, is proof the peer is
            // still there — which is what the idle timeout measures.
            if let Some(peer) = st.peers.iter_mut().find(|p| p.id == id) {
                peer.last_seen = now;
            }
            self.handle(&mut st, id, msg, &mut out);
            // The maintenance tick normally runs this. One comparison per
            // message keeps the choker honest for an embedder without a tick.
            if st.last_choke_round.elapsed() >= st.choke_interval {
                st.last_choke_round = now;
                run_choker(&mut st, &mut out);
            }
            record_sent(&mut st, &out, now);
            evict = std::mem::take(&mut st.to_drop);
            // Any message can move piece accounting; check while the lock is
            // still held and the state is settled.
            debug_check_state(&st);
        }
        // Outside the lock, as `maintain` does: `remove_peer` takes it.
        for id in evict {
            self.remove_peer(id);
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
            idle.append(&mut st.to_drop);
            debug_check_state(&st);
        }
        // Outside the lock: remove_peer takes it, and so does the send path.
        for id in idle {
            self.remove_peer(id);
        }
        self.send_all(out);
    }

    /// Hand every collected message to its peer's writer, dropping any peer
    /// whose queue will not take it. `try_send`, never `send`: the calling
    /// thread usually belongs to some *other* peer, and blocking behind one
    /// that stopped reading is one silent peer stalling the whole torrent. A
    /// full queue (see [`OUTGOING_QUEUE`]) or a closed one means the connection
    /// is finished either way.
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
            // No-ops: keep-alive, a cancel we serve synchronously so never
            // have queued, and fast-extension messages (BEP 6 off).
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
                // A choking peer drops outstanding requests (no fast
                // extension here); release them for re-picking.
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
                // Only when the bit actually changes: leaving withdraws what
                // the bitfield says exactly once, so a repeated `have` would
                // inflate that piece's availability for good.
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
    /// Nothing stops a peer repeating its bitfield or following it with
    /// have-all/have-none, and adding without subtracting leaks the difference
    /// permanently: announce every piece, then have-none, and the torrent still
    /// believes those copies exist.
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
                // Now their pex id is known, send them the peers we know.
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
        // drops anything over MAX_PEX_PEERS as spam, so an uncapped send would
        // silently stop working for exactly the busy torrents PEX is for.
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
        // Any block at all is proof the peer is still working for us, which is
        // what strikes measure. Cleared even for a late or duplicate block.
        st.peers[idx].strikes = 0;
        // A block for a piece we already hold is an endgame duplicate: another
        // peer answered first and the piece verified. Writing it would put this
        // peer's bytes over verified ones with nothing to re-verify afterwards.
        if !was_requested || st.picker.has(index) {
            // Unsolicited, already satisfied, or late; ignore the payload but
            // still keep the pipeline full below.
        } else if self.storage.write_block(index, begin, block).is_ok() {
            self.downloaded
                .fetch_add(block.len() as u64, std::sync::atomic::Ordering::Relaxed);
            // Remembered by destination, not connection: the verdict may come
            // after this connection is gone, and a ban is on the identity.
            let dest = st.peers[idx].dest;
            st.suppliers.insert((index, block_no), dest);
            if !st.picker.block_received(index, block_no) {
                return;
            }
            // Piece complete: verify from disk before trusting it.
            if let Ok(true) = self.storage.verify_piece(index) {
                st.picker.set_have(index);
                st.credit_suppliers(index);
                for peer in &st.peers {
                    out.push((peer.id, peer.out.clone(), Message::Have(index)));
                }
            } else {
                st.picker.reset_piece(index);
                // Whoever is banned here is queued for removal by the caller,
                // and its blocks go back to the picker then; the refill below
                // may still ask it for more, which is one wasted request
                // rather than a leaked one.
                st.blame_suppliers(index);
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
        // The request must lie inside the piece it names: storage bounds reads
        // against the whole torrent, so a range running off the end of one
        // piece reads into the next.
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
/// keep-alive clock starts again for them. Done once over the batch rather than
/// at each of the dozen `out.push` sites.
fn record_sent(st: &mut State, out: &[Outgoing], now: Instant) {
    for (id, _, _) in out {
        if let Some(peer) = st.peers.iter_mut().find(|p| p.id == *id) {
            peer.last_sent = now;
        }
    }
}

/// Recompute whether we are interested in this peer and send the transition if
/// it changed. State-only, so a free function rather than a method.
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
/// Each expired request goes back to the picker so its piece can finish
/// elsewhere, frees the peer's pipeline slot, and earns a strike — at
/// [`REQUEST_STRIKES`] the slot goes back to the swarm.
///
/// *Every* eligible peer is refilled, not just those that timed out.
/// [`fill_requests`] otherwise runs only when a block arrives or a peer
/// unchokes, so a peer whose pipeline drains while the picker has nothing to
/// offer stays connected, interested, unchoked and permanently idle, with no
/// event left that would wake it — a second way to stall, and one a single slow
/// peer is enough to cause.
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
                // Removed by the caller outside the lock, which returns
                // whatever it still holds to the picker. No point topping up.
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

    // In endgame the picker deliberately hands one block to several peers, but
    // it has no per-peer view, so it can also hand a block back to the peer
    // that already owes it — a wasted request, and a count never settled since
    // the peer answers once. Drop those and give the count straight back.
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
/// End to end — our handshake, theirs, and the assembly — because the half
/// before them is the cheaper half for a peer to stall in.
const METADATA_DEADLINE: Duration = Duration::from_secs(120);

/// Per-read/write socket bound for a metadata stream. [`METADATA_DEADLINE`] is
/// only consulted between frames, so a peer that accepts the stream and then
/// says nothing is stopped by this and nothing else.
const METADATA_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Frames a peer may spend before sending its extension handshake. Generous,
/// because a peer legitimately opens with a bitfield and a have or two; finite,
/// because the loop that waits ignores whatever else arrives.
const METADATA_GREETING_FRAMES: u32 = 64;

/// Frames of slack per metadata piece, over the one useful reply each.
///
/// Nothing else can end the exchange: a piece we already hold, or one of the
/// wrong length, makes no progress and costs the peer nothing, so "read until
/// complete" is a loop it can keep us in indefinitely. A read timeout does not
/// catch it either — answering promptly and uselessly never trips one.
const METADATA_FRAME_SLACK: u32 = 8;

/// Read frames until the peer's extension handshake arrives, and take the
/// `ut_metadata` id and metadata size out of it.
///
/// Bounded by frames *and* a deadline, which catch different peers: one
/// flooding cheap frames, one sending a few very slowly.
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
/// (`ut_metadata`) — the magnet bootstrap. Blocking and sequential, so it runs
/// on the dialing thread before the full peer connection exists.
///
/// The reassembled bytes are checked against `info_hash` inside the assembler,
/// so a peer cannot serve a different torrent. Bounded end to end by
/// `METADATA_DEADLINE` (120s), with `METADATA_IO_TIMEOUT` (30s) underneath it.
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

    // One clock for the whole exchange, started before the first byte: the
    // fetch walks candidates one at a time, so a single peer that accepts and
    // then stops is enough to keep a magnet from ever resolving.
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

    /// A real single-file torrent via bencode+parse, so `raw_info` holds the
    /// genuine info-dict bytes (needed to serve BEP 9 metadata).
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

    /// One piece per file, so "which file was skipped" is readable off the wire.
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
        // hanging it — the absence is what this asserts on.
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
    /// for a high-priority one first. Asserted on the wire, because everything
    /// upstream was already right and the one thing nothing did was tell the
    /// engine — so a skipped file downloaded in full.
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
        // Skip the middle file, raise the last: the untouched sequential order
        // would be 0, 1, 2, so both effects are visible.
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
        // Finished once the two wanted pieces land, rather than waiting for a
        // piece it will never ask for.
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

    /// Two instances negotiate BEP 10 and one learns a third peer via `i2p_pex`.
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

        // The counter must record only what PEX taught us: B also knows A from
        // the connection, and A knows B and X without either arriving over PEX.
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

    /// A peer answering the metadata request with an endless stream of pieces
    /// it knows we cannot use: a piece of the wrong length never advances the
    /// assembly, so nothing but the fetch's own bound can end the exchange.
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
            // Mirror the BT handshake, then advertise a size needing three
            // pieces.
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
            // Answer forever with a piece of the wrong length, which the
            // assembler refuses and which never completes anything.
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

    /// The same refusal one stage earlier: a peer that completes the
    /// `BitTorrent` handshake and then never sends an *extension* handshake,
    /// filling the wait with individually ordinary frames. The waiting loop
    /// ignores anything else by design, so a peer need only keep talking.
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
            // Never an extension handshake. Just `have`s: valid, cheap, and
            // nothing the waiting loop reacts to.
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

    /// A seeder and a leecher complete a multi-piece, multi-file download over
    /// the mock network.
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
        // Piece by piece: file and piece boundaries differ, so storage maps
        // each piece's bytes to the right file spans.
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

    /// The same seeder/leecher exchange as the mock test, but over a **real**
    /// local router via the SAM backend. Router-gated, so `#[ignore]`d:
    ///
    /// ```text
    /// CLOVE_SAM_PORT=7656 cargo test -p clove-core -- --ignored --nocapture
    /// ```
    ///
    /// Both destinations live on the one router, the harder topology: it must
    /// resolve a leaseSet it published seconds ago, so a failure here is as
    /// likely to be the router as clove.
    #[test]
    #[ignore = "needs a live I2P router; set CLOVE_SAM_PORT and run with --ignored"]
    fn two_instances_download_over_sam() {
        use i2pnet::sam::{SamConfig, SamListener, SamSession};

        let port: u16 = std::env::var("CLOVE_SAM_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .expect("set CLOVE_SAM_PORT (e.g. 7656) and run with --ignored");

        // ~5 pieces, last one short. Single file keeps this on the transport;
        // the mock test covers multi-file piece mapping.
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
            // Skip a connection that arrived without its destination header
            // rather than treating it as the end.
            let (stream, from) = loop {
                if let Some(pair) = seed_listener.accept().expect("seeder accept") {
                    break pair;
                }
            };
            seeder_bg.attach(stream, from).expect("seeder attach");
        });

        // A fresh transient destination needs time to build tunnels and publish
        // its leaseSet, so an immediate dial gets `CantReachPeer`. A failed
        // connect establishes nothing, so the seeder's single accept still pairs
        // with the one dial that succeeds.
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
