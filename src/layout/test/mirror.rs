use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::encoding::deflate;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, SequenceRange, StreamId, TenantId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::lifecycle::{FileType, PartState};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};

fn config(tenant: u128) -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 0,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets::default(),
        tenant_id: TenantId::new_test_id(tenant),
    }
}

fn one_row(tenant: u128) -> HefRow {
    HefRow {
        epoch: 1,
        sequence: 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xF00D_0000 + tenant),
                tenant_id: TenantId::new_test_id(tenant),
                stream_id: StreamId(1),
                stream_sequence: 0,
                occurred_at: TimestampValue::from_physical_nanos(1_000),
                ingested_at: TimestampValue::from_physical_nanos(2_000),
                source: "crm".to_owned(),
                event_type: "deal.updated".to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: 0,
                entity_id_hash_high: 1,
                entity_id: None,
                actor_id_hash_low: 3,
                actor_id: None,
                account_id_hash_low: 4,
                account_id: None,
                trace_id_hash_low: 5,
                dedupe_hash_low: 0,
                dedupe_hash_high: 6,
                schema_version: 1,
                flags: EventFlags(0),
            },
            payload: PayloadInput::Variant(VariantValue::Int(1)),
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: 0,
            connector_delivery_hash_high: 0,
            provenance: None,
            relationships: None,
        },
    }
}

/// Builds a real minimal HEF file and returns its exact footer tail bytes — the same bytes a per-file tail fetch
/// ([`super::reader::tail_range`]) would return remotely — alongside its `file_id` and an entry usable with
/// [`open_footer`].
fn built_tail(tenant: u128) -> (u128, Vec<u8>, HefFileEntry) {
    let built = build_hef_file(vec![one_row(tenant)], &config(tenant)).unwrap();
    let tail_start = built.bytes.len() - built.footer_len as usize;
    let tail_bytes = built.bytes[tail_start..].to_vec();
    let entry = HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: 1,
        },
        feature_metadata: None,
        file_seal: built.file_seal,
        file_id: built.file_id,
        file_type: FileType::HefFile,
        footer_len: Some(built.footer_len),
        optional_feature_flags: built.footer.optional_feature_flags,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: built.footer.required_feature_flags,
        size_bytes: built.bytes.len() as u64,
        tenant_id: TenantId::new_test_id(tenant),
        tree_len: built.tree_len,
    };
    (built.file_id, tail_bytes, entry)
}

#[test]
fn one_mirror_read_opens_every_file_in_the_generation() {
    let (file_id_a, tail_a, entry_a) = built_tail(1);
    let (file_id_b, tail_b, entry_b) = built_tail(2);
    let generation = 7;
    let mirror_bytes = FooterMirror::build(generation, &[(file_id_a, tail_a), (file_id_b, tail_b)]);
    let mirror = FooterMirror::open(&mirror_bytes).unwrap();

    for entry in [&entry_a, &entry_b] {
        match open_footer(Some(&mirror), entry, generation) {
            FooterSource::Mirror(footer) => assert_eq!(footer.footer().exact_counts.row_count, 1),
            FooterSource::PerFileTail(_) => panic!("expected the mirror to open {entry:?}"),
        }
    }
}

#[test]
fn missing_mirror_falls_back_to_the_per_file_tail() {
    let (_, _, entry) = built_tail(3);
    match open_footer(None, &entry, 1) {
        FooterSource::PerFileTail(range) => assert_eq!(range, entry.tail_range()),
        FooterSource::Mirror(_) => panic!("there is no mirror to open from"),
    }
}

#[test]
fn generation_mismatch_falls_back_to_the_per_file_tail() {
    let (file_id, tail, entry) = built_tail(4);
    let mirror_bytes = FooterMirror::build(5, &[(file_id, tail)]);
    let mirror = FooterMirror::open(&mirror_bytes).unwrap();

    // The planner is opening a later generation than the one the mirror was built for — the section must not be
    // trusted even though the file_id matches.
    match open_footer(Some(&mirror), &entry, 6) {
        FooterSource::PerFileTail(range) => assert_eq!(range, entry.tail_range()),
        FooterSource::Mirror(_) => panic!("a stale generation must not be trusted"),
    }
}

