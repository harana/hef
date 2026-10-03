//! Checks that the encoder picks between two named strategies — one biased toward fast decode for freshly written
//! parts, the other biased toward smaller bytes for rewritten or compacted parts — based solely on the part's lifecycle
//! stage, never on a caller-supplied switch.

use hef::encoding::{CascadeStrategy, ColumnData, decode_block, encode_block, encode_block_with_strategy};

fn monotonic_u64(n: usize) -> ColumnData {
    ColumnData::U64((0..n as u64).collect())
}

/// conformance:
/// hef-encodings-and-compression/lifecycle-selected-cascade-strategies/fresh-part-uses-the-decode-optimized-strategy
#[test]
fn fresh_part_uses_the_decode_optimized_strategy() {
    // The write path selects DecodeOptimized — it is the result of CascadeStrategy::for_fresh_publication(), not a
    // caller choice.
    assert_eq!(
        CascadeStrategy::for_fresh_publication(),
        CascadeStrategy::DecodeOptimized
    );

    // encode_block (the write-path entry point) always runs DecodeOptimized.
    let data = monotonic_u64(512);
    let block = encode_block(&data, false);
    let decoded = decode_block(block.pipeline, &block.bytes).unwrap();
    assert_eq!(decoded, data, "DecodeOptimized block decodes correctly");
}

/// conformance:
/// hef-encodings-and-compression/lifecycle-selected-cascade-strategies/rewrite-uses-the-size-optimized-strategy
#[test]
fn rewrite_uses_the_size_optimized_strategy() {
    // Rewrite and compaction pass select SizeOptimized.
    assert_eq!(
        CascadeStrategy::for_rewrite_or_compaction(),
        CascadeStrategy::SizeOptimized
    );

    let data = monotonic_u64(512);
    let block = encode_block_with_strategy(&data, false, CascadeStrategy::SizeOptimized);
    let decoded = decode_block(block.pipeline, &block.bytes).unwrap();
    assert_eq!(decoded, data, "SizeOptimized block decodes correctly");
}

/// conformance:
/// hef-encodings-and-compression/lifecycle-selected-cascade-strategies/no-operator-knob-selects-the-strategy
#[test]
fn no_operator_knob_selects_the_strategy() {
    // encode_block is the caller-facing entry point for the write path. It takes no strategy parameter — strategy is
    // selected by lifecycle stage internally. The test compiles only if encode_block has the same signature as before
    // (no strategy arg added to the public API).
    let data = ColumnData::F64(vec![1.0, 2.0, 3.0]);
    let _ = encode_block(&data, false);
}

/// conformance: hef-encodings-and-compression/lifecycle-selected-cascade-strategies/strategy-selection-is-deterministic
#[test]
fn strategy_selection_is_deterministic() {
    // Two encodings of the same content under the same strategy must produce identical bytes, regardless of which node
    // runs the encoder.
    let data = ColumnData::F64(vec![1.5, 2.5, 3.5, 4.5, 5.5]);
    for strategy in [CascadeStrategy::DecodeOptimized, CascadeStrategy::SizeOptimized] {
        let a = encode_block_with_strategy(&data, false, strategy);
        let b = encode_block_with_strategy(&data, false, strategy);
        assert_eq!(a.pipeline, b.pipeline, "{strategy:?}: pipeline differs");
        assert_eq!(a.bytes, b.bytes, "{strategy:?}: bytes differ");
    }
}

/// conformance:
/// hef-encodings-and-compression/lifecycle-selected-cascade-strategies/both-strategies-round-trip-correctly
#[test]
fn both_strategies_round_trip_correctly() {
    // Encoding under either strategy and then decoding must reproduce the original values exactly.
    let datasets = [
        monotonic_u64(256),
        ColumnData::I64((-128..128_i64).collect()),
        ColumnData::F64((0..64).map(|i| i as f64 * 0.5).collect()),
        ColumnData::Decimal {
            values: (1..=32_i128).map(|i| i * 100).collect(),
            scale: 2,
        },
    ];
    for data in &datasets {
        for strategy in [CascadeStrategy::DecodeOptimized, CascadeStrategy::SizeOptimized] {
            let block = encode_block_with_strategy(data, false, strategy);
            let decoded = decode_block(block.pipeline, &block.bytes).unwrap();
            assert_eq!(&decoded, data, "{strategy:?}: round-trip failed");
        }
    }
}
