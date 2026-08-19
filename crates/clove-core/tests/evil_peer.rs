//! Evil-peer behaviour suite (`docs/SCOPE.md` §9).
//!
//! `tests/hostile.rs` proves the *parsers* survive bytes an attacker chose.
//! This proves the *engine* does: a peer that completes a valid handshake and
//! then misbehaves — lies about what it has, answers requests with rubbish,
//! sends messages nobody asked for, floods PEX, or simply goes silent while
//! holding a connection open.
//!
//! The contract under test is not "the engine notices and punishes." It is the
//! weaker, more important one:
//!
//! 1. No panic, no hang, no unbounded allocation.
//! 2. Bad data never becomes good data — a piece that fails SHA-1 is never
//!    counted as had, however insistently it is offered.
//! 3. A misbehaving peer cannot deny service to an honest one. Every hostile
//!    scenario ends by putting an honest peer through a real download against
//!    the same torrent instance and requiring it to finish.
//!
//! These run in debug, so the invariant assertions over the piece accounting,
//! the choke scheduler and the peer table (`cfg(debug_assertions)`) are live
//! throughout — a hostile peer that corrupted internal state would trip them
//! even where the test makes no explicit claim.

// The fixtures and raw-peer helpers below sit outside `#[test]` functions,
// where clippy's `allow-expect-in-tests` does not reach. Every `expect` here
// names the invariant it asserts, which is the discipline the lint exists to
// enforce; a fixture that cannot be built is a broken test, not a runtime
// error to be handled.
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clove_core::bencode::{self, Value as Ben};
use clove_core::bitfield::Bitfield;
use clove_core::extension;
use clove_core::metainfo::MetaInfo;
use clove_core::picker::Mode;
use clove_core::storage::Storage;
use clove_core::torrent::Torrent;
use clove_core::wire::{self, BLOCK_LEN, Handshake, Message};
use i2pnet::mock::{MockNet, MockStream};
use i2pnet::{DestHash, I2pDialer};
use sha1::{Digest, Sha1};

/// How long an "it must finish" assertion waits before calling it a hang.
const DEADLINE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------- fixtures

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("clove-evil-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("temp dir");
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Four full pieces plus a short last one, so piece and block boundaries are
/// both exercised.
fn content() -> Vec<u8> {
    (0..(4 * BLOCK_LEN + 500))
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect()
}

fn meta_for(content: &[u8]) -> MetaInfo {
    meta_with_private(content, false)
}

/// [`meta_for`], with BEP 27's `info.private` set as asked.
fn meta_with_private(content: &[u8], private: bool) -> MetaInfo {
    let pieces: Vec<u8> = content
        .chunks(BLOCK_LEN as usize)
        .flat_map(|c| <[u8; 20]>::from(Sha1::digest(c)))
        .collect();
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), Ben::Bytes(b"evil".to_vec()));
    info.insert(b"piece length".to_vec(), Ben::Int(i64::from(BLOCK_LEN)));
    info.insert(b"pieces".to_vec(), Ben::Bytes(pieces));
    info.insert(
        b"length".to_vec(),
        Ben::Int(i64::try_from(content.len()).expect("fixture fits in i64")),
    );
    if private {
        info.insert(b"private".to_vec(), Ben::Int(1));
    }
    let mut root = BTreeMap::new();
    root.insert(b"info".to_vec(), Ben::Dict(info));
    MetaInfo::parse(&bencode::encode(&Ben::Dict(root))).expect("fixture parses")
}

/// A torrent holding every piece, with its files already written to disk.
fn seeding_torrent(meta: &MetaInfo, content: &[u8], dir: &TempDir) -> Arc<Torrent> {
    let storage = Arc::new(Storage::create(meta, &dir.0, false).expect("storage"));
    for p in 0..storage.num_pieces() {
        let start = p as usize * BLOCK_LEN as usize;
        let end = (start + storage.piece_len(p) as usize).min(content.len());
        storage
            .write_block(p, 0, &content[start..end])
            .expect("seed write");
    }
    let have = storage.verify_all().expect("verify seed");
    assert_eq!(have.count(), have.len(), "fixture seeder must be complete");
    Torrent::new(
        meta,
        storage,
        &have,
        Mode::RarestFirst,
        *b"-CV0001-seedseedseed",
    )
}

/// A torrent that holds the first `pieces` pieces and still wants the rest —
/// the state a real leecher spends its life in, and the only one in which it
/// both serves peers and downloads from them at the same time.
fn partly_seeded_torrent(
    meta: &MetaInfo,
    content: &[u8],
    dir: &TempDir,
    pieces: u32,
) -> Arc<Torrent> {
    let storage = Arc::new(Storage::create(meta, &dir.0, false).expect("storage"));
    for p in 0..pieces {
        let start = p as usize * BLOCK_LEN as usize;
        let end = (start + storage.piece_len(p) as usize).min(content.len());
        storage
            .write_block(p, 0, &content[start..end])
            .expect("partial write");
    }
    let have = storage.verify_all().expect("verify partial");
    assert_eq!(have.count(), pieces, "fixture should hold {pieces} pieces");
    Torrent::new(
        meta,
        storage,
        &have,
        Mode::RarestFirst,
        *b"-CV0001-halfhalfhalf",
    )
}

/// An empty torrent that will try to download `meta`.
fn leeching_torrent(meta: &MetaInfo, dir: &TempDir) -> Arc<Torrent> {
    let storage = Arc::new(Storage::create(meta, &dir.0, false).expect("storage"));
    let empty = Bitfield::empty(u32::try_from(meta.pieces.len()).expect("piece count"));
    Torrent::new(
        meta,
        storage,
        &empty,
        Mode::RarestFirst,
        *b"-CV0001-leechleeches",
    )
}

// ------------------------------------------------------------ raw peer I/O

