//! Fuzz the peer wire codec — the surface any peer on the swarm can drive.
//!
//! A peer connection is a handshake and then a *stream*: the framing takes a
//! four-byte length prefix off the wire and `Message::parse` reads the body
//! behind it. Handing one body straight to `Message::parse`, which is all this
//! target used to do, skips the framing entirely — and the framing is where the
//! length prefix, the oversize ceiling and the short read live.
//!
//! It also left `fuzz/dicts/wire.dict` talking past the target. Most of that
//! file is length-prefixed framing (`"\x00\x00\x00\x0d\x06"` is a request), and
//! a parser reading its input as a bare body rejects every one of those tokens
//! as a wrong-length message. 168 edges, unmoved across five runs and 239
//! million executions, is what that combination produces.
//!
//! There are two framing entry points, and the target reads the stream through
//! both at once. [`wire::read_frame`] allocates a buffer per frame;
//! [`wire::read_frame_into`] fills a caller-owned one, which is what a peer
//! connection uses so that a whole download does not churn 16 KiB per block
//! per peer. A reused buffer is the one that can carry the tail of a longer
//! previous frame into a shorter one, and the failure that produces is a
//! message the peer never sent — no panic anywhere to mark it. So the two are
//! run in lockstep over the same bytes and required to agree.
#![no_main]
use libfuzzer_sys::fuzz_target;
use clove_core::wire::{self, Handshake, Message, BLOCK_LEN, HANDSHAKE_LEN};

/// The ceiling `Torrent` hands every peer reader: enough for a block, a
/// metadata piece or a bitfield, plus header slack. Deliberately this rather
/// than `MAX_MESSAGE_LEN` — a tight ceiling puts the oversize rejection one
/// mutated byte away instead of behind a prefix over a megabyte, and it is
/// what the daemon actually runs with.
const MAX_FRAME: u32 = BLOCK_LEN + 256;

fuzz_target!(|data: &[u8]| {
    // The 68 bytes a peer opens with.
    let mut buf = [0u8; HANDSHAKE_LEN];
    let take = data.len().min(HANDSHAKE_LEN);
    buf[..take].copy_from_slice(&data[..take]);
    let _ = Handshake::parse(&buf);

    // Then the stream, read from the whole input rather than from whatever
    // follows the handshake: every framing token in the dictionary would
    // otherwise need 68 bytes of preamble built in front of it before it could
    // count for anything.
    //
    // One cursor per reader, over the same bytes, so the reused buffer is
    // carrying real previous frames rather than whatever a single cursor last
    // left behind.
    let mut allocating = std::io::Cursor::new(data);
    let mut reusing = std::io::Cursor::new(data);
    let mut body = Vec::new();
    loop {
        let owned = wire::read_frame(&mut allocating, MAX_FRAME);
        let filled = wire::read_frame_into(&mut reusing, MAX_FRAME, &mut body);
        let Ok(owned) = owned else {
            assert!(
                filled.is_err(),
                "read_frame stopped where read_frame_into kept going"
            );
            break;
        };
        assert!(
            filled.is_ok(),
            "read_frame_into stopped where read_frame kept going"
        );
        // The whole point of the buffer being caller-owned: what a peer sent
        // cannot depend on what the last peer message happened to be longer
        // than.
        assert_eq!(
            owned, body,
            "a reused frame buffer disagreed with a fresh one"
        );

        let Ok(message) = Message::parse(&body) else { continue };

        // A message that parsed has to survive its own encoder: the frame it
        // writes must declare its true body length, and that body must parse
        // back to the same message. A codec that disagrees with itself
        // corrupts a transfer without ever panicking, which is exactly the
        // class of bug a target that only asks "did it crash" cannot see.
        let frame = message.encode();
        let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
        assert_eq!(
            declared as usize,
            frame.len() - 4,
            "length prefix disagrees with the body it frames: {message:?}"
        );
        let reparsed = Message::parse(&frame[4..]).expect("our own frame must parse");
        assert_eq!(reparsed, message, "round trip changed the message");
    }
});
