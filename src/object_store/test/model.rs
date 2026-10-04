use super::*;
use crate::events::{SequenceRange, TenantId};
use crate::lifecycle::{FileType, PartState};
use crate::typed_id::TypedIdTestExt;

fn entry(file_id: u128, part_state: PartState) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: file_id as u64 * 10,
            last_sequence: file_id as u64 * 10 + 9,
        },
        feature_metadata: None,
        file_seal: [file_id as u8; 32],
        file_id,
        file_type: FileType::HefFile,
        footer_len: Some(64),
        optional_feature_flags: 0,
        part_index: 0,
        part_state,
        required_feature_flags: 0,
        size_bytes: 1024,
        tenant_id: TenantId::new_test_id(9),
        tree_len: None,
    }
}

#[test]
fn a_delta_applied_to_its_checkpoint_gives_back_the_generation() {
    let checkpoint = ManifestGeneration {
        files: vec![
            entry(1, PartState::Active),
            entry(2, PartState::Active),
            entry(3, PartState::DeleteOnDestroy),
        ],
        generation: 64,
        ..Default::default()
    };
    let next = ManifestGeneration {
        files: vec![
            entry(1, PartState::Outdated),
            entry(2, PartState::Active),
            entry(4, PartState::Active),
        ],
        generation: 70,
        retirements: vec![Retirement {
            file_id: 1,
            generation: 70,
            since_nanos: 5,
        }],
        ..Default::default()
    };

    let delta = GenerationDelta::between(&checkpoint, &next);

    // Only the changed and new entries travel; the untouched one does not.
    assert_eq!(
        delta.upserted_files,
        vec![entry(1, PartState::Outdated), entry(4, PartState::Active)]
    );
    assert_eq!(delta.removed_file_ids, vec![3]);
    assert_eq!(delta.checkpoint, 64);
    assert_eq!(delta.apply(checkpoint), next);
}