/// Complete a handshake as a raw peer would: write ours, read theirs.
///
/// `info_hash` is a parameter rather than taken from the torrent so a test can
/// deliberately hand over the wrong one.
fn handshake(stream: &mut MockStream, info_hash: [u8; 20]) -> std::io::Result<Handshake> {
    let ours = Handshake {
        info_hash,
        peer_id: *b"-XX0000-evilevilevil",
        extensions: wire::Extensions {
            extended: true,
            fast: false,
        },
    };
    stream.write_all(&ours.encode())?;
    let mut buf = [0u8; wire::HANDSHAKE_LEN];
    stream.read_exact(&mut buf)?;
    Handshake::parse(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Run `attack` as a peer dialing `dest`, ignoring any I/O error it ends with:
/// the engine hanging up on a misbehaving peer is a pass, not a failure, and
/// which side notices first is a race we do not want to assert on.
fn attack<F>(net: &MockNet, dest: DestHash, info_hash: [u8; 20], attack: F)
where
    F: FnOnce(&mut MockStream) -> std::io::Result<()>,
{
    let ep = net.endpoint();
    let Ok(mut stream) = ep.dial(dest, Duration::from_secs(5)) else {
        return;
    };
    if handshake(&mut stream, info_hash).is_err() {
        return;
    }
    let _ = attack(&mut stream);
}

/// How long a raw peer waits for a frame before deciding none is coming.
///
/// Every read below goes through this. Without a bound, a message the engine
/// *fails* to send is a test that hangs rather than one that fails — and a hang
/// looks exactly like slow progress, in CI most of all. Generous enough that a
/// loaded machine does not produce a false negative.
const FRAME_WAIT: Duration = Duration::from_secs(2);

/// The next message from a peer, or `None` if nothing arrived in [`FRAME_WAIT`].
///
/// The stream must have a read timeout set (see [`raw_peer`]). A timeout partway
/// through a frame would desynchronise the stream, but the mock delivers a
/// written frame in one piece, so a timeout here always lands on a boundary.
fn next_message(stream: &mut MockStream) -> Option<Message> {
    match wire::read_frame(stream, wire::MAX_MESSAGE_LEN) {
        Ok(frame) => Message::parse(&frame).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(_) => None,
    }
}

/// Dial `dest`, handshake, and return a stream whose reads are bounded.
fn raw_peer(net: &MockNet, dest: DestHash, info_hash: [u8; 20]) -> MockStream {
    let ep = net.endpoint();
    let mut stream = ep.dial(dest, Duration::from_secs(5)).expect("peer dial");
    handshake(&mut stream, info_hash).expect("peer handshake");
    stream.set_timeouts(Some(FRAME_WAIT));
    stream
}

/// Read frames until `want` of them are `Request`s, or give up.
///
/// Anything else the engine sends (its bitfield, extension handshake,
/// interest, choke changes) is skipped: this is about what it *asks* for.
fn read_requests(stream: &mut MockStream, want: usize) -> Vec<wire::BlockRequest> {
    let deadline = Instant::now() + DEADLINE;
    let mut out = Vec::with_capacity(want);
    while out.len() < want {
        assert!(
            Instant::now() < deadline,
            "only {} of {want} requests arrived",
            out.len()
        );
        if let Some(Message::Request(req)) = next_message(stream) {
            out.push(req);
        }
    }
    out
}

/// Announce every piece and unchoke, so the engine starts requesting from us.
fn claim_everything(stream: &mut MockStream, num_pieces: u32) -> std::io::Result<()> {
    let mut full = vec![0u8; (num_pieces as usize).div_ceil(8)];
    for p in 0..num_pieces as usize {
        full[p / 8] |= 0x80 >> (p % 8);
    }
    wire::write_message(stream, &Message::Bitfield(full))?;
    wire::write_message(stream, &Message::Unchoke)
}

/// Accept connections for `torrent` until the returned handle is dropped.
///
/// Each accepted stream gets the full [`Torrent::attach`] treatment, so a peer
/// that handshakes the wrong torrent is refused exactly as it would be in the
/// daemon.
fn spawn_acceptor(
    torrent: &Arc<Torrent>,
    ep: i2pnet::mock::Endpoint,
) -> std::thread::JoinHandle<()> {
    let torrent = Arc::clone(torrent);
    std::thread::spawn(move || {
        while let Ok((stream, from)) = ep.accept() {
            let torrent = Arc::clone(&torrent);
            // Attaching blocks on a handshake exchange; a peer that never
            // sends one must not stall the accept loop behind it.
            std::thread::spawn(move || {
                let _ = torrent.attach(stream, from);
            });
        }
    })
}

/// Drive an honest download of `meta` from `seeder_dest` and require it to
/// complete. This is the "still serves honest peers" half of every case below.
fn honest_download_completes(net: &MockNet, meta: &MetaInfo, seeder_dest: DestHash, tag: &str) {
    let dir = TempDir::new(tag);
    let leecher = leeching_torrent(meta, &dir);
    let ep = net.endpoint();
    let stream = ep
        .dial(seeder_dest, Duration::from_secs(5))
        .expect("dial seeder");
    leecher
        .attach(stream, seeder_dest)
        .expect("attach to seeder");

    let deadline = Instant::now() + DEADLINE;
    while leecher.have().count() < leecher.have().len() {
        assert!(
            Instant::now() < deadline,
            "{tag}: honest download stalled at {}/{} pieces",
            leecher.have().count(),
            leecher.have().len()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    leecher.disconnect_all();
}

// ------------------------------------------------------------------- tests

/// A seeder is subjected to every hostile pattern we can name, then asked to
/// serve an honest peer. Each attack shares the *same* torrent instance, so
/// damage from one would show up in all the later ones.
#[test]
fn a_seeder_survives_hostile_peers_and_still_serves() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;
    let dir = TempDir::new("seeder");
    let seeder = seeding_torrent(&meta, &content, &dir);

    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    // A peer that handshakes a different torrent entirely. The daemon routes
    // by info-hash, so this is a mis-routed or malicious connection.
    attack(&net, dest, [0xAB; 20], |_| Ok(()));

    // Valid handshake, then bytes that are not a message stream.
    attack(&net, dest, info_hash, |s| s.write_all(&[0xFF; 512]));

    // A frame header claiming a gigabyte. The reader must refuse on the
    // length alone rather than trying to collect it.
    attack(&net, dest, info_hash, |s| {
        s.write_all(&u32::MAX.to_be_bytes())
    });

    // Claims to have a piece index far past the end of the torrent.
    attack(&net, dest, info_hash, |s| {
        wire::write_message(s, &Message::Have(u32::MAX))
    });

    // A bitfield of the wrong length for this torrent, in both directions.
    attack(&net, dest, info_hash, |s| {
        wire::write_message(s, &Message::Bitfield(vec![0xFF; 4096]))
    });
    attack(&net, dest, info_hash, |s| {
        wire::write_message(s, &Message::Bitfield(Vec::new()))
    });

    // Block data nobody requested, for a piece that does not exist.
    attack(&net, dest, info_hash, |s| {
        wire::write_message(
            s,
            &Message::Piece {
                index: 9999,
                begin: 0,
                block: vec![0u8; 64],
            },
        )
    });

    // Requests for pieces off the end, at a nonsense offset, and of a length
    // no sane peer would ask for.
    attack(&net, dest, info_hash, |s| {
        wire::write_message(
            s,
            &Message::Request(wire::BlockRequest {
                index: u32::MAX,
                begin: u32::MAX,
                length: u32::MAX,
            }),
        )
    });

    // PEX spam: a single message carrying thousands of destinations.
    attack(&net, dest, info_hash, |s| {
        let mut handshake = BTreeMap::new();
        let mut m = BTreeMap::new();
        m.insert(b"i2p_pex".to_vec(), Ben::Int(1));
        handshake.insert(b"m".to_vec(), Ben::Dict(m));
        wire::write_message(
            s,
            &Message::Extended {
                id: 0,
                payload: bencode::encode(&Ben::Dict(handshake)),
            },
        )?;
        let mut pex = BTreeMap::new();
        pex.insert(b"added".to_vec(), Ben::Bytes(vec![0x5A; 32 * 5000]));
        wire::write_message(
            s,
            &Message::Extended {
                id: 1,
                payload: bencode::encode(&Ben::Dict(pex)),
            },
        )
    });

    // An extended handshake that is not even bencode.
    attack(&net, dest, info_hash, |s| {
        wire::write_message(
            s,
            &Message::Extended {
                id: 0,
                payload: vec![0xFF; 200],
            },
        )
    });

    // Slow-loris: handshake, say nothing, and hold the connection open. Kept
    // alive across the honest download below by holding the stream.
    let loris_ep = net.endpoint();
    let mut loris = loris_ep
        .dial(dest, Duration::from_secs(5))
        .expect("loris dial");
    handshake(&mut loris, info_hash).expect("loris handshake");

    // The point of all of it: an honest peer still gets the whole file.
    honest_download_completes(&net, &meta, dest, "honest-after-attacks");

    drop(loris);
    assert_eq!(
        seeder.have().count(),
        seeder.have().len(),
        "the seeder's own piece set changed under attack"
    );
}

/// A peer that answers every request with correctly-shaped garbage. The
/// leecher must never mark a piece as had, and must not spin, panic, or trip
/// an accounting assertion while being lied to.
#[test]
fn corrupt_blocks_never_become_verified_pieces() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;
    let num_pieces = u32::try_from(meta.pieces.len()).expect("piece count");

    let liar_ep = net.endpoint();
    let liar_dest = liar_ep.dest();
    let liar = std::thread::spawn(move || {
        let Ok((mut stream, _from)) = liar_ep.accept() else {
            return;
        };
        // The leecher dials, so it writes its handshake first; mirror it.
        let mut buf = [0u8; wire::HANDSHAKE_LEN];
        if stream.read_exact(&mut buf).is_err() {
            return;
        }
        let ours = Handshake {
            info_hash,
            peer_id: *b"-XX0000-liarliarliar",
            extensions: wire::Extensions::default(),
        };
        if stream.write_all(&ours.encode()).is_err() {
            return;
        }
        // Claim everything, unchoke, then answer requests with rubbish of
        // exactly the right length — the shape is valid, only the bytes lie.
        // Trailing spare bits must be zero or the engine rejects the bitfield
        // outright, and the liar would never get a request to lie about.
        let mut full = vec![0u8; (num_pieces as usize).div_ceil(8)];
        for p in 0..num_pieces as usize {
            full[p / 8] |= 0x80 >> (p % 8);
        }
        if wire::write_message(&mut stream, &Message::Bitfield(full)).is_err() {
            return;
        }
        if wire::write_message(&mut stream, &Message::Unchoke).is_err() {
            return;
        }
        while let Ok(frame) = wire::read_frame(&mut stream, wire::MAX_MESSAGE_LEN) {
            let Ok(msg) = Message::parse(&frame) else {
                continue;
            };
            if let Message::Request(req) = msg {
                let reply = Message::Piece {
                    index: req.index,
                    begin: req.begin,
                    block: vec![0x2A; req.length as usize],
                };
                if wire::write_message(&mut stream, &reply).is_err() {
                    return;
                }
            }
        }
    });

    let dir = TempDir::new("lied-to");
    let leecher = leeching_torrent(&meta, &dir);
    let ep = net.endpoint();
    let stream = ep
        .dial(liar_dest, Duration::from_secs(5))
        .expect("dial liar");
    leecher.attach(stream, liar_dest).expect("attach to liar");

    // Let it be lied to for long enough to have downloaded the whole torrent
    // several times over had the data been real.
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        assert_eq!(
            leecher.have().count(),
            0,
            "a piece that failed SHA-1 was counted as had"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // And the liar bought itself nothing: an honest seeder still completes
    // the same torrent for a fresh leecher.
    leecher.disconnect_all();
    drop(liar);

    let seed_dir = TempDir::new("honest-seed");
    let seeder = seeding_torrent(&meta, &content, &seed_dir);
    let seed_ep = net.endpoint();
    let seed_dest = seed_ep.dest();
    let _acceptor = spawn_acceptor(&seeder, seed_ep);
    honest_download_completes(&net, &meta, seed_dest, "after-liar");
}

/// The endgame hands one block to more than one peer on purpose. The peer that
/// answers second must not be able to put its bytes over a piece that already
/// verified — in a debug build that tripped the picker's own invariant, and in
/// release it silently corrupted a finished piece the leecher went on to
/// announce and serve.
#[test]
fn a_late_duplicate_block_cannot_corrupt_a_verified_piece() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;
    let num_pieces = u32::try_from(meta.pieces.len()).expect("piece count");

    let dir = TempDir::new("late-dup");
    let leecher = leeching_torrent(&meta, &dir);
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&leecher, ep);

    // Two peers, both claiming the whole torrent. The fixture is small enough
    // that the whole download is inside the endgame window, so the second peer
    // is handed the same blocks as the first — which the assertion below
    // states rather than assumes.
    let honest_ep = net.endpoint();
    let mut honest = honest_ep
        .dial(dest, Duration::from_secs(5))
        .expect("honest dial");
    handshake(&mut honest, info_hash).expect("honest handshake");
    claim_everything(&mut honest, num_pieces).expect("honest claim");
    let first = read_requests(&mut honest, num_pieces as usize);

    let late_ep = net.endpoint();
    let mut late = late_ep
        .dial(dest, Duration::from_secs(5))
        .expect("late dial");
    handshake(&mut late, info_hash).expect("late handshake");
    claim_everything(&mut late, num_pieces).expect("late claim");
    let second = read_requests(&mut late, num_pieces as usize);
    assert_eq!(
        first, second,
        "the endgame should have offered both peers the same blocks"
    );

    // The honest peer answers truthfully; the torrent completes and verifies.
    for req in &first {
        let start = req.index as usize * BLOCK_LEN as usize + req.begin as usize;
        let block = content[start..start + req.length as usize].to_vec();
        wire::write_message(
            &mut honest,
            &Message::Piece {
                index: req.index,
                begin: req.begin,
                block,
            },
        )
        .expect("honest block");
    }
    let deadline = Instant::now() + DEADLINE;
    while leecher.have().count() < num_pieces {
        assert!(
            Instant::now() < deadline,
            "honest peer did not complete the download: {}/{num_pieces}",
            leecher.have().count()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Now the late peer answers the same requests with rubbish of exactly the
    // right shape. Every one of them is for a piece that is already verified.
    for req in &second {
        wire::write_message(
            &mut late,
            &Message::Piece {
                index: req.index,
                begin: req.begin,
                block: vec![0xEE; req.length as usize],
            },
        )
        .expect("late block");
    }

    // Give the reader thread time to process all of it, then check the bytes.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        leecher.have().count(),
        num_pieces,
        "the leecher gave up pieces it had verified"
    );
    let storage = Storage::create(&meta, &dir.0, false).expect("re-open storage");
    let on_disk = storage.verify_all().expect("verify");
    assert_eq!(
        on_disk.count(),
        num_pieces,
        "a late duplicate block overwrote verified data on disk"
    );
    let mut fetched = Vec::new();
    for p in 0..num_pieces {
        fetched.extend(
            storage
                .read_block(p, 0, storage.piece_len(p))
                .expect("read back"),
        );
    }
    assert_eq!(
        fetched, content,
        "the file on disk is not the file we wanted"
    );
}

/// A peer that asks for data and then stops reading its socket. Its own
/// connection may starve — that is its choice — but it must not park the
/// threads serving anybody else: outgoing messages for one peer are queued by
/// whichever *other* peer's reader thread caused them (a broadcast `have`, a
/// choke round), so a blocking send here stalls the whole torrent.
#[test]
fn a_peer_that_stops_reading_cannot_stall_an_honest_one() {
    // Small stream buffers so "stopped reading" bites in a few messages
    // instead of a few thousand.
    let net = MockNet::with_capacity(32 * 1024);
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;
    let num_pieces = u32::try_from(meta.pieces.len()).expect("piece count");
    let held_at_start = num_pieces / 2;

    // The torrent under test holds half the pieces: enough to serve the
    // hostile peer, not enough to be finished.
    let dir = TempDir::new("no-read");
    let leecher = partly_seeded_torrent(&meta, &content, &dir, held_at_start);
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&leecher, ep);

    // The hostile peer: interested, so it gets unchoked and served, then a
    // flood of requests whose answers it never reads.
    let evil_ep = net.endpoint();
    let mut evil = evil_ep
        .dial(dest, Duration::from_secs(5))
        .expect("evil dial");
    handshake(&mut evil, info_hash).expect("evil handshake");
    wire::write_message(&mut evil, &Message::Interested).expect("evil interest");
    let flood = std::thread::spawn(move || {
        for _ in 0..2000 {
            let req = Message::Request(wire::BlockRequest {
                index: 0,
                begin: 0,
                length: BLOCK_LEN,
            });
            if wire::write_message(&mut evil, &req).is_err() {
                break;
            }
        }
        // Hold the connection open, still not reading, well past the deadline
        // below. This has to outlast it: dropping the stream would close the
        // queue the engine is stuck on and release the stall, and the test
        // would then pass for the wrong reason.
        std::thread::sleep(DEADLINE * 3);
    });
    // Let the engine fill that peer's outgoing queue.
    std::thread::sleep(Duration::from_millis(500));

    // Only now does an honest seeder appear. The download must finish.
    let seed_dir = TempDir::new("no-read-seed");
    let seeder = seeding_torrent(&meta, &content, &seed_dir);
    let seed_ep = net.endpoint();
    let seed_dest = seed_ep.dest();
    let _seed_acceptor = spawn_acceptor(&seeder, seed_ep);
    let dial_ep = net.endpoint();
    let stream = dial_ep
        .dial(seed_dest, Duration::from_secs(5))
        .expect("dial seeder");
    leecher.attach(stream, seed_dest).expect("attach seeder");

    let deadline = Instant::now() + DEADLINE;
    while leecher.have().count() < num_pieces {
        assert!(
            Instant::now() < deadline,
            "honest download stalled at {}/{num_pieces} pieces behind a peer that \
             stopped reading (it held {held_at_start} before the hostile peer attached)",
            leecher.have().count()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(flood);
}

/// The same peer, asked the harder question: dropping it from the table has to
/// *reclaim* it, not merely forget it.
///
/// Removing a peer used to drop its outgoing channel and nothing else, which
/// wakes a writer waiting for the next message but not one already blocked
/// inside a write — and the peer worth removing is exactly the one whose queue
/// filled because it stopped reading, so its writer is always in the second
/// state. The entry went, the connection stayed: two threads, a descriptor and
/// a router-side stream each. Then, because the table slot was free, the same
/// peer could reconnect and take another set.
///
/// So this asserts on threads rather than on the peer table. Every hostile
/// connection is held open to the end of the test: if reclamation only happens
/// because the peer hung up, that is the peer's doing, not ours, and a real one
/// will not oblige.
#[test]
fn a_peer_that_stops_reading_is_reclaimed_not_merely_dropped() {
    const ROUNDS: usize = 4;

    // Small buffers so "stopped reading" bites in a few messages, as above.
    let net = MockNet::with_capacity(32 * 1024);
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("reclaim");
    let server = seeding_torrent(&meta, &content, &dir);
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&server, ep);

    // Held to the end of the test, never read from, never dropped.
    let mut wedged: Vec<MockStream> = Vec::new();
    for round in 0..ROUNDS {
        let evil_ep = net.endpoint();
        let mut evil = evil_ep
            .dial(dest, Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("evil dial {round}: {e}"));
        handshake(&mut evil, info_hash).unwrap_or_else(|e| panic!("evil handshake {round}: {e}"));
        wire::write_message(&mut evil, &Message::Interested)
            .unwrap_or_else(|e| panic!("evil interest {round}: {e}"));
        // Ask for far more than the outgoing queue can hold, and never read a
        // byte of the answer. Writes start failing once the engine gives up on
        // us, which is the point — it is not an error here.
        for _ in 0..2000 {
            let req = Message::Request(wire::BlockRequest {
                index: 0,
                begin: 0,
                length: BLOCK_LEN,
            });
            if wire::write_message(&mut evil, &req).is_err() {
                break;
            }
        }
        wedged.push(evil);
    }

    // Every one of them must be gone from the table *and* off the thread list.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let (peers, threads) = (server.connected_peers().len(), server.live_threads());
        if peers == 0 && threads == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{ROUNDS} peers that stopped reading left {peers} in the table and \
             {threads} live thread(s) behind; every connection is still open, so \
             nothing but clove closing them can reclaim these"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Only now: the connections were open for all of that.
    drop(wedged);
}

/// A peer that keeps re-announcing what it has. Availability is a global signal
/// — rarest-first steers the whole torrent by it — so a peer that can inflate it
/// permanently decides what everyone downloads first, and the inflation outlives
/// the peer because leaving withdraws its bitfield exactly once.
///
/// Held open and asserted on directly rather than fired and forgotten: the
/// engine registers a peer on its own thread, so a test that attacks and
/// immediately looks at the peer table can win that race and assert nothing at
/// all. (It did, on the first attempt.)
#[test]
fn re_announcing_a_piece_set_does_not_distort_availability() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;
    let num_pieces = u32::try_from(meta.pieces.len()).expect("piece count");

    let dir = TempDir::new("have-spam");
    let leecher = leeching_torrent(&meta, &dir);
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&leecher, ep);

    let mut peer = raw_peer(&net, dest, info_hash);

    // Every way to say "what I have" — repeatedly, and in combinations no
    // honest peer sends.
    let spam = |s: &mut MockStream| -> std::io::Result<()> {
        claim_everything(s, num_pieces)?;
        for _ in 0..200 {
            wire::write_message(s, &Message::Have(0))?;
        }
        claim_everything(s, num_pieces)?;
        wire::write_message(s, &Message::HaveAll)?;
        wire::write_message(s, &Message::HaveNone)?;
        wire::write_message(s, &Message::HaveAll)?;
        for piece in 0..num_pieces {
            wire::write_message(s, &Message::Have(piece))?;
            wire::write_message(s, &Message::Have(piece))?;
        }
        wire::write_message(s, &Message::HaveNone)
    };
    spam(&mut peer).expect("spam");

    // While it is still attached, one peer holding a piece counts once —
    // whatever it said and however often.
    let deadline = Instant::now() + DEADLINE;
    while leecher.connected_peers().len() != 1 {
        assert!(Instant::now() < deadline, "the peer never attached");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300)); // let the spam drain
    for piece in 0..num_pieces {
        assert!(
            leecher.availability(piece) <= 1,
            "piece {piece}: one peer inflated availability to {}",
            leecher.availability(piece)
        );
    }

    // And when it leaves, everything it contributed goes with it.
    drop(peer);
    let deadline = Instant::now() + DEADLINE;
    while !leecher.connected_peers().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the spamming peer was never cleaned up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    for piece in 0..num_pieces {
        assert_eq!(
            leecher.availability(piece),
            0,
            "piece {piece}: availability outlived the only peer that claimed it"
        );
    }

    // And the torrent still works afterwards.

    let seed_dir = TempDir::new("have-spam-seed");
    let seeder = seeding_torrent(&meta, &content, &seed_dir);
    let seed_ep = net.endpoint();
    let seed_dest = seed_ep.dest();
    let _seed_acceptor = spawn_acceptor(&seeder, seed_ep);
    let dial_ep = net.endpoint();
    let stream = dial_ep
        .dial(seed_dest, Duration::from_secs(5))
        .expect("dial seeder");
    leecher.attach(stream, seed_dest).expect("attach seeder");
    let deadline = Instant::now() + DEADLINE;
    while leecher.have().count() < num_pieces {
        assert!(
            Instant::now() < deadline,
            "download stalled at {}/{num_pieces} after the have-spam",
            leecher.have().count()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A request whose range runs off the end of the piece it names. Storage bounds
/// reads against the whole torrent, not the piece, so serving it would hand out
/// bytes from the *next* piece — which we may not hold and have certainly not
/// verified as part of this one.
#[test]
fn a_request_may_not_reach_past_its_piece() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    // Holds piece 0 and 1 only, so a read past piece 1 reaches pieces it does
    // not have at all.
    let dir = TempDir::new("straddle");
    let server = partly_seeded_torrent(&meta, &content, &dir, 2);
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&server, ep);

    let mut peer = raw_peer(&net, dest, info_hash);
    wire::write_message(&mut peer, &Message::Interested).expect("interest");

    // Wait to be unchoked, or the requests below are refused for that reason
    // instead of the one under test.
    let deadline = Instant::now() + DEADLINE;
    loop {
        assert!(Instant::now() < deadline, "never unchoked");
        if matches!(next_message(&mut peer), Some(Message::Unchoke)) {
            break;
        }
    }

    // The straddling request first, then a legal one. Messages from one peer
    // are answered in order, so if the first is served at all we see it before
    // the second — no timing guesswork.
    let piece_len = BLOCK_LEN; // this fixture's pieces are one block each
    wire::write_message(
        &mut peer,
        &Message::Request(wire::BlockRequest {
            index: 0,
            begin: piece_len - 1,
            length: BLOCK_LEN,
        }),
    )
    .expect("straddling request");
    wire::write_message(
        &mut peer,
        &Message::Request(wire::BlockRequest {
            index: 1,
            begin: 0,
            length: 16,
        }),
    )
    .expect("legal request");

    let deadline = Instant::now() + DEADLINE;
    loop {
        assert!(
            Instant::now() < deadline,
            "the legal request went unanswered"
        );
        if let Some(Message::Piece {
            index,
            begin,
            block,
        }) = next_message(&mut peer)
        {
            assert_eq!(
                (index, begin, block.len()),
                (1, 0, 16),
                "the straddling request was served"
            );
            assert_eq!(block, &content[BLOCK_LEN as usize..BLOCK_LEN as usize + 16]);
            break;
        }
    }
}

