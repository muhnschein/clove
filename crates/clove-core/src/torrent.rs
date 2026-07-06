//! The torrent coordinator: peer connections wired to the picker, choker,
//! and storage — the Q5 sync thread-per-peer model in practice.
//!
//! Each connection runs two threads over one `i2pnet` stream (hence
//! [`i2pnet::I2pStream::try_clone`]): a reader that blocks on incoming
//! messages and a writer that drains a bounded outgoing queue. Shared
//! torrent state (picker, choker, the peer table) lives behind one mutex;
//! handlers compute their outgoing messages while holding it, then release
//! it *before* sending so a slow writer can never stall the whole torrent.
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
//! This is the engine core for the M2 lab demo (download between two
//! instances). The real router backend (Phase D) and swarm features
//! (trackers, PEX, magnets — Phase E) attach at the same seams.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use i2pnet::{DestHash, I2pStream};

use crate::bitfield::{self, Bitfield};
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

/// Outgoing message queue depth per peer before the writer applies
/// backpressure. Bounded — no unbounded channels in the engine (SCOPE §4).
const OUTGOING_QUEUE: usize = 256;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running torrent: owns the shared state and the peer threads.
pub struct Torrent {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct Shared {
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    storage: Arc<Storage>,
    num_pieces: u32,
    max_frame: u32,
    /// Raw `info` dictionary bytes, for serving BEP 9 metadata to magnet
    /// peers. Empty if unknown (a synthetic test torrent).
    raw_info: Vec<u8>,
    state: Mutex<State>,
    done: Mutex<bool>,
    done_cv: Condvar,
}

struct State {
    picker: Picker,
    choker: Choker,
    peers: Vec<Peer>,
    next_id: u64,
    /// Peer destinations we know about (from connections, PEX, or the
    /// tracker), for peer exchange and future dialing.
    known_peers: HashSet<DestHash>,
}

// The four flags are the canonical BEP 3 per-connection choke/interest
// state; modelling them as anything other than four booleans would obscure
// the protocol, so the excessive-bools and field-name lints are waived here.
#[allow(clippy::struct_excessive_bools, clippy::struct_field_names)]
struct Peer {
    id: u64,
    /// The peer's I2P destination, for peer exchange.
    dest: DestHash,
    out: SyncSender<Message>,
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
    /// Blocks we have requested from them, as (piece, block).
    in_flight: HashSet<(u32, u32)>,
    /// Bytes served to them, the choker's ranking signal.
    uploaded: u64,
    /// The message id the peer listens on for `i2p_pex`, once it handshakes.
    pex_id: Option<u8>,
    /// The message id the peer listens on for `ut_metadata`.
    metadata_id: Option<u8>,
}

/// A message queued to a specific peer, collected under the lock and sent
/// after it is released.
type Outgoing = (SyncSender<Message>, Message);

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
            storage,
            num_pieces,
            max_frame,
            raw_info: meta.raw_info.clone(),
            state: Mutex::new(State {
                picker,
                choker: Choker::default(),
                peers: Vec::new(),
                next_id: 0,
                known_peers: HashSet::new(),
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
    pub fn add_peers(&self, peers: &[DestHash]) {
        let mut st = lock(&self.shared.state);
        for &p in peers {
            st.known_peers.insert(p);
        }
    }

    /// The peer destinations this torrent currently knows about.
    #[must_use]
    pub fn known_peers(&self) -> Vec<DestHash> {
        lock(&self.shared.state)
            .known_peers
            .iter()
            .copied()
            .collect()
    }

    /// Perform the handshake on `stream` (with `remote` the peer's known
    /// destination) and, on success, register the peer and spawn its
    /// reader/writer threads. Used for both dialed and accepted connections.
    ///
    /// # Errors
    ///
    /// Handshake I/O failure or an info-hash mismatch (wrong torrent).
    pub fn attach<S: I2pStream + 'static>(
        &self,
        mut stream: S,
        remote: DestHash,
    ) -> std::io::Result<()> {
        let ours = Handshake {
            info_hash: self.shared.info_hash,
            peer_id: self.shared.peer_id,
            // Advertise the BEP 10 extension protocol (i2p_pex, ut_metadata).
            // Fast (BEP 6) stays off until its semantics are wired.
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
        if theirs.info_hash != self.shared.info_hash {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer handshaked a different torrent",
            ));
        }

        // Handshake done duplex; now split into independent halves so the
        // reader and writer run on separate threads (Q5 sync model).
        let (reader, writer) = stream.split()?;
        let (tx, rx) = sync_channel::<Message>(OUTGOING_QUEUE);

        let id = self.shared.register_peer(tx.clone(), remote);

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

        let writer_handle = spawn_writer(writer, rx);
        let reader_handle = spawn_reader(Arc::clone(&self.shared), id, reader);
        let mut threads = lock(&self.threads);
        threads.push(writer_handle);
        threads.push(reader_handle);
        Ok(())
    }
}

