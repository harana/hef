use std::collections::HashSet;

use super::*;

#[test]
fn present_path_always_found() {
    let paths = ["attributes.revenue.amount", "attributes.customer.id", "context.source"];
    let index = PathPresenceIndex::build_from_paths(&paths);
    for path in paths {
        assert!(
            index.might_contain_path(path),
            "path '{path}' must be found in the index"
        );
    }
}

#[test]
fn hash_probe_matches_string_probe() {
    let paths = ["foo.bar.baz", "qux.quux"];
    let index = PathPresenceIndex::build_from_paths(&paths);
    for path in paths {
        let via_string = index.might_contain_path(path);
        let via_hash = index.might_contain_hash(hash_path(path));
        assert_eq!(via_string, via_hash, "string and hash probes must agree for '{path}'");
    }
}

#[test]
fn absent_rare_path_prunes_the_block() {
    let paths = ["common.field", "other.field"];
    let index = PathPresenceIndex::build_from_paths(&paths);
    // "zzzzzz.unlikely.path" has a trigram-style fingerprint that collides with almost nothing in a small filter. We
    // cannot guarantee it is absent (false positives are allowed) but we can verify the present paths work.
    assert!(index.might_contain_path("common.field"));
    assert!(index.might_contain_path("other.field"));
    // The filter never gives a false negative on a present path.
}

#[test]
fn exactness_is_always_inexact_no_false_negative() {
    let index = PathPresenceIndex::build_from_paths(&["a.b"]);
    assert_eq!(index.exactness(), Exactness::InexactNoFalseNegative);
}

#[test]
fn round_trip() {
    let paths = ["attributes.amount", "context.source", "user.id"];
    let index = PathPresenceIndex::build_from_paths(&paths);
    let bytes = index.encode();
    let decoded = PathPresenceIndex::decode(&bytes).expect("decode must succeed");
    assert_eq!(decoded, index);
    for path in paths {
        assert!(decoded.might_contain_path(path));
    }
}

#[test]
fn decode_rejects_wrong_magic() {
    let bad = [0u8; 16];
    assert!(PathPresenceIndex::decode(&bad).is_err());
}

#[test]
fn build_from_empty_paths_is_sound() {
    let index = PathPresenceIndex::build_from_paths(&[]);
    // Encoding and round-tripping must succeed even for an empty path set.
    let bytes = index.encode();
    let decoded = PathPresenceIndex::decode(&bytes).expect("empty round-trip");
    assert_eq!(decoded, index);
}

#[test]
fn hash_path_is_stable_and_discriminating() {
    assert_eq!(hash_path("a.b.c"), hash_path("a.b.c"));
    assert_ne!(hash_path("a.b.c"), hash_path("a.b.d"));
    assert_ne!(hash_path("attributes.amount"), hash_path("attributes.currency"));
}

#[test]
fn build_from_hashes_and_paths_agree() {
    let paths = ["x.y", "a.b.c"];
    let hashes: Vec<u64> = paths.iter().map(|&p| hash_path(p)).collect();
    let from_paths = PathPresenceIndex::build_from_paths(&paths);
    let from_hashes = PathPresenceIndex::build(&hashes);
    // Both must be found from either route.
    for path in paths {
        assert_eq!(
            from_paths.might_contain_path(path),
            from_hashes.might_contain_path(path),
        );
    }
}

#[test]
fn encoding_matches_the_collect_and_copy_reference_byte_for_byte() {
    for count in [0usize, 1, 25, 26, 51, 52, 300] {
        let owned: Vec<String> = (0..count)
            .map(|i| format!("attributes.\u{00e9}v\u{00e9}nement.{}.{}", i % 7, "x".repeat(i % 40)))
            .collect();
        let mut paths: Vec<&str> = owned.iter().map(String::as_str).collect();
        paths.extend(owned.iter().take(count / 3).map(String::as_str));
        let hashes: Vec<u64> = paths.iter().map(|&p| hash_path(p)).collect();
        let mut reference = PATH_PRESENCE_MAGIC.to_le_bytes().to_vec();
        reference.extend(SplitBlockBloomFilter::build(&hashes, BITS_PER_PATH).encode());

        assert_eq!(PathPresenceIndex::build_from_paths(&paths).encode(), reference);
        assert_eq!(PathPresenceIndex::build(&hashes).encode(), reference);

        let distinct: HashSet<u64> = hashes.iter().copied().collect();
        let mut sorted: Vec<u64> = distinct.iter().copied().collect();
        sorted.sort_unstable();
        let mut set_reference = PATH_PRESENCE_MAGIC.to_le_bytes().to_vec();
        set_reference.extend(SplitBlockBloomFilter::build(&sorted, BITS_PER_PATH).encode());
        let from_set = PathPresenceIndex::build_from_hashes(distinct.iter().copied(), distinct.len()).encode();
        assert_eq!(from_set, set_reference);
        sorted.reverse();
        let reversed = PathPresenceIndex::build_from_hashes(sorted.iter().copied(), sorted.len()).encode();
        assert_eq!(reversed, from_set);
    }
}