/// Peer exchange has to stay inside its own limits: `PexMessage::parse` treats
/// more than `MAX_PEX_PEERS` destinations as spam and drops the whole message,
/// so a torrent that knows more peers than that must still send something its
/// own kind can read.
#[test]
fn peer_exchange_stays_within_the_limit_it_enforces() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("pex-cap");
    let seeder = seeding_torrent(&meta, &content, &dir);

    // Far more peers than one message may carry, and more than the torrent is
    // willing to remember.
    let many: Vec<DestHash> = (0..4000u32)
        .map(|i| {
            let mut hash = [0u8; 32];
            hash[..4].copy_from_slice(&i.to_be_bytes());
            hash[4] = 0xA5;
            DestHash(hash)
        })
        .collect();
    seeder.add_peers(&many);
    assert!(
        seeder.known_peers().len() <= clove_core::torrent::MAX_KNOWN_PEERS,
        "the known-peer set is unbounded: {} entries",
        seeder.known_peers().len()
    );

    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    let mut peer = raw_peer(&net, dest, info_hash);
    // Advertise i2p_pex, which is what prompts the engine to send its set.
    let mut m = BTreeMap::new();
    m.insert(b"i2p_pex".to_vec(), Ben::Int(1));
    let mut hs = BTreeMap::new();
    hs.insert(b"m".to_vec(), Ben::Dict(m));
    wire::write_message(
        &mut peer,
        &Message::Extended {
            id: 0,
            payload: bencode::encode(&Ben::Dict(hs)),
        },
    )
    .expect("ext handshake");

    let deadline = Instant::now() + DEADLINE;
    loop {
        assert!(Instant::now() < deadline, "no PEX message arrived");
        if let Some(Message::Extended { id: 1, payload }) = next_message(&mut peer) {
            let parsed = clove_core::pex::PexMessage::parse(&payload)
                .expect("our own PEX message must parse under our own parser");
            assert!(parsed.added.len() <= clove_core::pex::MAX_PEX_PEERS);
            assert!(
                !parsed.added.is_empty(),
                "an empty PEX message is pointless"
            );
            break;
        }
    }
}