fn spawn_writer<W: std::io::Write + Send + 'static>(
    mut writer: W,
    rx: Receiver<Message>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            if wire::write_message(&mut writer, &msg).is_err() {
                break;
            }
        }
    })
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    shared: Arc<Shared>,
    id: u64,
    mut reader: R,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(body) = wire::read_frame(&mut reader, shared.max_frame) {
            match Message::parse(&body) {
                Ok(msg) => shared.on_message(id, &msg),
                Err(_) => break, // protocol violation: drop the peer
            }
        }
        shared.remove_peer(id);
    })
}

impl Shared {
    fn register_peer(&self, out: SyncSender<Message>, dest: DestHash) -> u64 {
        let mut st = lock(&self.state);
        let id = st.next_id;
        st.next_id += 1;
        st.known_peers.insert(dest);
        st.peers.push(Peer {
            id,
            dest,
            out,
            has: Bitfield::empty(self.num_pieces),
            peer_choking: true,
            we_choke: true,
            they_interested: false,
            we_interested: false,
            in_flight: HashSet::new(),
            uploaded: 0,
            pex_id: None,
            metadata_id: None,
        });
        id
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
        let mut st = lock(&self.state);
        if let Some(pos) = st.peers.iter().position(|p| p.id == id) {
            let peer = st.peers.swap_remove(pos);
            st.picker.remove_bitfield(&peer.has);
            for (piece, block) in peer.in_flight {
                st.picker.block_failed(piece, block);
            }
        }
    }

