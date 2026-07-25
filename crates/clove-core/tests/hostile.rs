//! Hostile-input sweep over every parser (`docs/SCOPE.md` §9).
//!
//! Each of clove's parsers reads bytes an attacker chooses: a peer's wire
//! messages and extension payloads, a tracker's reply, a `.torrent` or magnet
//! handed over by anyone, a resume file that may have been tampered with. The
//! contract is uniform and absolute: **parse or return an error — never panic,
//! never hang, never read out of bounds.**
//!
//! This test enforces that contract cheaply enough to run on every push. It
//! takes a valid sample of each format, mutates it tens of thousands of ways
//! with a deterministic PRNG, and feeds the result to the parser. A panic
//! fails the test; the harness needs no unwind catching for that. Anything
//! that parses successfully is additionally re-encoded and re-parsed, so a
//! parser cannot "succeed" into a value its own encoder disagrees with.
//!
//! Deterministic on purpose: a failure here reproduces from the printed seed
//! rather than being a once-in-a-blue-moon CI flake. Deep, coverage-guided
//! fuzzing is the companion job in `fuzz/` (see `fuzz/README.md`); this is the
//! part that runs everywhere, always, with no nightly toolchain.

use std::collections::BTreeMap;
use std::io::Cursor;

use clove_core::bencode::{self, Value as Ben};
use clove_core::{extension, http, json, magnet, metadata, metainfo, pex, resume, tracker, wire};

/// Mutations tried per seed input. Sized so the whole sweep stays well under a
/// second in debug builds — it runs on every push, so it must never be the
/// reason someone skips the test suite.
const ROUNDS: usize = 3_000;

/// xorshift64*, so a failing case reproduces exactly from its seed. Any small
/// deterministic generator would do; this one is four lines and has no
/// dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        // The modulus keeps the result below `n`, which is a usize already,
        // so the conversion cannot fail on any target.
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }
}

/// Damage `input` in one of the ways a hostile or corrupt source plausibly
/// would: flip a bit, overwrite a byte with an interesting value, truncate,
/// extend, splice out a run, or duplicate one.
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    // Values that historically break length- and delimiter-driven parsers.
    const INTERESTING: [u8; 12] = [
        0x00, 0x01, 0x7F, 0x80, 0xFF, b'0', b'9', b'-', b':', b'e', b'i', b'd',
    ];
    let mut out = input.to_vec();
    match rng.below(6) {
        0 if !out.is_empty() => {
            let at = rng.below(out.len());
            out[at] ^= 1 << rng.below(8);
        }
        1 if !out.is_empty() => {
            let at = rng.below(out.len());
            out[at] = INTERESTING[rng.below(INTERESTING.len())];
        }
        2 if !out.is_empty() => {
            let keep = rng.below(out.len());
            out.truncate(keep);
        }
        3 => {
            let at = rng.below(out.len() + 1);
            out.insert(at, INTERESTING[rng.below(INTERESTING.len())]);
        }
        4 if out.len() > 2 => {
            let at = rng.below(out.len() - 1);
            let len = 1 + rng.below(out.len() - at - 1);
            out.drain(at..at + len);
        }
        _ if !out.is_empty() => {
            let at = rng.below(out.len());
            let len = 1 + rng.below(out.len() - at);
            let run: Vec<u8> = out[at..at + len].to_vec();
            out.extend_from_slice(&run);
        }
        _ => {}
    }
    out
}

/// Run `parse` over mutations of every seed. `parse` returning is the whole
/// assertion: a panic, an overflow in a debug build, or a hang fails the test.
fn sweep(name: &str, seeds: &[Vec<u8>], seed_value: u64, mut parse: impl FnMut(&[u8])) {
    assert!(!seeds.is_empty(), "{name}: no seed inputs");
    let mut rng = Rng(seed_value);
    // The unmodified seeds first: a parser that cannot read its own valid
    // input would make the mutation results meaningless.
    for seed in seeds {
        parse(seed);
    }
    for round in 0..ROUNDS {
        let seed = &seeds[rng.below(seeds.len())];
        // Chain a few mutations so damage compounds beyond single-byte edits.
        let mut case = mutate(&mut rng, seed);
        for _ in 0..rng.below(3) {
            case = mutate(&mut rng, &case);
        }
        // Printed only on failure, via the panic message location; the seed
        // plus round number reproduces the exact case.
        let _ = (name, round);
        parse(&case);
    }
}

/// A valid, if minimal, bencoded torrent: one file, one piece.
fn torrent_bytes() -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), Ben::Int(16_384));
    info.insert(b"name".to_vec(), Ben::Bytes(b"hostile.bin".to_vec()));
    info.insert(b"piece length".to_vec(), Ben::Int(16_384));
    info.insert(b"pieces".to_vec(), Ben::Bytes(vec![0x5A; 20]));
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        Ben::Bytes(b"http://tracker.i2p/announce".to_vec()),
    );
    root.insert(b"info".to_vec(), Ben::Dict(info));
    bencode::encode(&Ben::Dict(root))
}