/// BEP 27: a private torrent's peers come from its tracker and nowhere else.
///
/// `info.private` was parsed, stored, and reported by `GET /v1/torrents/<hash>`
/// — and read by nothing. Peer exchange ran on a private torrent exactly as on
/// a public one, which hands a swarm member's destination to whoever completes
/// a handshake with us. On I2P a destination is not an IP a tracker would
/// rather not publish; it is the whole of how to reach that peer, and the
/// tracker deciding who may have it is what "private" means.
///
/// Both directions, because they fail differently: we must not *send* our
/// address book, and we must not *learn* from a peer that sends us one anyway.
#[test]
fn a_private_torrent_speaks_no_peer_exchange() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_with_private(&content, true);
    assert!(meta.private, "fixture must be a private torrent");
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("pex-private");
    let seeder = seeding_torrent(&meta, &content, &dir);

    // An address book worth leaking, so an empty PEX message is not what makes
    // this pass.
    let known: Vec<DestHash> = (0..64u32)
        .map(|i| {
            let mut hash = [0u8; 32];
            hash[..4].copy_from_slice(&i.to_be_bytes());
            hash[4] = 0xC3;
            DestHash(hash)
        })
        .collect();
    seeder.add_peers(&known);
    assert!(
        !seeder.known_peers().is_empty(),
        "fixture has nothing to leak"
    );

    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    let mut peer = raw_peer(&net, dest, info_hash);
    // Offer peer exchange, which on a public torrent is what prompts the engine
    // to send its set, and send one unprompted as well.
    let mut m = BTreeMap::new();
    m.insert(b"i2p_pex".to_vec(), Ben::Int(1));
    let mut hs = BTreeMap::new();
    hs.insert(b"m".to_vec(), Ben::Dict(m));
    wire::write_message(
        &mut peer,
        &Message::Extended {
            id: 0,
            payload: bencode::encode(&Ben::Dict(hs)),
        },
    )
    .expect("ext handshake");

    // Eight destinations of one repeated byte each, so they are recognisable
    // in the address book afterwards and cannot be confused with the fixture's
    // or with the connecting peer's own.
    let offered: Vec<DestHash> = (0..8u8).map(|i| DestHash([0xE0 | i; 32])).collect();
    let mut added = Vec::new();
    for dest in &offered {
        added.extend_from_slice(&dest.0);
    }
    let mut pex = BTreeMap::new();
    pex.insert(b"added".to_vec(), Ben::Bytes(added));
    wire::write_message(
        &mut peer,
        &Message::Extended {
            id: 1,
            payload: bencode::encode(&Ben::Dict(pex)),
        },
    )
    .expect("pex offer");

    // Read everything the engine has to say for a while. Its own extension
    // handshake must not advertise `i2p_pex`, and no PEX message may follow.
    let deadline = Instant::now() + DEADLINE;
    let mut saw_handshake = false;
    while Instant::now() < deadline {
        match next_message(&mut peer) {
            Some(Message::Extended { id: 0, payload }) => {
                let ours = extension::Handshake::parse(&payload).expect("our own handshake parses");
                assert_eq!(
                    ours.id_for("i2p_pex"),
                    None,
                    "a private torrent advertised i2p_pex"
                );
                // Metadata exchange is not restricted by BEP 27: it serves the
                // torrent we both already hold, not a way to reach anyone.
                assert!(
                    ours.id_for("ut_metadata").is_some(),
                    "ut_metadata should still be offered"
                );
                saw_handshake = true;
            }
            Some(Message::Extended { id: 1, .. }) => {
                panic!("a private torrent sent a PEX message");
            }
            _ => {}
        }
    }
    assert!(
        saw_handshake,
        "the engine never sent its extension handshake"
    );

    assert_eq!(
        seeder.pex_learned(),
        0,
        "a private torrent learned peers over PEX"
    );
    // The book may have gained the destination that dialled us — a peer we are
    // talking to is not a peer we were told about. What it must not contain is
    // anything the PEX message offered.
    let book = seeder.known_peers();
    for dest in &offered {
        assert!(
            !book.contains(dest),
            "a destination offered over PEX reached a private torrent's address book"
        );
    }
}

