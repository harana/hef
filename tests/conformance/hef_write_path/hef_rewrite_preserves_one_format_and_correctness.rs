//! Checks that rewriting query files into a repacked form keeps a single file format and the same data, and only
//! retires the original source files once the replacement is safely in place. The rewrite engine arrives with the
//! layout-and-clustering work; the file lifecycle states it relies on are already enforced by the state machine, and
//! that enforcement is what this
use hef::events::{SequenceRange, TenantId};
use hef::lifecycle::{HefFileEntry, ManifestGeneration, PartState};
use hef::typed_id::TypedIdTestExt;

fn tenant() -> TenantId {
    TenantId::new_test_id(9)
}

fn file_entry(state: PartState) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: 100,
        },
        feature_metadata: None,
        file_seal: [0u8; 32],
        file_id: 1,
        file_type: hef::lifecycle::FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: state,
        required_feature_flags: 0,
        size_bytes: 4096,
        tenant_id: tenant(),
        tree_len: None,
    }
}

/// conformance: hef-write-path/hef-rewrite-preserves-one-format-and-correctness/source-files-retired-safely
#[test]
fn source_files_retired_safely() {
    // Source files move through the lifecycle in one forward direction only. Active → Outdated only after the
    // replacement generation is published; Outdated → DeleteOnDestroy only after the safety window and in-flight query
    // horizon expire. No edges may be skipped.

    // Legal retirement path.
    assert!(PartState::Active.can_transition_to(PartState::Outdated));
    assert!(PartState::Outdated.can_transition_to(PartState::DeleteOnDestroy));
    assert!(PartState::DeleteOnDestroy.can_transition_to(PartState::Deleted));

    // Skipping Outdated is forbidden: files must remain readable for in-flight snapshots before they can be scheduled
    // for destruction.
    assert!(!PartState::Active.can_transition_to(PartState::DeleteOnDestroy));
    assert!(!PartState::Active.can_transition_to(PartState::Deleted));
    assert!(!PartState::Outdated.can_transition_to(PartState::Deleted));

    // Only Active files join new query snapshots.
    assert!(PartState::Active.selectable_for_new_snapshots());
    assert!(!PartState::Outdated.selectable_for_new_snapshots());
    assert!(!PartState::DeleteOnDestroy.selectable_for_new_snapshots());
    assert!(!PartState::Deleted.selectable_for_new_snapshots());

    // Before replacement: source file is Active and visible to new snapshots.
    let mut manifest = ManifestGeneration {
        files: vec![file_entry(PartState::Active)],
        generation: 1,
        ..Default::default()
    };
    assert_eq!(manifest.snapshot_files().count(), 1);

    // After replacement published: source file is Outdated and hidden from new snapshots, but still counted as covering
    // the range for in-flight queries, LiveOverlay eviction, and HEJ retention gating.
    manifest.files[0].part_state = PartState::Outdated;
    let sub_range = SequenceRange {
        epoch: 1,
        first_sequence: 10,
        last_sequence: 50,
    };
    assert_eq!(
        manifest.snapshot_files().count(),
        0,
        "outdated file hidden from new snapshots"
    );
    assert!(
        manifest.covers(&sub_range, tenant()),
        "outdated file still covers in-flight queries"
    );

    // After safety window: file is scheduled for sweeper deletion but its manifest entry stays for bookkeeping; still
    // not selected for new snapshots.
    manifest.files[0].part_state = PartState::DeleteOnDestroy;
    assert_eq!(manifest.snapshot_files().count(), 0);
    assert!(
        manifest.covers(&sub_range, tenant()),
        "deleteondestroy file still covers until swept"
    );

    // Once deleted, the file is gone and no longer covers any range.
    manifest.files[0].part_state = PartState::Deleted;
    assert_eq!(manifest.snapshot_files().count(), 0);
    assert!(!manifest.covers(&sub_range, tenant()));
}
