//! Fuzz the bencode codec: the parser under every torrent, resume file and
//! tracker reply. Asserts the round-trip property as well as no-panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fn count_nodes(value: &clove_core::bencode::Value) -> usize {
    use clove_core::bencode::Value;
    match value {
        Value::Bytes(_) | Value::Int(_) => 1,
        Value::List(items) => 1 + items.iter().map(count_nodes).sum::<usize>(),
        Value::Dict(map) => 1 + map.values().map(count_nodes).sum::<usize>(),
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = clove_core::bencode::decode(data) {
        let reencoded = clove_core::bencode::encode(&value);
        let again = clove_core::bencode::decode(&reencoded)
            .expect("re-encoded bencode must decode");
        assert_eq!(again, value, "bencode round trip disagreed");
        assert!(
            count_nodes(&value) <= clove_core::bencode::node_budget(data.len()),
            "more values than the node budget"
        );
    }
    let _ = clove_core::bencode::decode_prefix(data);
});
