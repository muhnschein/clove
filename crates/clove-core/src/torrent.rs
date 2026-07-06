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

use i2pnet::I2pStream;

use crate::bitfield::{self, Bitfield};
use crate::choker::{Choker, PeerSnapshot};
use crate::metainfo::MetaInfo;
use crate::picker::{Mode, Picker};
use crate::storage::Storage;
use crate::wire::{self, BLOCK_LEN, BlockRequest, Extensions, Handshake, Message};

/// Outstanding block requests a peer may have in flight before we wait for
/// data. Config-tunable later (R5).
pub const PIPELINE_DEPTH: usize = 16;

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
    state: Mutex<State>,
    done: Mutex<bool>,
    done_cv: Condvar,
}

struct State {
    picker: Picker,
    choker: Choker,
    peers: Vec<Peer>,
    next_id: u64,
}

// The four flags are the canonical BEP 3 per-connection choke/interest
// state; modelling them as anything other than four booleans would obscure
// the protocol, so the excessive-bools and field-name lints are waived here.
#[allow(clippy::struct_excessive_bools, clippy::struct_field_names)]
struct Peer {
    id: u64,
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
        let max_frame = u32::try_from(bitfield::byte_len(num_pieces))
            .unwrap_or(u32::MAX)
            .max(BLOCK_LEN + 13)
            .saturating_add(16);
        let shared = Arc::new(Shared {
            info_hash: meta.info_hash.0,
            peer_id,
            storage,
            num_pieces,
            max_frame,
            state: Mutex::new(State {
                picker,
                choker: Choker::default(),
                peers: Vec::new(),
                next_id: 0,
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

    /// Perform the handshake on `stream` and, on success, register the peer
    /// and spawn its reader/writer threads. Used for both dialed and
    /// accepted connections.
    ///
    /// # Errors
    ///
    /// Handshake I/O failure or an info-hash mismatch (wrong torrent).
    pub fn attach<S: I2pStream + 'static>(&self, mut stream: S) -> std::io::Result<()> {
        let ours = Handshake {
            info_hash: self.shared.info_hash,
            peer_id: self.shared.peer_id,
            // Plain BEP 3 for the lab demo; the codec supports fast/extended
            // and Phase E turns them on with their semantics.
            extensions: Extensions::default(),
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

        let writer = stream.try_clone()?;
        let (tx, rx) = sync_channel::<Message>(OUTGOING_QUEUE);

        let id = self.shared.register_peer(tx.clone());

        // Announce our piece set immediately (empty bitfield is legal).
        let bitfield = {
            let st = lock(&self.shared.state);
            Message::Bitfield(st.picker.have_field().as_bytes().to_vec())
        };
        let _ = tx.try_send(bitfield);

        let writer_handle = spawn_writer(writer, rx);
        let reader_handle = spawn_reader(Arc::clone(&self.shared), id, stream);
        let mut threads = lock(&self.threads);
        threads.push(writer_handle);
        threads.push(reader_handle);
        Ok(())
    }
}

fn spawn_writer<S: I2pStream + 'static>(mut writer: S, rx: Receiver<Message>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            if wire::write_message(&mut writer, &msg).is_err() {
                break;
            }
        }
    })
}

fn spawn_reader<S: I2pStream + 'static>(
    shared: Arc<Shared>,
    id: u64,
    mut reader: S,
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
    fn register_peer(&self, out: SyncSender<Message>) -> u64 {
        let mut st = lock(&self.state);
        let id = st.next_id;
        st.next_id += 1;
        st.peers.push(Peer {
            id,
            out,
            has: Bitfield::empty(self.num_pieces),
            peer_choking: true,
            we_choke: true,
            they_interested: false,
            we_interested: false,
            in_flight: HashSet::new(),
            uploaded: 0,
        });
        id
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
            // never have queued, and fast/extended messages negotiated off
            // this phase.
            Message::KeepAlive
            | Message::Cancel(_)
            | Message::RejectRequest(_)
            | Message::SuggestPiece(_)
            | Message::AllowedFast(_)
            | Message::Extended { .. } => {}
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
        }
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

        let seeder_bg = Arc::clone(&seeder);
        let accept_thread = std::thread::spawn(move || {
            let (stream, _from) = seed_ep.accept().unwrap();
            seeder_bg.attach(stream).unwrap();
        });

        let stream = leech_ep.dial(seed_dest, Duration::from_secs(5)).unwrap();
        leecher.attach(stream).unwrap();
        accept_thread.join().unwrap();

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
