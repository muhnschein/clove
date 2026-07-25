//! Fuzz .torrent parsing, including the path validation the rest of the
//! engine trusts and the I2P-only announce filter.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(meta) = clove_core::metainfo::MetaInfo::parse(data) {
        assert!(!meta.pieces.is_empty());
        assert!(meta.piece_length > 0);
        let sum: u64 = meta.files.iter().map(|f| f.length).sum();
        assert_eq!(sum, meta.total_length);
        for file in &meta.files {
            assert!(!file.path.is_empty());
            for part in &file.path {
                // Path traversal here would let a torrent write outside its
                // own directory.
                assert!(part != "." && part != ".." && !part.contains('/'));
                assert!(!part.contains('\0'));
            }
        }
        for tier in &meta.trackers {
            for url in tier {
                assert!(url.to_ascii_lowercase().contains(".i2p"));
            }
        }
    }
    let _ = clove_core::metainfo::MetaInfo::from_info_dict(data);
});
