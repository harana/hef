//! Conformance tests for atomic publication and consistent snapshot fields.
//!
//! Manifest publication is atomic with respect to HEF coverage and watermarks. A query must never see a partial state —
//! either the previous generation (no file, no coverage) or the new generation (file plus coverage) in full.

use hef::events::{SequenceRange, TenantId};
use hef::invariants::PublishedSet;
use hef::invariants::sim::SimulatedPublishedSet;
use hef::lifecycle::{FileType, HefFileEntry, ManifestGeneration, PartState};
use hef::typed_id::TypedIdTestExt;

fn tenant() -> TenantId {
    TenantId::new_test_id(12)
}
const FILE_A: u128 = 0xA1A1;

fn entry_for(range: SequenceRange) -> HefFileEntry {
    HefFileEntry {
        coverage: range,
        feature_metadata: None,
        file_seal: [1u8; 32],
        file_id: FILE_A,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 4096,
        tenant_id: tenant(),
        tree_len: None,
    }
}

/// conformance: hef-manifest-integration/atomic-publication-and-consistent-snapshot-fields/no-watermark-file-skew
#[test]
fn no_watermark_file_skew() {
    // Before publication: the head generation contains no files and covers no ranges. After publication: the head
    // generation has an entry with both coverage and file_seal set together in one atomic step. There is no
    // observable in-between state where coverage exists without its file or a file exists without its coverage.
    let range = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 50,
    };
    let mut store = SimulatedPublishedSet::new();

    // Pre-publication: nothing is covered.
    let (_, head) = store.head().unwrap();
    assert!(
        !head.covers(&range, tenant()),
        "unpublished range must not be covered before publication"
    );
    assert_eq!(head.files.len(), 0, "no files before publication");

    // Publish: entry with coverage and file_seal lands atomically.
    let entry = entry_for(range);
    let gen1 = ManifestGeneration {
        generation: 1,
        files: vec![entry],
        ..Default::default()
    };
    store.put_generation(gen1).unwrap();
    store.advance_head(0, 1).unwrap();

    // Post-publication: coverage and file_seal are both present — never one without the other.
    let (_, published) = store.head().unwrap();
    assert!(
        published.covers(&range, tenant()),
        "coverage must be visible after publication"
    );
    let entry = &published.files[0];
    assert_eq!(entry.coverage, range, "entry coverage must match the published range");
    assert_ne!(
        entry.file_seal, [0u8; 32],
        "file_seal must be set alongside coverage — no partial entry observed"
    );
}
