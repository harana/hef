use super::*;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn parallel_hash_is_byte_identical_to_single_threaded() {
    for len in [0usize, 1, 1023, 1024, 1025, 4096, 100_003, 2_000_000] {
        let bytes = pattern(len);
        assert_eq!(hash_tree(&bytes), blake3::hash(&bytes), "default path, len {len}");
    }
}

#[test]
fn flat_segment_hashing_matches_the_single_threaded_hash_for_every_piece_shape() {
    const PIECE: usize = PARALLEL_HASH_MIN_LEN;
    let inputs: Vec<Vec<u8>> = [
        0,
        1,
        PIECE - 1,
        PIECE,
        PIECE + 1,
        2 * PIECE,
        3 * PIECE + 7,
        5 * PIECE + PIECE / 2,
    ]
    .into_iter()
    .map(pattern)
    .collect();
    let segments: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
    let hashes = hash_segments(&segments);
    assert_eq!(hashes.len(), segments.len());
    for (segment, hash) in segments.iter().zip(&hashes) {
        assert_eq!(hash, &blake3::hash(segment), "len {}", segment.len());
    }
    assert!(hash_segments(&[]).is_empty());
}

#[test]
fn crc64_locks_the_nvme_variant_and_detects_change() {
    // The CRC-64/NVME check value for the canonical "123456789" input.
    assert_eq!(crc64_nvme(b"123456789"), 0xae8b_1486_0a79_9888);
    assert!(crc64_matches(b"123456789", 0xae8b_1486_0a79_9888));
    assert!(!crc64_matches(b"123456788", 0xae8b_1486_0a79_9888));
}

#[test]
fn independently_streamed_crc_ranges_compose_without_rehashing() {
    let prefix = pattern(4_097);
    let suffix = pattern(128_003);
    let mut prefix_crc = StreamingCrc64Nvme::new();
    prefix_crc.update(&prefix);
    let mut suffix_crc = StreamingCrc64Nvme::new();
    suffix_crc.update(&suffix);

    prefix_crc.combine(&suffix_crc);

    let mut joined = prefix;
    joined.extend_from_slice(&suffix);
    assert_eq!(prefix_crc.finalize(), crc64_nvme(&joined));
}

#[test]
fn verified_range_returns_only_the_requested_bytes() {
    let group = 1024;
    let mut content = pattern(4092);
    content.extend_from_slice(b"MAGC");
    let root = *hash_tree(&content).as_bytes();
    let object = attach_outboard_tree(content.clone(), group);
    assert!(object.len() > content.len(), "a tree should be attached");

    let got = verify_object_range(&object, &root, 100, 50, group, b"MAGC").unwrap();
    assert_eq!(got, &content[100..150]);
}

#[test]
fn one_traversal_builds_the_standard_root_and_verifies_a_fetched_slice() {
    let group = 1024;
    let content = pattern(5 * group + 17);
    let built = build_outboard_tree_and_root(&content, group);
    assert_eq!(built.root, *blake3::hash(&content).as_bytes());
    let tree = built.tree.expect("multi-group input has proof nodes");

    // Request bytes crossing a group boundary, but fetch only the two complete groups needed to prove them.
    let requested_start = group as u64 - 9;
    let requested_len = 37u64;
    let fetched_start = 0u64;
    let fetched = &content[..2 * group];
    let verified = verify_range_from_slice(
        content.len() as u64,
        fetched,
        fetched_start,
        &tree,
        &built.root,
        requested_start,
        requested_len,
        group,
    )
    .unwrap();
    assert_eq!(verified, &content[group - 9..group - 9 + 37]);

    let mut corrupt = fetched.to_vec();
    corrupt[group] ^= 1;
    assert!(
        verify_range_from_slice(
            content.len() as u64,
            &corrupt,
            fetched_start,
            &tree,
            &built.root,
            requested_start,
            requested_len,
            group,
        )
        .is_err()
    );
}

#[test]
fn a_zero_length_range_still_proves_the_object_against_its_root() {
    // A zero-length range overlaps no chunk group, so the tree proves nothing about the content. Serving it as
    // "verified" would let a caller treat a corrupt cached object as checked; the empty slice has to be earned by
    // whole-content verification instead.
    let group = 1024;
    let mut content = pattern(4092);
    content.extend_from_slice(b"MAGC");
    let root = *hash_tree(&content).as_bytes();
    let object = attach_outboard_tree(content.clone(), group);
    assert!(object.len() > content.len(), "a tree should be attached");

    assert_eq!(verify_object_range(&object, &root, 0, 0, group, b"MAGC").unwrap(), b"");

    let mut corrupt = object;
    corrupt[100] ^= 0xFF; // flip a content byte, leaving the tree intact
    assert!(verify_object_range(&corrupt, &root, 0, 0, group, b"MAGC").is_err());

    let wrong_root = [0u8; 32];
    let sound = attach_outboard_tree(content, group);
    assert!(verify_object_range(&sound, &wrong_root, 0, 0, group, b"MAGC").is_err());
}