#[test]
fn unknown_file_id_falls_back_to_the_per_file_tail() {
    let (file_id, tail, _) = built_tail(5);
    let (_, _, other_entry) = built_tail(6);
    let mirror_bytes = FooterMirror::build(1, &[(file_id, tail)]);
    let mirror = FooterMirror::open(&mirror_bytes).unwrap();

    match open_footer(Some(&mirror), &other_entry, 1) {
        FooterSource::PerFileTail(range) => assert_eq!(range, other_entry.tail_range()),
        FooterSource::Mirror(_) => panic!("the mirror carries no section for this file"),
    }
}

#[test]
fn a_corrupted_section_falls_back_alone_the_rest_of_the_mirror_still_opens() {
    let (file_id_a, tail_a, entry_a) = built_tail(7);
    let (file_id_b, tail_b, entry_b) = built_tail(8);
    let generation = 1;
    let mirror_bytes = FooterMirror::build(generation, &[(file_id_a, tail_a), (file_id_b, tail_b)]);

    // Flip a byte inside the first section's tail bytes, past the section count and its own header (file_id,
    // generation, blake3, length), so the corrupted section's own BLAKE3 no longer verifies but the framing around
    // it stays intact.
    let mut plain = deflate::decompress(&mirror_bytes).unwrap();
    let corrupt_at = 8 + 16 + 8 + 32 + 8 + 4;
    plain[corrupt_at] ^= 0xFF;
    let corrupted = deflate::compress(&plain);

    let mirror = FooterMirror::open(&corrupted).unwrap();
    match open_footer(Some(&mirror), &entry_a, generation) {
        FooterSource::PerFileTail(range) => assert_eq!(range, entry_a.tail_range()),
        FooterSource::Mirror(_) => panic!("the corrupted section must not verify"),
    }
    match open_footer(Some(&mirror), &entry_b, generation) {
        FooterSource::Mirror(footer) => assert_eq!(footer.footer().exact_counts.row_count, 1),
        FooterSource::PerFileTail(_) => panic!("the second file's section was untouched"),
    }
}

#[test]
fn truncated_object_refuses_instead_of_panicking() {
    assert!(FooterMirror::open(&[1, 2, 3]).is_err());
}

#[test]
fn a_relabelled_section_fails_its_checksum_instead_of_serving_another_files_footer() {
    // The checksum covered only the tail bytes, so an intact section could be relabelled with another file's `file_id`
    // and still verify — the planner would then read that file's schema, pruning statistics, and byte ranges. Binding
    // the identifiers into the hash makes the relabelled section fail and fall back to the authoritative tail read
    // (issue #8462).
    let (file_id, tail, _) = built_tail(11);
    let (other_file_id, _, other_entry) = built_tail(12);
    let generation = 3;
    let mirror_bytes = FooterMirror::build(generation, &[(file_id, tail)]);

    // Rewrite the section's file_id in place: the section count is 8 bytes, then the section's own file_id.
    let mut plain = deflate::decompress(&mirror_bytes).unwrap();
    plain[8..24].copy_from_slice(&other_file_id.to_le_bytes());
    let relabelled = deflate::compress(&plain);

    let mirror = FooterMirror::open(&relabelled).unwrap();
    match open_footer(Some(&mirror), &other_entry, generation) {
        FooterSource::PerFileTail(range) => assert_eq!(range, other_entry.tail_range()),
        FooterSource::Mirror(_) => panic!("a relabelled section must not verify"),
    }
}

#[test]
fn a_section_moved_to_another_generation_fails_its_checksum() {
    // The generation is inside the checksum too, so a section carried forward into a generation it does not describe
    // fails rather than being served (issue #8462).
    let (file_id, tail, entry) = built_tail(13);
    let mirror_bytes = FooterMirror::build(3, &[(file_id, tail)]);

    let mut plain = deflate::decompress(&mirror_bytes).unwrap();
    plain[24..32].copy_from_slice(&9u64.to_le_bytes());
    let moved = deflate::compress(&plain);

    let mirror = FooterMirror::open(&moved).unwrap();
    match open_footer(Some(&mirror), &entry, 9) {
        FooterSource::PerFileTail(range) => assert_eq!(range, entry.tail_range()),
        FooterSource::Mirror(_) => panic!("a section moved between generations must not verify"),
    }
}
