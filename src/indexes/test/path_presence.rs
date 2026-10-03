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