#[test]
fn corrupt_content_is_rejected_through_the_tree() {
    let group = 1024;
    let mut content = pattern(4092);
    content.extend_from_slice(b"MAGC");
    let root = *hash_tree(&content).as_bytes();
    let mut object = attach_outboard_tree(content, group);
    object[100] ^= 0xFF; // flip a content byte, leaving the tree intact

    assert!(verify_object_range(&object, &root, 64, 64, group, b"MAGC").is_err());
}

#[test]
fn small_content_has_no_tree_and_verifies_whole() {
    let group = 1024;
    let mut content = vec![5u8; group - 4];
    content.extend_from_slice(b"MAGC");
    let root = *hash_tree(&content).as_bytes();
    let object = attach_outboard_tree(content.clone(), group);
    assert_eq!(object, content, "content at one group needs no tree");

    let got = verify_object_range(&object, &root, 0, 10, group, b"MAGC").unwrap();
    assert_eq!(got, &content[0..10]);

    let mut corrupt = content;
    corrupt[0] ^= 0xFF;
    assert!(verify_object_range(&corrupt, &root, 0, 10, group, b"MAGC").is_err());
}

#[test]
fn a_damaged_outboard_trailer_still_serves_intact_content() {
    // The outboard tree and its trailer are non-authoritative metadata: BLAKE3 over the content is what proves the
    // bytes. Damage confined to the trailer used to surface as a codec error before the whole-content fallback could
    // run, making perfectly good content unreadable (issue #9740).
    const MAGIC: [u8; 4] = *b"HEFC";
    let mut content = pattern(300_000);
    let end = content.len() - 4;
    content[end..].copy_from_slice(&MAGIC);
    let root = *blake3::hash(&content).as_bytes();
    let group = 1 << 16;
    let object = attach_outboard_tree(content.clone(), group);
    assert!(object.len() > content.len(), "the object carries a tree");

    // Undamaged: the tree serves the range.
    assert_eq!(
        verify_object_range(&object, &root, 100, 5000, group, &MAGIC).unwrap(),
        &content[100..5100]
    );

    // The declared tree length is corrupted so it runs past the object: the split cannot be trusted at all, and the
    // content boundary has to be recovered from the object's own length.
    let mut damaged = object.clone();
    let len_at = damaged.len() - TREE_TRAILER_LEN;
    damaged[len_at..len_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        verify_object_range(&damaged, &root, 100, 5000, group, &MAGIC).unwrap(),
        &content[100..5100],
        "intact content stays readable through a damaged trailer length"
    );

    // A trailer length that still points inside the object, but at the wrong place, fails the content-magic check.
    let mut shifted = object.clone();
    shifted[len_at..len_at + 8].copy_from_slice(&64u64.to_le_bytes());
    assert_eq!(
        verify_object_range(&shifted, &root, 100, 5000, group, &MAGIC).unwrap(),
        &content[100..5100]
    );

    // Corruption in the content itself is still refused — the fallback never lowers the bar.
    let mut rotted = object.clone();
    rotted[500] ^= 0xFF;
    assert!(verify_object_range(&rotted, &root, 100, 5000, group, &MAGIC).is_err());
}

#[test]
fn many_large_hashes_at_once_stay_correct_without_spawning_per_hash_threads() {
    // Each large hash used to build its own tree of scoped OS threads, so concurrent uploads multiplied into hundreds
    // of them and `Scope::spawn` panicked once the OS refused another (issue #8896). The work now runs on the shared
    // rayon pool; running many at once must still be correct.
    let bytes: Vec<Vec<u8>> = (0..8).map(|n| pattern((1 << 20) + n * 7919)).collect();
    let expected: Vec<blake3::Hash> = bytes.iter().map(|b| blake3::hash(b)).collect();
    std::thread::scope(|scope| {
        for (input, want) in bytes.iter().zip(&expected) {
            scope.spawn(move || assert_eq!(&hash_tree(input), want));
        }
    });
}
