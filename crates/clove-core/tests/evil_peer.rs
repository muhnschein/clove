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
use clove_core::metainfo::MetaInfo;
use clove_core::picker::Mode;
use clove_core::storage::Storage;
use clove_core::torrent::Torrent;
use clove_core::wire::{self, BLOCK_LEN, Handshake, Message};
use i2pnet::mock::{MockNet, MockStream};
use i2pnet::{DestHash, I2pDialer, I2pListener};
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