/// One destination may not take the whole peer table.
///
/// Nothing used to stop it. The dial sweep skips destinations it is already
/// connected to, but the inbound path had no such check, so a single destination
/// could dial in a loop and fill every slot — and, through the shared budget,
/// slots belonging to every other torrent. The damage is not just the memory: a
/// peer holding every slot is the only peer we talk to, so its piece set is the
/// only availability rarest-first can see, and no honest peer can get in.
///
/// The cap is per destination, not per connection attempt, so the check must
/// hold however many connections arrive at once — hence the concurrent burst
/// rather than a tidy loop.
#[test]
fn one_destination_cannot_monopolise_the_peer_table() {
    use clove_core::torrent::MAX_CONNECTIONS_PER_DEST;

    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("one-dest");
    let seeder = seeding_torrent(&meta, &content, &dir);
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    // One endpoint — one destination — dialling far more times than its share,
    // all at once. Each connection handshakes correctly and then just sits
    // there, which is the cheapest possible attack and the hardest to fault.
    let attacker = net.endpoint();
    let attacker_dest = attacker.dest();
    let attempts = MAX_CONNECTIONS_PER_DEST + 6;
    let mut held = Vec::new();
    for _ in 0..attempts {
        let Ok(mut stream) = attacker.dial(dest, Duration::from_secs(5)) else {
            continue;
        };
        if handshake(&mut stream, info_hash).is_err() {
            continue;
        }
        stream.set_timeouts(Some(FRAME_WAIT));
        held.push(stream);
    }

    // The table settles at the cap, not at the number of connections offered.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let peers = seeder.connected_peers();
        let mine = peers.iter().filter(|&&d| d == attacker_dest).count();
        assert!(
            mine <= MAX_CONNECTIONS_PER_DEST,
            "one destination holds {mine} connections, cap is {MAX_CONNECTIONS_PER_DEST}"
        );
        if mine == MAX_CONNECTIONS_PER_DEST {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the attacker never reached the cap ({mine} connections); \
             the test is not exercising it"
        );
    }
    // Give any connection the engine was going to accept time to show up, then
    // re-check: the assertion above must hold at rest, not just in passing.
    std::thread::sleep(Duration::from_millis(200));
    let settled = seeder
        .connected_peers()
        .iter()
        .filter(|&&d| d == attacker_dest)
        .count();
    assert_eq!(
        settled, MAX_CONNECTIONS_PER_DEST,
        "the cap did not hold once the burst finished"
    );

    // And the whole point: an honest peer still gets in and still finishes.
    honest_download_completes(&net, &meta, dest, "one-dest-honest");
    drop(held);
}

