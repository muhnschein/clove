//! Fuzz .torrent parsing, including the path validation the rest of the
//! engine trusts and the I2P-only announce filter.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(meta) = clove_core::metainfo::MetaInfo::parse(data) {
        assert!(!meta.pieces.is_empty());
        assert!(meta.piece_length > 0);
        // The parse-time caps: descriptors, bitfield size, announce cycle.
        assert!(meta.files.len() <= clove_core::metainfo::MAX_FILES);
        assert!(meta.pieces.len() <= clove_core::metainfo::MAX_PIECES as usize);
        assert!(
            meta.trackers.iter().map(Vec::len).sum::<usize>()
                <= clove_core::metainfo::MAX_TRACKERS
        );
        let sum: u64 = meta.files.iter().map(|f| f.length).sum();
        assert_eq!(sum, meta.total_length);
        // Paths must be distinct and non-shadowing: two entries on one path
        // alias the same bytes on disk and the pieces over them never verify.
        let mut paths: Vec<&[String]> = meta.files.iter().map(|f| f.path.as_slice()).collect();
        paths.sort_unstable();
        for pair in paths.windows(2) {
            assert_ne!(pair[0], pair[1], "two files share a path");
            assert!(!pair[1].starts_with(pair[0]), "a file path is a directory");
        }
        for file in &meta.files {
            assert!(!file.path.is_empty());
            for part in &file.path {
                // Path traversal here would let a torrent write outside its
                // own directory.
                assert!(part != "." && part != ".." && !part.contains('/'));
                assert!(!part.contains('\0'));
                assert!(part.len() <= clove_core::metainfo::MAX_COMPONENT_BYTES);
            }
        }
        for tier in &meta.trackers {
            for url in tier {
                assert!(url.to_ascii_lowercase().contains(".i2p"));
                // The filter and the announce builder must agree exactly: a
                // URL kept here is one the announcer will be handed, and one
                // it cannot build is a tracker that fails on every attempt for
                // the life of the torrent, far from the URL that caused it.
                // They disagreed once, each having its own parser.
                let params = clove_core::tracker::AnnounceParams {
                    info_hash: meta.info_hash.0,
                    peer_id: *b"-CV0001-fuzzfuzzfuzz",
                    uploaded: 0,
                    downloaded: 0,
                    left: 0,
                    event: clove_core::tracker::Event::Started,
                    numwant: 30,
                    our_dest_b64: "DESTINATION",
                };
                let (host, request) = clove_core::tracker::build_announce(url, &params)
                    .expect("metainfo kept a URL that build_announce refuses");

                // Whatever the torrent said, what goes on the wire is exactly
                // the request we meant to write: a request line, Host,
                // User-Agent, Connection. A torrent-supplied path carrying a
                // CRLF would append headers of its choosing to an announce we
                // send under our own name, and the count is what notices.
                let text = String::from_utf8(request.clone()).expect("ascii request");
                let head = text.split("\r\n\r\n").next().unwrap_or(&text);
                assert_eq!(
                    head.split("\r\n").count(),
                    4,
                    "the request grew a line: {head:?}"
                );
                assert!(!host.contains(['\r', '\n', ' ']), "host {host:?}");

                // And the destination we send is never echoed back into what
                // gets logged when the announce fails. Asserted as "the ip
                // parameter is redacted" rather than "the sentinel is absent
                // from the line": the *path* comes from the torrent, so a
                // torrent announcing to `/DESTINATION` would fail the second
                // form while nothing was wrong.
                let logged = clove_core::tracker::announced_url(&host, &request);
                assert!(logged.contains("ip=<redacted>"), "{logged}");
                assert!(!logged.contains('\r') && !logged.contains('\n'), "{logged}");
            }
        }
    }
    let _ = clove_core::metainfo::MetaInfo::from_info_dict(data);
});
