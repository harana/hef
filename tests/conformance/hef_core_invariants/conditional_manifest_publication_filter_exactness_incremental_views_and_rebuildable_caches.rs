//! Checks the rules that keep the shared catalogue of published files correct when several writers update it at once.
//! When two writers race to publish, the loser must re-read the latest state and re-apply its change on top rather than
//! overwriting the winner's work.
use hef::lifecycle::ManifestGeneration;

use crate::support;
use hef::invariants::PublishedSet;
use hef::invariants::sim::SimulatedPublishedSet;
/// conformance:
/// hef-core-invariants/conditional-manifest-publication-filter-exactness-incremental-views-and-rebuildable-caches/
/// lost-manifest-cas-rebases-instead-of-overwriting
#[test]
fn lost_manifest_cas_rebases_instead_of_overwriting() {
    // Two publishers race to advance the head; the loser re-reads the latest generation, rebases, and retries — the
    // winner's generation object is never overwritten. Exercised against the PublishedSet interface's real
    // create-only/If-Match semantics; the object-store implementation arrives with hef-manifest-integration behind the
    // same trait.
    let mut set = SimulatedPublishedSet::new();
    let (head, base) = set.head().unwrap();
    // Winner publishes generation 1.
    let winner = ManifestGeneration {
        generation: head + 1,
        files: base.files.clone(),
        ..Default::default()
    };
    set.put_generation(winner).unwrap();
    set.advance_head(head, head + 1).unwrap();
    // Loser tries the same advance from the stale head: CAS lost.
    let lost = set.advance_head(head, head + 1);
    assert!(lost.is_err());
    // Rebase: re-read, build generation 2, retry. Generation 1 survives.
    let (new_head, latest) = set.head().unwrap();
    assert_eq!(new_head, 1);
    let mut rebased = ManifestGeneration {
        generation: new_head + 1,
        files: latest.files,
        ..Default::default()
    };
    // The rebased publication carries the loser's entry forward.
    rebased.files.push(hef::lifecycle::HefFileEntry {
        coverage: hef::events::SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: 2,
        },
        feature_metadata: None,
        file_seal: [0; 32],
        file_id: 7,
        file_type: hef::lifecycle::FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: hef::lifecycle::PartState::Active,
        required_feature_flags: 0,
        size_bytes: 1,
        tenant_id: support::tenant(),
        tree_len: None,
    });
    set.put_generation(rebased).unwrap();
    set.advance_head(new_head, new_head + 1).unwrap();
    assert_eq!(set.head().unwrap().0, 2);
    assert!(set.generation(1).unwrap().files.is_empty(), "winner not overwritten");
}
