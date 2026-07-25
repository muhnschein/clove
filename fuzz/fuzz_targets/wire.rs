//! Fuzz the peer wire codec — the surface any peer on the swarm can drive.
#![no_main]
use libfuzzer_sys::fuzz_target;
use clove_core::wire::{Handshake, Message, HANDSHAKE_LEN};

fuzz_target!(|data: &[u8]| {
    let _ = Message::parse(data);
    let mut buf = [0u8; HANDSHAKE_LEN];
    let take = data.len().min(HANDSHAKE_LEN);
    buf[..take].copy_from_slice(&data[..take]);
    let _ = Handshake::parse(&buf);
});
