use super::*;
use crate::events::{SequenceRange, TenantId};
use crate::lifecycle::{FileType, HefFileEntry, PartState};
use crate::object_store::constant::CHECKPOINT_INTERVAL;
use crate::object_store::sim::SimObjectStore;
use crate::typed_id::TypedIdTestExt;

const PREFIX: &str = "catalogue";

fn entry(file_id: u128) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: file_id as u64 * 10,
            last_sequence: file_id as u64 * 10 + 9,
        },
        feature_metadata: None,
        file_seal: [7; 32],
        file_id,
        file_type: FileType::HefFile,
        footer_len: Some(64),
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 1024,
        tenant_id: TenantId::new_test_id(9),
        tree_len: None,
    }
}

fn store() -> Arc<SimObjectStore> {
    Arc::new(SimObjectStore::new())
}

/// Reads the head, adds `file_id`, and publishes it as the next generation, rebasing on every lost race.
fn publish(set: &mut LivePublishedSet, file_id: u128) -> u64 {
    loop {
        let (head_id, mut next) = set.head().unwrap();
        next.generation = head_id + 1;
        next.files.push(entry(file_id));
        set.put_generation(next).unwrap();
        match set.advance_head(head_id, head_id + 1) {
            Ok(()) => return head_id + 1,
            Err(PublishError::CasLost { .. }) => continue,
            Err(error) => panic!("unexpected publish error {error:?}"),
        }
    }
}

#[test]
fn an_empty_store_reads_as_generation_zero() {
    let set = LivePublishedSet::new(store(), PREFIX);
    assert_eq!(set.head().unwrap(), (0, ManifestGeneration::default()));
}

#[test]
fn the_loser_of_a_head_race_reports_the_winner_and_rebases() {
    let shared = store();
    let mut first = LivePublishedSet::new(shared.clone(), PREFIX);
    let mut second = LivePublishedSet::new(shared, PREFIX);

    // Both publishers read the same head.
    let (first_head, mut first_next) = first.head().unwrap();
    let (second_head, mut second_next) = second.head().unwrap();
    assert_eq!(first_head, second_head);

    // The first wins generation 1 and moves the pointer.
    first_next.generation = 1;
    first_next.files.push(entry(1));
    first.put_generation(first_next).unwrap();
    first.advance_head(0, 1).unwrap();

    // The second cannot write generation 1 again, and cannot move the pointer from the head it read.
    second_next.generation = 1;
    second_next.files.push(entry(2));
    assert_eq!(
        second.put_generation(second_next.clone()),
        Err(PublishError::GenerationExists)
    );
    assert_eq!(
        second.advance_head(0, 1),
        Err(PublishError::CasLost { current_generation: 1 })
    );

    // It rebases onto the winner and retries; the winner's generation is never overwritten.
    let (head_id, mut rebased) = second.head().unwrap();
    assert_eq!(head_id, 1);
    rebased.generation = 2;
    rebased.files.push(entry(2));
    second.put_generation(rebased).unwrap();
    second.advance_head(1, 2).unwrap();

    let (head_id, head) = first.head().unwrap();
    assert_eq!(head_id, 2);
    assert_eq!(head.files, vec![entry(1), entry(2)]);
    assert_eq!(first.generation(1).unwrap().files, vec![entry(1)]);
}

#[test]
fn a_cached_etag_that_went_stale_loses_the_cas() {
    let shared = store();
    let mut stale = LivePublishedSet::new(shared.clone(), PREFIX);
    let mut fresh = LivePublishedSet::new(shared, PREFIX);
    publish(&mut stale, 1);
    // `stale` remembers the pointer at generation 1; `fresh` moves it to 2 behind its back.
    publish(&mut fresh, 2);

    let mut next = stale.generation(1).unwrap();
    next.generation = 2;
    assert_eq!(stale.put_generation(next), Err(PublishError::GenerationExists));
    assert_eq!(
        stale.advance_head(1, 2),
        Err(PublishError::CasLost { current_generation: 2 })
    );
}

#[test]
fn a_create_only_put_of_an_existing_generation_is_refused() {
    let mut set = LivePublishedSet::new(store(), PREFIX);
    publish(&mut set, 1);
    let mut again = set.generation(1).unwrap();
    again.files.push(entry(99));
    assert_eq!(set.put_generation(again), Err(PublishError::GenerationExists));
    assert_eq!(set.generation(1).unwrap().files, vec![entry(1)]);
}

#[test]
fn advancing_to_a_generation_that_was_never_written_is_refused() {
    let mut set = LivePublishedSet::new(store(), PREFIX);
    assert_eq!(set.advance_head(0, 1), Err(PublishError::UnknownGeneration));
}

#[test]
fn generation_objects_stay_small_as_the_file_count_grows() {
    let shared = store();
    let mut set = LivePublishedSet::new(shared.clone(), PREFIX);
    let generations = 3 * CHECKPOINT_INTERVAL;
    for file_id in 1..=u128::from(generations) {
        publish(&mut set, file_id);
    }
    let size = |id: u64| shared.object(&format!("{PREFIX}/generations/{id:020}")).unwrap().len();

    // Deltas just after each checkpoint carry one entry however many files the catalogue already holds.
    let early_delta = size(CHECKPOINT_INTERVAL + 1);
    let late_delta = size(2 * CHECKPOINT_INTERVAL + 1);
    assert!(
        late_delta <= early_delta + 64,
        "delta grew with the file count: {early_delta} -> {late_delta}"
    );
    // A full copy of the catalogue is many times larger than a delta.
    let checkpoint = size(2 * CHECKPOINT_INTERVAL);
    assert!(
        late_delta * 16 < checkpoint,
        "delta {late_delta} not small next to checkpoint {checkpoint}"
    );
}

#[test]
fn a_reader_resolves_the_same_files_from_checkpoint_and_deltas() {
    let mut set = LivePublishedSet::new(store(), PREFIX);
    let mut expected = Vec::new();
    for id in 1..=CHECKPOINT_INTERVAL + 5 {
        let (head_id, head) = set.head().unwrap();
        let mut next = ManifestGeneration {
            generation: head_id + 1,
            ..head
        };
        // Mix every kind of change: add a file, retire an older one, and drop one that was swept.
        next.files.push(entry(u128::from(id)));
        if id % 3 == 0
            && let Some(older) = next.files.iter_mut().find(|file| file.part_state == PartState::Active)
        {
            older.part_state = PartState::Outdated;
        }
        if id % 7 == 0 {
            next.files.remove(0);
        }
        set.put_generation(next.clone()).unwrap();
        set.advance_head(head_id, head_id + 1).unwrap();
        expected.push(next);
    }

    for generation in expected {
        assert_eq!(set.generation(generation.generation).unwrap(), generation);
    }
}
