//! Checks that columns are sorted by when their value can be computed. Values known at the moment the file is finalized
//! (Tier A) are stored right in the base file; values that only become known later, or can change (Tier B, such as
//! cross-event anomaly scores), live in separate sibling files.
use hef::events::families::{ColumnFamily, Tier};

/// conformance:
/// hef-logical-event-model/column-families-split-by-temporal-computability/anomaly-score-arrives-days-after-seal
#[test]
fn anomaly_score_arrives_days_after_seal() {
    // Revenue-anomaly columns are Tier B: they are cross-event, asynchronous, revisable model outputs that cannot be
    // computed at seal time. They are therefore classified to live in a sibling derived-columns file, never in the
    // immutable base file.
    assert_eq!(ColumnFamily::RevenueAnomalyColumns.tier(), Tier::B);
    assert!(!ColumnFamily::RevenueAnomalyColumns.lives_in_base_file());
    // All three Tier B families share this classification.
    assert_eq!(ColumnFamily::ClusterColumns.tier(), Tier::B);
    assert_eq!(ColumnFamily::DriverAndCauseColumns.tier(), Tier::B);
}

/// conformance:
/// hef-logical-event-model/column-families-split-by-temporal-computability/per-event-embedding-computed-at-seal
#[test]
fn per_event_embedding_computed_at_seal() {
    // Per-event-deterministic derivations are Tier A: computable at seal time and stored HEF-native in the base file.
    assert_eq!(ColumnFamily::EmbeddingColumnsInternal.tier(), Tier::A);
    assert!(ColumnFamily::EmbeddingColumnsInternal.lives_in_base_file());
    // The cross-event, revisable families are the only Tier B ones.
    let tier_b: Vec<ColumnFamily> = ColumnFamily::ALL
        .into_iter()
        .filter(|family| family.tier() == Tier::B)
        .collect();
    assert_eq!(
        tier_b,
        vec![
            ColumnFamily::ClusterColumns,
            ColumnFamily::RevenueAnomalyColumns,
            ColumnFamily::DriverAndCauseColumns,
        ]
    );
}
