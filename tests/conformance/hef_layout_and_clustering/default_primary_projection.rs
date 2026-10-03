use hef::layout::clustering::clustering_preserves_sequence_coverage;

/// conformance: hef-layout-and-clustering/default-primary-projection/clustering-preserves-sequence-coverage
#[test]
fn clustering_preserves_sequence_coverage_test() {
    // When occurred_at bucket boundaries align with the sequence ordering — bucket A owns sequences 1–4, bucket B owns 5–8 — no interleaving occurs and the granule-level [min_seq, max_seq] coverage metadata stays exact. The clustering is applied.
    let non_interleaved: Vec<(u64, u32)> = vec![
        (1, 0),
        (2, 0),
        (3, 0),
        (4, 0), // bucket 0 owns sequences 1-4
        (5, 1),
        (6, 1),
        (7, 1),
        (8, 1), // bucket 1 owns sequences 5-8
    ];
    assert!(
        clustering_preserves_sequence_coverage(&non_interleaved),
        "contiguous bucket ownership: clustering is safe and should be applied"
    );

    // When two buckets interleave — A, B, A in sequence order — the bucket-grouped granule for A would span [1, 3] but sequence 2 belongs to B, making the coverage a loose approximation. The clustering is NOT applied.
    let interleaved: Vec<(u64, u32)> = vec![
        (1, 0), // bucket 0
        (2, 1), // bucket 1 — interleaved between bucket 0's rows
        (3, 0), // bucket 0 again: now bucket 0's range [1,3] has a gap at seq=2
    ];
    assert!(
        !clustering_preserves_sequence_coverage(&interleaved),
        "interleaved buckets break coverage: clustering must be rejected"
    );
}