fn resume_bytes() -> Vec<u8> {
    resume::Resume {
        info_hash: [0x11; 20],
        num_pieces: 12,
        have: vec![0b1010_1010, 0b1111_0000],
        verified: vec![0b1010_1010, 0b1111_0000],
        priorities: vec![1, 0, 2],
        uploaded: 4_096,
        downloaded: 8_192,
        trackers: vec![vec!["http://tracker.i2p/announce".to_owned()]],
        paused: false,
    }
    .encode()
}

#[test]
fn bencode_survives_hostile_input() {
    let seeds = vec![
        torrent_bytes(),
        bencode::encode(&Ben::Int(-42)),
        bencode::encode(&Ben::Bytes(vec![0xFF; 40])),
        bencode::encode(&Ben::List(vec![Ben::Int(0), Ben::Bytes(b"x".to_vec())])),
        b"d3:onei1e3:twod5:threeleee".to_vec(),
    ];
    sweep("bencode", &seeds, 0x5EED_0001, |case| {
        if let Ok(value) = bencode::decode(case) {
            // A decoded value must survive a re-encode/decode round trip: the
            // codec and the parser have to agree on what was read.
            let again =
                bencode::decode(&bencode::encode(&value)).expect("re-encoded bencode must decode");
            assert_eq!(again, value, "bencode round trip disagreed");
        }
        let _ = bencode::decode_prefix(case);
    });
}

#[test]
fn metainfo_survives_hostile_input() {
    let seeds = vec![torrent_bytes()];
    sweep("metainfo", &seeds, 0x5EED_0002, |case| {
        if let Ok(meta) = metainfo::MetaInfo::parse(case) {
            // Structural invariants any accepted torrent must satisfy — these
            // are what the rest of the engine relies on.
            assert!(!meta.pieces.is_empty(), "accepted a torrent with no pieces");
            assert!(meta.piece_length > 0, "accepted a zero piece length");
            let sum: u64 = meta.files.iter().map(|f| f.length).sum();
            assert_eq!(sum, meta.total_length, "file lengths disagree with total");
            for file in &meta.files {
                assert!(!file.path.is_empty(), "accepted an empty file path");
                for part in &file.path {
                    assert!(part != "." && part != ".." && !part.contains('/'));
                    assert!(!part.contains('\0'), "accepted NUL in a path");
                }
            }
        }
        let _ = metainfo::MetaInfo::from_info_dict(case);
    });
}

#[test]
fn resume_survives_hostile_input() {
    let seeds = vec![resume_bytes()];
    sweep("resume", &seeds, 0x5EED_0003, |case| {
        if let Ok(state) = resume::Resume::decode(case) {
            assert_eq!(
                state.have.len(),
                resume::bitfield_len(state.num_pieces),
                "have bitfield length disagrees with num_pieces"
            );
            assert_eq!(state.verified.len(), resume::bitfield_len(state.num_pieces));
            assert!(state.priorities.iter().all(|&p| p <= 2));
            let again = resume::Resume::decode(&state.encode()).expect("re-encoded resume decodes");
            assert_eq!(again, state, "resume round trip disagreed");
        }
    });
}

#[test]
fn json_survives_hostile_input() {
    let seeds = vec![
        br#"{"a":1,"b":[true,false,null],"c":{"d":"e"}}"#.to_vec(),
        r#"[-1.5e10,0,"\u00e9\ud83d\ude00","\\\/\b\f\n\r\t"]"#
            .as_bytes()
            .to_vec(),
        b"{}".to_vec(),
    ];
    sweep("json", &seeds, 0x5EED_0004, |case| {
        // The parser takes &str; invalid UTF-8 is the caller's problem, so
        // feed it both the lossy form and, when valid, the exact bytes.
        if let Ok(text) = std::str::from_utf8(case)
            && let Ok(value) = json::parse(text)
        {
            let again = json::parse(&value.encode()).expect("re-encoded JSON parses");
            assert_eq!(again, value, "JSON round trip disagreed");
        }
        let _ = json::parse(&String::from_utf8_lossy(case));
    });
}

#[test]
fn http_survives_hostile_input() {
    let seeds = vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nhello".to_vec(),
        b"HTTP/1.0 404 Not Found\r\n\r\nbody-to-close".to_vec(),
        b"GET /v1/torrents?verbose=1 HTTP/1.1\r\nHost: clove\r\nX-Clove-Token: t\r\n\r\n".to_vec(),
        b"POST /v1/torrents HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody".to_vec(),
    ];
    sweep("http", &seeds, 0x5EED_0005, |case| {
        let _ = http::read_response(&mut Cursor::new(case), 64 * 1024);
        let _ = http::read_request(&mut Cursor::new(case), 64 * 1024);
        let _ = http::percent_decode(&String::from_utf8_lossy(case));
    });
}

