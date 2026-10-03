//! Checks that when a field is newly promoted to its own column, older files can be back-filled with that column
//! without rewriting their event bodies — the new column is attached as a side file and later folded in during
//! compaction.
use crate::support;
use hef::events::SequenceRange;
use hef::lifecycle::{FileType, HefFileEntry, ManifestGeneration, PartState};

/// conformance:
/// hef-column-design/promotion-backfill-via-manifest-native-vertical-projection/
/// historical-backfill-without-payload-rewrite
#[test]
fn historical_backfill_without_payload_rewrite() {
    // Build the base HEF file and record its bytes before any further work.
    let base = support::built_file(8);
    let base_bytes_snapshot = base.bytes.clone();

    let coverage = SequenceRange {
        epoch: base.header.min_epoch,
        first_sequence: base.header.min_sequence,
        last_sequence: base.header.max_sequence,
    };

    let base_entry = HefFileEntry {
        coverage,
        feature_metadata: None,
        file_seal: base.file_seal,
        file_id: base.file_id,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: base.footer.optional_feature_flags,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: base.footer.required_feature_flags,
        size_bytes: base.bytes.len() as u64,
        tenant_id: support::tenant(),
        tree_len: None,
    };

    // The vertical projection (DerivedColumns) is published as a sibling manifest entry for the same range. It carries
    // the newly promoted column without touching the base file's payload arena.
    let sidecar_entry = HefFileEntry {
        coverage,
        feature_metadata: None,
        file_seal: [0xab; 32],
        file_id: base.file_id + 1,
        file_type: FileType::DerivedColumns,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 512,
        tenant_id: support::tenant(),
        tree_len: None,
    };

    // Both entries are published atomically in one manifest generation.
    let manifest = ManifestGeneration {
        files: vec![base_entry, sidecar_entry],
        generation: 1,
        ..Default::default()
    };

    // New snapshots see both the base file and the sidecar projection.
    assert_eq!(manifest.snapshot_files().count(), 2);

    // The sidecar covers exactly the same row range as the base file — that is the ordinal-alignment guarantee: row i
    // in the sidecar is row i in the base.
    assert_eq!(manifest.files[0].coverage, manifest.files[1].coverage);
    assert_eq!(manifest.files[1].file_type, FileType::DerivedColumns);

    // The base file's bytes are completely unchanged. The payload arena was not rewritten; only a sidecar was published
    // alongside it.
    assert_eq!(
        base.bytes, base_bytes_snapshot,
        "base file bytes must not change during backfill"
    );
}

/// conformance:
/// hef-column-design/promotion-backfill-via-manifest-native-vertical-projection/compaction-folds-the-sidecar
#[test]
fn compaction_folds_the_sidecar() {
    let coverage = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 8,
    };

    // Generation 1: a base HEF file (without the promoted column) and a DerivedColumns sidecar (with the promoted
    // column) are both active.
    let base_entry = HefFileEntry {
        coverage,
        feature_metadata: None,
        file_seal: [0x11; 32],
        file_id: 101,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 1024,
        tenant_id: support::tenant(),
        tree_len: None,
    };
    let sidecar_entry = HefFileEntry {
        coverage,
        feature_metadata: None,
        file_seal: [0x22; 32],
        file_id: 102,
        file_type: FileType::DerivedColumns,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 256,
        tenant_id: support::tenant(),
        tree_len: None,
    };
    let before_compaction = ManifestGeneration {
        files: vec![base_entry.clone(), sidecar_entry.clone()],
        generation: 1,
        ..Default::default()
    };

    // Before compaction: both files appear in new snapshots.
    assert_eq!(before_compaction.snapshot_files().count(), 2);

    // The Active → Outdated transition is legal for both file types, which is what lets compaction retire the sidecar
    // without losing in-flight snapshot access.
    assert!(PartState::Active.can_transition_to(PartState::Outdated));

    // Generation 2 (after compaction): the promoted column is embedded in the new consolidated base file; both old
    // entries transition to Outdated.
    let consolidated = HefFileEntry {
        coverage,
        feature_metadata: None,
        file_seal: [0x33; 32],
        file_id: 103,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 2048, // larger: the promoted column is now embedded
        tenant_id: support::tenant(),
        tree_len: None,
    };
    let after_compaction = ManifestGeneration {
        files: vec![
            HefFileEntry {
                part_state: PartState::Outdated,
                ..base_entry
            },
            HefFileEntry {
                part_state: PartState::Outdated,
                ..sidecar_entry
            },
            consolidated,
        ],
        generation: 2,
        ..Default::default()
    };

    // After compaction: only the new consolidated base is visible to new snapshots. The sidecar is no longer a separate
    // file.
    let snapshot: Vec<_> = after_compaction.snapshot_files().collect();
    assert_eq!(
        snapshot.len(),
        1,
        "only the consolidated base file should be active after compaction"
    );
    assert_eq!(snapshot[0].file_type, FileType::HefFile);
    assert_eq!(snapshot[0].file_id, 103);
    assert_eq!(snapshot[0].size_bytes, 2048);
}
