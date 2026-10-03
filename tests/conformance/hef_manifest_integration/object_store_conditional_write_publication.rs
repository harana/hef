//! Conformance tests for object-store conditional-write publication.
//!
//! Manifest publication uses create-only PUT for generation objects and If-Match CAS for the head pointer. A publisher
//! that loses the race re-reads the latest generation, rebases its change on top, and retries. It never overwrites the
//! winner's generation object.

use hef::error::PublishError;
use hef::events::{SequenceRange, TenantId};
use hef::invariants::PublishedSet;
use hef::invariants::sim::SimulatedPublishedSet;
use hef::lifecycle::{FileType, HefFileEntry, ManifestGeneration, PartState};
use hef::typed_id::TypedIdTestExt;

fn tenant() -> TenantId {
    TenantId::new_test_id(13)
}

fn make_entry(file_id: u128, range: SequenceRange) -> HefFileEntry {
    HefFileEntry {
        coverage: range,
        feature_metadata: None,
        file_seal: [0u8; 32],
        file_id,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 1024,
        tenant_id: tenant(),
        tree_len: None,
    }
}

/// conformance:
/// hef-manifest-integration/object-store-conditional-write-publication/concurrent-publishers-race-on-the-pointer
#[test]
fn concurrent_publishers_race_on_the_pointer() {
    // Two publishers race to advance the head. Publisher B (the winner) claims generation 1 first. Publisher A (the
    // loser) detects the conflict, re-reads the new head, rebases its entry onto generation 1, and publishes as
    // generation 2. The winner's generation 1 object must never be overwritten.
    let range_a = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 10,
    };
    let range_b = SequenceRange {
        epoch: 1,
        first_sequence: 11,
        last_sequence: 20,
    };
    let entry_a = make_entry(0xAA, range_a);
    let entry_b = make_entry(0xBB, range_b);

    let mut store = SimulatedPublishedSet::new();
    let (head, _) = store.head().unwrap();

    // Publisher B wins generation 1.
    let gen1 = ManifestGeneration {
        generation: head + 1,
        files: vec![entry_b.clone()],
        ..Default::default()
    };
    store.put_generation(gen1).unwrap();
    store.advance_head(head, head + 1).unwrap();

    // Publisher A tries to create the same generation id — create-only put fails.
    let gen1_loser = ManifestGeneration {
        generation: 1,
        files: vec![entry_a.clone()],
        ..Default::default()
    };
    let err = store.put_generation(gen1_loser).unwrap_err();
    assert!(
        matches!(err, PublishError::GenerationExists),
        "create-only put must fail with GenerationExists when the id is taken"
    );

    // Publisher A rebases: re-reads the current head, builds generation 2 on top.
    let (new_head, latest) = store.head().unwrap();
    assert_eq!(new_head, 1, "head must have advanced to the winner's generation");
    let mut gen2_files = latest.files.clone();
    gen2_files.push(entry_a);
    let gen2 = ManifestGeneration {
        generation: new_head + 1,
        files: gen2_files,
        ..Default::default()
    };
    store.put_generation(gen2).unwrap();
    store.advance_head(new_head, new_head + 1).unwrap();

    // The winner's generation 1 object is intact.
    let winner_gen = store.generation(1).unwrap();
    assert_eq!(winner_gen.files.len(), 1);
    assert_eq!(
        winner_gen.files[0].file_id, 0xBB,
        "winner's generation must not be overwritten"
    );

    // Generation 2 is the rebased result: it contains both files.
    let final_gen = store.generation(2).unwrap();
    assert_eq!(final_gen.files.len(), 2, "rebased generation must include both entries");
    assert_eq!(store.head().unwrap().0, 2);
}