/// A PEX flood cannot shut the tracker's real peers out of the candidate set.
///
/// `known_peers` is capped, and the cap used to refuse *new* entries once full
/// rather than evicting — which sounds conservative and is the opposite. PEX
/// carries 512 destinations per message with no limit on messages, so two
/// messages from the first peer to connect filled the set with addresses of its
/// choosing, and from then until a restart no tracker reply could add a single
/// real peer to that torrent.
///
/// A PEX-learned entry is now the first thing evicted when a trusted source
/// needs the room, and PEX itself evicts nothing.
#[test]
fn a_pex_flood_cannot_shut_out_the_trackers_peers() {
    use clove_core::pex::{MAX_PEX_PEERS, PexMessage};
    use clove_core::torrent::MAX_KNOWN_PEERS;

    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("pex-flood");
    let seeder = seeding_torrent(&meta, &content, &dir);
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    let mut peer = raw_peer(&net, dest, info_hash);
    // Advertise i2p_pex so the engine routes our messages to the PEX handler.
    let mut m = BTreeMap::new();
    m.insert(b"i2p_pex".to_vec(), Ben::Int(1));
    let mut hs = BTreeMap::new();
    hs.insert(b"m".to_vec(), Ben::Dict(m));
    wire::write_message(
        &mut peer,
        &Message::Extended {
            id: 0,
            payload: bencode::encode(&Ben::Dict(hs)),
        },
    )
    .expect("ext handshake");

    // Flood the set to its cap, 512 fabricated destinations at a time. Each
    // message is well-formed and within every limit the parser enforces — that
    // is what made this work.
    let mut sent = 0usize;
    let mut tag = 0u32;
    while sent < MAX_KNOWN_PEERS * 2 {
        let added: Vec<DestHash> = (0..MAX_PEX_PEERS)
            .map(|_| {
                tag += 1;
                let mut hash = [0xEEu8; 32];
                hash[..4].copy_from_slice(&tag.to_be_bytes());
                DestHash(hash)
            })
            .collect();
        wire::write_message(
            &mut peer,
            &Message::Extended {
                id: 1,
                payload: PexMessage {
                    added,
                    dropped: Vec::new(),
                }
                .encode(),
            },
        )
        .expect("pex flood");
        sent += MAX_PEX_PEERS;
    }

    // Wait for the flood to land, so the set really is full when we test it.
    let deadline = Instant::now() + DEADLINE;
    while seeder.known_peers().len() < MAX_KNOWN_PEERS {
        assert!(
            Instant::now() < deadline,
            "the flood never filled the set ({} entries); the test proves nothing",
            seeder.known_peers().len()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        seeder.known_peers().len() <= MAX_KNOWN_PEERS,
        "the flood pushed the set past its cap"
    );

    // Now the tracker answers. Every one of these must be remembered.
    let from_tracker: Vec<DestHash> = (0..64u32)
        .map(|i| {
            let mut hash = [0x11u8; 32];
            hash[..4].copy_from_slice(&i.to_be_bytes());
            DestHash(hash)
        })
        .collect();
    seeder.add_peers(&from_tracker);

    let known = seeder.known_peers();
    let kept = from_tracker.iter().filter(|d| known.contains(d)).count();
    assert_eq!(
        kept,
        from_tracker.len(),
        "{} of {} tracker peers were shut out by the flood",
        from_tracker.len() - kept,
        from_tracker.len()
    );
    assert!(
        known.len() <= MAX_KNOWN_PEERS,
        "making room for the tracker's peers broke the cap"
    );
}

/// Choke rounds are periodic in BEP 3, and the optimistic slot is how a peer
/// that arrives after the slots are taken ever gets served. Without a round on
/// a timer, whoever was unchoked first keeps the slot for the life of the
/// connection and the peers behind them wait forever.
///
/// This is the traffic-driven path — the fallback for an embedder driving
/// `Torrent` with no maintenance tick. The tick itself is covered by
/// `choke_rounds_run_without_any_traffic_at_all`, which is the case that
/// matters: a connection with nothing moving on it is exactly when the slots
/// need reconsidering.
#[test]
fn every_interested_peer_eventually_gets_a_turn() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("choke-rotation");
    let seeder = seeding_torrent(&meta, &content, &dir);
    // Rounds every 50 ms rather than the default ten seconds, so the rotation
    // is observable inside a test.
    seeder.set_choke_interval(Duration::from_millis(50));
    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    // More interested peers than there are slots (the choker unchokes four).
    let count = 6;
    let seen: Vec<Arc<AtomicBool>> = (0..count)
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();
    let mut writers = Vec::new();
    for flag in &seen {
        let peer_ep = net.endpoint();
        let mut peer = peer_ep
            .dial(dest, Duration::from_secs(5))
            .expect("peer dial");
        handshake(&mut peer, info_hash).expect("peer handshake");
        wire::write_message(&mut peer, &Message::Interested).expect("interest");
        let mut reader = peer.try_clone();
        let flag = Arc::clone(flag);
        std::thread::spawn(move || {
            while let Ok(frame) = wire::read_frame(&mut reader, wire::MAX_MESSAGE_LEN) {
                if matches!(Message::parse(&frame), Ok(Message::Unchoke)) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        });
        writers.push(peer);
    }

    // Keep a trickle of traffic going: rounds are driven by messages arriving,
    // not by a timer thread.
    let deadline = Instant::now() + DEADLINE;
    loop {
        if seen.iter().all(|f| f.load(Ordering::Relaxed)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {count} peers were ever unchoked",
            seen.iter().filter(|f| f.load(Ordering::Relaxed)).count()
        );
        for peer in &mut writers {
            let _ = wire::write_message(peer, &Message::KeepAlive);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The same rotation, with no traffic at all. Nothing is sent by these peers
/// after their initial interest, so only the maintenance tick can be running the
/// rounds — which is the honest version of the requirement, since a torrent with
/// four busy peers and two idle ones has no reason to generate messages for the
/// idle ones.
#[test]
fn choke_rounds_run_without_any_traffic_at_all() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("choke-tick");
    let seeder = seeding_torrent(&meta, &content, &dir);
    seeder.set_choke_interval(Duration::from_millis(50));
    // Long enough that nothing here is dropped for being quiet, which is the
    // whole premise of the test.
    seeder.set_idle_timeout(Duration::from_secs(600));
    seeder.set_keepalive_interval(Duration::from_secs(600));
    let _tick = seeder.spawn_maintenance(Duration::from_millis(20));

    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    let count = 6;
    let seen: Vec<Arc<AtomicBool>> = (0..count)
        .map(|_| Arc::new(AtomicBool::new(false)))
        .collect();
    let mut held = Vec::new();
    for flag in &seen {
        let peer_ep = net.endpoint();
        let mut peer = peer_ep
            .dial(dest, Duration::from_secs(5))
            .expect("peer dial");
        handshake(&mut peer, info_hash).expect("peer handshake");
        wire::write_message(&mut peer, &Message::Interested).expect("interest");
        let mut reader = peer.try_clone();
        let flag = Arc::clone(flag);
        std::thread::spawn(move || {
            while let Ok(frame) = wire::read_frame(&mut reader, wire::MAX_MESSAGE_LEN) {
                if matches!(Message::parse(&frame), Ok(Message::Unchoke)) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        });
        held.push(peer);
    }

    let deadline = Instant::now() + DEADLINE;
    while !seen.iter().all(|f| f.load(Ordering::Relaxed)) {
        assert!(
            Instant::now() < deadline,
            "only {} of {count} peers were ever unchoked with no traffic to drive rounds",
            seen.iter().filter(|f| f.load(Ordering::Relaxed)).count()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(held);
}

/// A peer we have nothing to say to still has to hear from us. Clients drop a
/// connection that has gone quiet for a few minutes, so without keep-alives the
/// *other* side hangs up — and on I2P redialling costs tunnel setup, which makes
/// this more expensive than the clearnet equivalent.
#[test]
fn a_quiet_connection_still_gets_keep_alives() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    // A seeder with nothing to say: the peer below never claims a piece, never
    // asks for one, and is never interested, so no other message is due.
    let dir = TempDir::new("keepalive");
    let seeder = seeding_torrent(&meta, &content, &dir);
    seeder.set_keepalive_interval(Duration::from_millis(50));
    seeder.set_idle_timeout(Duration::from_secs(600));
    let _tick = seeder.spawn_maintenance(Duration::from_millis(20));

    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    let mut peer = raw_peer(&net, dest, info_hash);

    // Two of them, so this is a repeating clock and not one opening flourish.
    let deadline = Instant::now() + DEADLINE;
    let mut got = 0;
    while got < 2 {
        assert!(
            Instant::now() < deadline,
            "only {got} keep-alives arrived on an otherwise silent connection"
        );
        if matches!(next_message(&mut peer), Some(Message::KeepAlive)) {
            got += 1;
        }
    }
}

/// A peer that says nothing at all is eventually dropped, so its slot and its
/// bookkeeping come back. The connection is held open throughout: this is the
/// silent-but-connected case, not a disconnect.
#[test]
fn a_peer_that_says_nothing_is_eventually_dropped() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let info_hash = meta.info_hash.0;

    let dir = TempDir::new("idle-drop");
    let seeder = seeding_torrent(&meta, &content, &dir);
    seeder.set_idle_timeout(Duration::from_millis(200));
    // No keep-alives: they would be the only thing we send, and this is about
    // what we *hear*.
    seeder.set_keepalive_interval(Duration::from_secs(600));
    let _tick = seeder.spawn_maintenance(Duration::from_millis(20));

    let ep = net.endpoint();
    let dest = ep.dest();
    let _acceptor = spawn_acceptor(&seeder, ep);

    let peer = raw_peer(&net, dest, info_hash);
    let deadline = Instant::now() + DEADLINE;
    while seeder.connected_peers().is_empty() {
        assert!(Instant::now() < deadline, "the peer never attached");
        std::thread::sleep(Duration::from_millis(10));
    }

    // Now it goes quiet while staying connected.
    let deadline = Instant::now() + DEADLINE;
    while !seeder.connected_peers().is_empty() {
        assert!(
            Instant::now() < deadline,
            "a peer that said nothing for well past the idle timeout was kept"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(peer);
}

/// The tick must not disturb a working transfer: a peer sending and receiving is
/// never idle, and keep-alives are just another message in the stream.
#[test]
fn maintenance_does_not_disturb_a_live_download() {
    let net = MockNet::new();
    let content = content();
    let meta = meta_for(&content);
    let num_pieces = u32::try_from(meta.pieces.len()).expect("piece count");

    let seed_dir = TempDir::new("tick-seed");
    let seeder = seeding_torrent(&meta, &content, &seed_dir);
    seeder.set_keepalive_interval(Duration::from_millis(30));
    seeder.set_idle_timeout(Duration::from_millis(400));
    let _seed_tick = seeder.spawn_maintenance(Duration::from_millis(10));
    let seed_ep = net.endpoint();
    let seed_dest = seed_ep.dest();
    let _acceptor = spawn_acceptor(&seeder, seed_ep);

    let leech_dir = TempDir::new("tick-leech");
    let leecher = leeching_torrent(&meta, &leech_dir);
    leecher.set_keepalive_interval(Duration::from_millis(30));
    leecher.set_idle_timeout(Duration::from_millis(400));
    let _leech_tick = leecher.spawn_maintenance(Duration::from_millis(10));

    let ep = net.endpoint();
    let stream = ep
        .dial(seed_dest, Duration::from_secs(5))
        .expect("dial seeder");
    leecher.attach(stream, seed_dest).expect("attach");

    let deadline = Instant::now() + DEADLINE;
    while leecher.have().count() < num_pieces {
        assert!(
            Instant::now() < deadline,
            "download stalled at {}/{num_pieces} with maintenance running",
            leecher.have().count()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The tick stops when its handle is dropped, and cannot keep a torrent (and the
/// files it holds open) alive by itself.
#[test]
fn the_maintenance_tick_outlives_neither_its_handle_nor_its_torrent() {
    let content = content();
    let meta = meta_for(&content);

    let dir = TempDir::new("tick-life");
    let seeder = seeding_torrent(&meta, &content, &dir);
    let weak = Arc::downgrade(&seeder);

    // Dropping the handle stops the thread; dropping the torrent must then be
    // the last reference, or the tick is holding it open.
    let tick = seeder.spawn_maintenance(Duration::from_millis(10));
    std::thread::sleep(Duration::from_millis(50));
    drop(tick);
    drop(seeder);
    let deadline = Instant::now() + DEADLINE;
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "something is still holding the torrent after its handle and its Arc went"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // And the other way round: a handle nobody drops must not pin the torrent.
    let seeder = seeding_torrent(&meta, &content, &dir);
    let weak = Arc::downgrade(&seeder);
    let forgotten = seeder.spawn_maintenance(Duration::from_millis(10));
    drop(seeder);
    let deadline = Instant::now() + DEADLINE;
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "a forgotten maintenance handle kept the torrent alive"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(forgotten);
}

/// A peer that takes blocks and never answers.
///
/// Every hostile peer above is hostile in a way the engine can see: it lies,
/// it sends rubbish, it floods, or it goes silent and trips the idle timeout.
/// These ones are polite. They handshake, unchoke, claim everything, take
/// whatever they are offered, and answer none of it — while sending
/// keep-alives, so they never look idle and are never dropped.
///
/// BEP 3 gives the requester nothing to detect that with: no acknowledgement,
/// no error, no obligation to answer. A dropped request is indistinguishable
/// from a slow one, and without a deadline it is **permanent** — the block
/// stays in the peer's pipeline and in the picker's in-flight set, so it is
/// never asked for again and its piece never completes.
///
/// **The endgame does not save you, and that is the whole reason this needs a
/// torrent of its own.** Duplicate requests only start once the torrent is
/// within `DEFAULT_ENDGAME_BLOCKS` of done, so a handful of stuck blocks is
/// quietly rescued — which is why the five-block fixture the rest of this file
/// uses cannot show the bug at all. Live it was ~186 blocks held across ~45
/// peers, far outside that window, and the download simply stopped: 43 pieces
/// outstanding, every one about three-quarters finished, none finishing
/// (`PROTOCOL.i2p-bt` §4.7). Four peers at a full pipeline each reproduce that
/// shape in miniature.
///
/// The claim is the suite's third contract — a misbehaving peer cannot deny
/// service to an honest one — in the form the engine used to fail.
#[test]
fn peers_that_take_blocks_and_never_answer_cannot_stall_the_download() {
    /// Comfortably more than `DEFAULT_ENDGAME_BLOCKS`, so the blocks these
    /// peers sit on cannot be swept up by the endgame instead of by the
    /// deadline under test.
    const DROPPERS: usize = 4;
    /// One block per piece, so a stuck block is a stuck piece and the
    /// arithmetic above is legible. Enough of them that the seeder has real
    /// work left after the droppers have taken their fill.
    const PIECES: u32 = 128;

    let net = MockNet::new();
    let content: Vec<u8> = (0..(PIECES * BLOCK_LEN))
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    let meta = meta_for(&content);
    let num_pieces = u32::try_from(meta.pieces.len()).expect("piece count");
    assert_eq!(num_pieces, PIECES, "one block per piece");

    let seed_dir = TempDir::new("drop-seed");
    let seeder = seeding_torrent(&meta, &content, &seed_dir);
    let seed_ep = net.endpoint();
    let seed_dest = seed_ep.dest();
    let _seed_accept = spawn_acceptor(&seeder, seed_ep);

    let leech_dir = TempDir::new("drop-leech");
    let leecher = leeching_torrent(&meta, &leech_dir);
    // Short enough that the test does not take minutes, long enough to be a
    // real wait rather than a formality.
    leecher.set_request_timeout(Duration::from_millis(400));
    let leech_ep = net.endpoint();
    let leech_dest = leech_ep.dest();
    let _leech_accept = spawn_acceptor(&leecher, leech_ep);
    let _tick = leecher.spawn_maintenance(Duration::from_millis(50));

    // The droppers connect first, so they get their pick of the blocks.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut threads = Vec::new();
    for _ in 0..DROPPERS {
        let mut stream = raw_peer(&net, leech_dest, meta.info_hash.0);
        claim_everything(&mut stream, num_pieces).expect("claim");
        let stop = Arc::clone(&stop);
        threads.push(std::thread::spawn(move || {
            let mut taken = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                // Keep-alives either way, so the idle timeout — the only
                // mechanism that used to reclaim anything — never fires.
                if let Some(Message::Request(_)) = next_message(&mut stream) {
                    taken += 1;
                }
                let _ = wire::write_message(&mut stream, &Message::KeepAlive);
            }
            taken
        }));
    }

    // Let them fill their pipelines before the honest seeder appears, so the
    // blocks they are sitting on are genuinely lost to the picker.
    std::thread::sleep(Duration::from_millis(200));

    leecher.add_peers(&[seed_dest]);
    let stream = net
        .endpoint()
        .dialer()
        .dial(seed_dest, Duration::from_secs(5))
        .expect("dial the seeder");
    leecher
        .attach(stream, seed_dest)
        .expect("attach to the seeder");

    assert!(
        leecher.wait_complete(DEADLINE),
        "peers that never answered a request stalled the whole download at {}/{num_pieces} pieces",
        leecher.have().count()
    );

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let taken: u64 = threads.into_iter().map(|t| t.join().unwrap_or(0)).sum();
    assert!(
        taken > u64::from(clove_core::picker::DEFAULT_ENDGAME_BLOCKS),
        "only {taken} block(s) were ever held, which the endgame alone would \
         have rescued — this test proved nothing"
    );
}