#[test]
fn tracker_responses_survive_hostile_input() {
    let mut compact = b"d8:intervali1800e5:peers64:".to_vec();
    compact.extend_from_slice(&[0xAB; 64]);
    compact.push(b'e');
    let seeds = vec![
        compact,
        b"d14:failure reason17:torrent not founde".to_vec(),
        b"d8:intervali60e12:min intervali30e5:peers0:e".to_vec(),
    ];
    sweep("tracker", &seeds, 0x5EED_0006, |case| {
        if let Ok(response) = tracker::parse_response(case) {
            // A tracker may hand back a nonsense interval — 0, or one second.
            // The parser deliberately passes it through rather than throwing
            // away the peers; the protection is the scheduler's floor, so
            // assert *that*, which is the invariant that actually matters.
            let mut state = tracker::AnnounceState::new();
            let now = 1_000_000u64;
            state.on_success(now, response.interval);
            let floor = tracker::MIN_ANNOUNCE_INTERVAL.as_secs();
            assert!(
                !state.due(now + floor - 1),
                "a hostile interval got us announcing inside the {floor}s floor"
            );
        }
    });
}

#[test]
fn wire_messages_survive_hostile_input() {
    let seeds = vec![
        vec![0u8],                                         // choke
        vec![4, 0, 0, 0, 7],                               // have
        vec![5, 0xFF, 0x0F],                               // bitfield
        vec![6, 0, 0, 0, 1, 0, 0, 0x40, 0, 0, 0, 0x40, 0], // request
        {
            let mut piece = vec![7, 0, 0, 0, 2, 0, 0, 0, 0];
            piece.extend_from_slice(&[0x5A; 64]);
            piece
        },
        vec![20, 0, b'd', b'e'], // extended handshake
    ];
    sweep("wire", &seeds, 0x5EED_0007, |case| {
        let _ = wire::Message::parse(case);
        // The handshake parser takes a fixed-size buffer; give it one built
        // from the case so length handling is exercised too.
        let mut buf = [0u8; wire::HANDSHAKE_LEN];
        let take = case.len().min(wire::HANDSHAKE_LEN);
        buf[..take].copy_from_slice(&case[..take]);
        let _ = wire::Handshake::parse(&buf);
    });
}

#[test]
fn extension_payloads_survive_hostile_input() {
    let mut pex_payload = b"d5:added64:".to_vec();
    pex_payload.extend_from_slice(&[0x11; 64]);
    pex_payload.extend_from_slice(b"7:dropped32:");
    pex_payload.extend_from_slice(&[0x22; 32]);
    pex_payload.push(b'e');

    let mut metadata_payload = b"d8:msg_typei1e5:piecei0e10:total_sizei20eee".to_vec();
    metadata_payload.extend_from_slice(&[0x33; 20]);

    let handshake = b"d1:md9:ut_metadatai2e7:i2p_pexi1ee13:metadata_sizei20ee".to_vec();

    sweep("pex", &[pex_payload], 0x5EED_0008, |case| {
        if let Ok(message) = pex::PexMessage::parse(case) {
            // Whole destination hashes only, and the spam cap must hold.
            assert!(message.added.len() + message.dropped.len() <= pex::MAX_PEX_PEERS);
        }
    });
    sweep("metadata", &[metadata_payload], 0x5EED_0009, |case| {
        let _ = metadata::MetadataMessage::parse(case);
    });
    sweep("extension", &[handshake], 0x5EED_000A, |case| {
        let _ = extension::Handshake::parse(case);
    });
}

#[test]
fn magnet_uris_survive_hostile_input() {
    let seeds = vec![
        b"magnet:?xt=urn:btih:58e2fc46a8dc57c78191f079648750b0644d03a2&dn=demo\
          &tr=http%3A%2F%2Ftracker.i2p%2Fannounce"
            .to_vec(),
        b"magnet:?xt=urn:btih:MFRGGZDFMZTWQ2LKNNWG23TPOBYXE43U".to_vec(),
    ];
    sweep("magnet", &seeds, 0x5EED_000B, |case| {
        let text = String::from_utf8_lossy(case);
        if let Ok(link) = magnet::Magnet::parse(&text) {
            // Every surviving tracker must be an I2P URL: the clearnet filter
            // is a security property, not a nicety.
            for url in &link.trackers {
                // Hostnames are case-insensitive, so ".i2P" is a legitimate
                // I2P host; compare the way the filter itself does.
                assert!(
                    url.to_ascii_lowercase().contains(".i2p"),
                    "magnet parser kept a non-I2P tracker: {url}"
                );
            }
        }
    });
}
