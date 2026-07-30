//! Fuzz the peer wire codec — the surface any peer on the swarm can drive.
//!
//! A peer connection is a handshake and then a *stream*: `read_frame` takes a
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
    let mut cursor = std::io::Cursor::new(data);
    while let Ok(body) = wire::read_frame(&mut cursor, MAX_FRAME) {
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
