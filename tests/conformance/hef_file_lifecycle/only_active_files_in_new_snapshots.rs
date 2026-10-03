//! Checks that a freshly started query only ever picks up files marked Active. After a rewrite replaces a file, the old
//! one becomes Outdated so new queries skip it, yet it stays referenced and readable for queries already in flight that
//! had selected it.
use crate::support;
use hef::events::SequenceRange;
use hef::lifecycle::{HefFileEntry, ManifestGeneration, PartState};

/// conformance: hef-file-lifecycle/only-active-files-in-new-snapshots/outdated-file-after-replacement
#[test]
fn outdated_file_after_replacement() {
    // After a rewrite the superseded file is Outdated: new snapshots do not select it, while it remains referenced
    // (readable) for in-flight snapshots that already selected it.
    let coverage = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 8,
    };
    let outdated = HefFileEntry {
        coverage,
        feature_metadata: None,
        file_seal: [0; 32],
        file_id: 1,
        file_type: hef::lifecycle::FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Outdated,
        required_feature_flags: 0,
        size_bytes: 4096,
        tenant_id: support::tenant(),
        tree_len: None,
    };
    let replacement = HefFileEntry {
        file_id: 2,
        part_state: PartState::Active,
        ..outdated.clone()
    };
    let generation = ManifestGeneration {
        generation: 2,
        files: vec![outdated, replacement],
        ..Default::default()
    };
    let selected: Vec<u128> = generation.snapshot_files().map(|entry| entry.file_id).collect();
    assert_eq!(selected, vec![2], "new snapshots select only Active files");
    assert!(
        generation.files.iter().any(|entry| entry.file_id == 1),
        "the Outdated file stays referenced for in-flight snapshots"
    );
    assert!(PartState::Active.can_transition_to(PartState::Outdated));
    assert!(PartState::Outdated.can_transition_to(PartState::DeleteOnDestroy));
}
