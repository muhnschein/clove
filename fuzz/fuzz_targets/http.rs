//! Fuzz both ends of the hand-rolled HTTP/1.1 code: tracker replies coming
//! in over an I2P stream, and API requests arriving on the control socket.
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let _ = clove_core::http::read_response(&mut Cursor::new(data), 64 * 1024);
    let _ = clove_core::http::read_request(&mut Cursor::new(data), 64 * 1024);
    let _ = clove_core::http::percent_decode(&String::from_utf8_lossy(data));
});
