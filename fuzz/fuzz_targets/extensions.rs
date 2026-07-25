//! Fuzz the BEP 10 extension payloads: i2p_pex, ut_metadata, and the
//! extension handshake. All three are peer-controlled.
#![no_main]
use libfuzzer_sys::fuzz_target;
use clove_core::{extension, metadata, pex};

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = pex::PexMessage::parse(data) {
        // The spam cap is what keeps a peer from flooding the address book.
        assert!(message.added.len() + message.dropped.len() <= pex::MAX_PEX_PEERS);
    }
    let _ = metadata::MetadataMessage::parse(data);
    let _ = extension::Handshake::parse(data);
});
