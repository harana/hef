use super::*;
use crate::events::{SequenceRange, TenantId};
use crate::indexes::artifact::ArtifactKind;
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
        graph_event_id_filter: None,
        graph_summary: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state,
        profile: crate::layout::Profile::Event,
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

fn artifact(object_key: &str, column_id: u32) -> IndexArtifactRef {
    IndexArtifactRef {
        artifact_blake3: [column_id as u8; 32],
        column_id,
        covered_file_id: 1,
        kind: ArtifactKind::PathPresence,
        object_key: object_key.to_owned(),
    }
}

#[test]
fn artifact_delta_borrows_keys_and_matches_the_owned_key_diff() {
    let long = "k".repeat(4096);
    let base = vec![
        artifact("kept", 1),
        artifact("changed", 1),
        artifact("gone", 1),
        artifact("\u{00e9}t\u{00e9}", 1),
        artifact("", 1),
        artifact(&long, 1),
        artifact("dup", 1),
        artifact("dup", 2),
        artifact("gone_twice", 1),
        artifact("gone_twice", 2),
    ];
    let next = vec![
        artifact("new", 1),
        artifact(&long, 1),
        artifact("changed", 2),
        artifact("", 1),
        artifact("kept", 1),
        artifact("dup", 2),
        artifact("dup", 1),
        artifact("\u{00e9}t\u{00e9}", 3),
    ];
    let checkpoint = ManifestGeneration {
        generation: 10,
        index_artifacts: base.clone(),
        ..Default::default()
    };
    let generation = ManifestGeneration {
        generation: 11,
        index_artifacts: next.clone(),
        ..Default::default()
    };

    let delta = GenerationDelta::between(&checkpoint, &generation);
    let (owned_added, owned_removed) = diff(&base, &next, |artifact| artifact.object_key.clone());

    assert_eq!(delta.added_artifacts, owned_added);
    assert_eq!(delta.removed_artifact_keys, owned_removed);
    assert_eq!(
        delta.added_artifacts,
        vec![
            artifact("new", 1),
            artifact("changed", 2),
            artifact("dup", 1),
            artifact("\u{00e9}t\u{00e9}", 3),
        ]
    );
    assert_eq!(delta.removed_artifact_keys, vec!["gone", "gone_twice", "gone_twice"]);

    let unique_checkpoint = ManifestGeneration {
        generation: 10,
        index_artifacts: vec![artifact("a", 1), artifact("b", 1), artifact("c", 1)],
        ..Default::default()
    };
    let unique_next = ManifestGeneration {
        generation: 11,
        index_artifacts: vec![artifact("a", 1), artifact("c", 2), artifact("d", 1)],
        ..Default::default()
    };
    let unique_delta = GenerationDelta::between(&unique_checkpoint, &unique_next);
    assert_eq!(unique_delta.apply(unique_checkpoint), unique_next);
}
