//! Checks that a file moves through a fixed sequence of states and can only step along the allowed edges. A file that
//! is finished on disk but not yet listed in the published catalogue is not queryable until that listing happens.
use crate::support;
use hef::events::SequenceRange;
use hef::lifecycle::{HefFileEntry, ManifestGeneration, PartState};

/// conformance: hef-file-lifecycle/defined-part-state-progression/sealed-file-not-yet-visible
#[test]
fn sealed_file_not_yet_visible() {
    // A Sealed file (valid footer and checksum) is not visible until the manifest references it; the progression edges
    // are exactly the defined ones.
    assert!(PartState::OpenTmp.can_transition_to(PartState::Sealed));
    assert!(PartState::Sealed.can_transition_to(PartState::Active));
    assert!(!PartState::OpenTmp.can_transition_to(PartState::Active));
    assert!(!PartState::Sealed.can_transition_to(PartState::OpenTmp));
    let sealed_entry = HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: 4,
        },
        feature_metadata: None,
        file_seal: [0; 32],
        file_id: 1,
        file_type: hef::lifecycle::FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Sealed,
        required_feature_flags: 0,
        size_bytes: 4096,
        tenant_id: support::tenant(),
        tree_len: None,
    };
    let generation = ManifestGeneration {
        generation: 1,
        files: vec![sealed_entry],
        ..Default::default()
    };
    assert_eq!(generation.snapshot_files().count(), 0, "Sealed is not queryable");
}