    /// Handle one message: mutate state under the lock, collect outgoing
    /// messages, then send them after releasing it.
    fn on_message(&self, id: u64, msg: &Message) {
        let mut out: Vec<Outgoing> = Vec::new();
        {
            let mut st = lock(&self.state);
            self.handle(&mut st, id, msg, &mut out);
        }
        for (tx, msg) in out {
            let _ = tx.send(msg);
        }
        self.check_done();
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
                let dropped: Vec<(u32, u32)> = peer.in_flight.drain().collect();
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
                if *piece < self.num_pieces {
                    st.peers[idx].has.set(*piece);
                    st.picker.add_single(*piece);
                    update_interest(st, idx, out);
                }
            }
            Message::Bitfield(bytes) => {
                if let Ok(field) = Bitfield::from_bytes(bytes, self.num_pieces) {
                    st.picker.add_bitfield(&field);
                    st.peers[idx].has = field;
                    update_interest(st, idx, out);
                }
            }
            Message::HaveAll => {
                let full = Bitfield::full(self.num_pieces);
                st.picker.add_bitfield(&full);
                st.peers[idx].has = full;
                update_interest(st, idx, out);
            }
            Message::HaveNone => {
                st.peers[idx].has = Bitfield::empty(self.num_pieces);
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
                        st.known_peers.insert(dest);
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
        let added: Vec<DestHash> = st
            .known_peers
            .iter()
            .copied()
            .filter(|&d| d != peer_dest)
            .collect();
        let msg = PexMessage {
            added,
            dropped: Vec::new(),
        };
        if msg.is_empty() {
            return;
        }
        out.push((
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
        let was_requested = st.peers[idx].in_flight.remove(&(index, block_no));
        if !was_requested {
            // Unsolicited or already-satisfied block; ignore the payload but
            // still try to keep the pipeline full below.
        } else if self.storage.write_block(index, begin, block).is_ok()
            && st.picker.block_received(index, block_no)
        {
            // Piece complete: verify from disk before trusting it.
            match self.storage.verify_piece(index) {
                Ok(true) => {
                    st.picker.set_have(index);
                    for peer in &st.peers {
                        out.push((peer.out.clone(), Message::Have(index)));
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
        if st.peers[idx].we_choke || req.length > BLOCK_LEN || !st.picker.has(req.index) {
            return;
        }
        if let Ok(data) = self.storage.read_block(req.index, req.begin, req.length) {
            let peer = &mut st.peers[idx];
            peer.uploaded += data.len() as u64;
            out.push((
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

/// Recompute whether we are interested in this peer and send the transition
/// if it changed. Called after learning what they have. State-only, so a
/// free function rather than a method.
fn update_interest(st: &mut State, idx: usize, out: &mut Vec<Outgoing>) {
    let peer = &st.peers[idx];
    let want = peer.has.iter_present().any(|p| !st.picker.has(p));
    if want && !peer.we_interested {
        st.peers[idx].we_interested = true;
        out.push((st.peers[idx].out.clone(), Message::Interested));
        // If they are already unchoking us we can request right away.
        if !st.peers[idx].peer_choking {
            fill_requests(st, idx, out);
        }
    } else if !want && peer.we_interested {
        st.peers[idx].we_interested = false;
        out.push((st.peers[idx].out.clone(), Message::NotInterested));
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
    let peer = &mut st.peers[idx];
    for req in requests {
        peer.in_flight.insert((req.index, req.begin / BLOCK_LEN));
        out.push((peer.out.clone(), Message::Request(req)));
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
            out.push((peer.out.clone(), Message::Unchoke));
        }
    }
    for id in decision.choke {
        if let Some(peer) = st.peers.iter_mut().find(|p| p.id == id) {
            peer.we_choke = true;
            out.push((peer.out.clone(), Message::Choke));
        }
    }
}

/// Frame ceiling for the metadata-fetch handshake flow: a `ut_metadata` data
/// message is one 16 KiB piece plus small header/extension overhead.
const METADATA_FRAME: u32 = 16 * 1024 + 256; // METADATA_PIECE_LEN + overhead

/// Fetch and verify a torrent's `info` dictionary from one peer over BEP 9
/// (`ut_metadata`) — the magnet bootstrap. Blocking and sequential, so it
/// runs on the dialing thread against a duplex stream before the full peer
/// connection (and storage/picker) exist.
///
/// The reassembled bytes are checked against `info_hash` inside the
/// assembler, so a peer cannot serve a different torrent.
///
/// # Errors
///
/// Handshake failure, a peer that does not offer metadata, a rejected
/// piece, verification failure, or any I/O error.
pub fn fetch_metadata<S: I2pStream>(
    mut stream: S,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
) -> std::io::Result<MetaInfo> {
    let invalid = |m: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);

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

    // Wait for the peer's handshake, which carries its ut_metadata id and the
    // total metadata size.
    let (their_meta_id, total_size) = loop {
        let body = wire::read_frame(&mut stream, METADATA_FRAME)?;
        if let Ok(Message::Extended { id: 0, payload }) = Message::parse(&body) {
            let hs =
                extension::Handshake::parse(&payload).map_err(|_| invalid("bad ext handshake"))?;
            match (hs.id_for(UT_METADATA), hs.metadata_size) {
                (Some(mid), Some(size)) => break (mid, size),
                _ => return Err(invalid("peer does not serve metadata")),
            }
        }
        // Ignore anything else (bitfield, etc.) until the handshake arrives.
    };

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
    while !asm.is_complete() {
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
            pieces,
            files,
            total_length: total,
            private: true,
            trackers: vec![],
            skipped_trackers: 0,
            raw_info: Vec::new(),
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

    /// The M2 demo: a seeder and a leecher over the mock network complete a
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
}
