use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan, column_ids};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, SequenceRange, StreamId, TimestampValue};
use crate::file::constant::CHUNK_GROUP_BYTES;
use crate::layout::reader::PayloadRead;
use crate::layout::{HEADER_BLOCK_LEN, LayoutTargets, required_features};
use crate::lifecycle::{FileType, PartState};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};
use std::collections::BTreeMap as StdBTreeMap;
use std::sync::Mutex;

const TENANT: u128 = 3;

/// A remote object that records every range asked of it.
struct CountingSource {
    object: Vec<u8>,
    requests: Mutex<Vec<(u64, u64)>>,
}

impl CountingSource {
    fn new(object: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            object: object.to_vec(),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<(u64, u64)> {
        self.requests.lock().unwrap().clone()
    }
}

impl RangeSource for CountingSource {
    fn read_range(&self, _object: u128, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        self.requests.lock().unwrap().push((offset, len));
        let end = offset.checked_add(len).ok_or(FileError::OutOfBounds)?;
        self.object
            .get(offset as usize..end as usize)
            .map(<[u8]>::to_vec)
            .ok_or(FileError::OutOfBounds)
    }
}

fn noise(seed: u64, len: usize) -> String {
    let mut state = seed + 1;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            char::from(33 + ((state >> 32) % 94) as u8)
        })
        .collect()
}

fn row(i: u64, payload: StdBTreeMap<String, VariantValue>) -> HefRow {
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xF00D_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(TENANT),
                stream_id: StreamId(1),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000 + i as i64),
                ingested_at: TimestampValue::from_physical_nanos(2_000 + i as i64),
                source: "crm".to_owned(),
                event_type: "deal.updated".to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: i,
                entity_id_hash_high: 1,
                entity_id: None,
                actor_id_hash_low: 3,
                actor_id: None,
                account_id_hash_low: 4,
                account_id: None,
                trace_id_hash_low: 5,
                dedupe_hash_low: i,
                dedupe_hash_high: 6,
                schema_version: 1,
                flags: EventFlags(0),
            },
            payload: PayloadInput::Variant(VariantValue::Object(payload)),
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: i,
            connector_delivery_hash_high: 0,
            provenance: None,
            relationships: None,
        },
    }
}

fn config(index_granularity: usize, granule_bytes: usize, stripe_target_bytes: usize) -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity,
            index_granularity_bytes: granule_bytes,
            stripe_target_bytes,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(TENANT),
    }
}

/// One granule in one multi-megabyte stripe, so the stripe carries an outboard proof tree. Every eighth row's `blob`
/// is an integer and the rest are 64 KiB of incompressible text, so `blob` is too mixed to shred and its values make
/// the granule's residual arena several proof leaves long.
fn large_file() -> BuiltHef {
    let rows: Vec<_> = (0..64u64)
        .map(|i| {
            let blob = if i.is_multiple_of(8) {
                VariantValue::Int(i as i64)
            } else {
                VariantValue::String(noise(i, 64 * 1024))
            };
            row(
                i,
                StdBTreeMap::from([
                    ("amount".to_owned(), VariantValue::Int(i as i64)),
                    ("blob".to_owned(), blob),
                ]),
            )
        })
        .collect();
    let built = build_hef_file(rows, &config(64, 64 << 20, 64 << 20)).unwrap();
    assert_eq!(built.footer.stripes.len(), 1);
    assert!(built.tree_len.is_some(), "a multi-megabyte stripe carries proof nodes");
    built
}

/// Small stripes, one granule of four rows each.
fn multi_stripe_file(footer_dek: Option<[u8; 32]>) -> BuiltHef {
    let rows: Vec<_> = (0..40u64)
        .map(|i| {
            row(
                i,
                StdBTreeMap::from([
                    ("amount".to_owned(), VariantValue::Int(i as i64)),
                    ("note".to_owned(), VariantValue::String(format!("note {i}"))),
                ]),
            )
        })
        .collect();
    let mut config = config(4, 1 << 20, 1);
    if let Some(dek) = footer_dek {
        config.footer_encryption = crate::security::FooterEncryption::Encrypted;
        config.footer_dek = Some(dek);
    }
    let built = build_hef_file(rows, &config).unwrap();
    assert!(built.footer.stripes.len() > 1);
    built
}

fn entry(built: &BuiltHef) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: built.footer.exact_counts.row_count,
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
        tenant_id: TenantId::new_test_id(TENANT),
        tree_len: built.tree_len,
    }
}

fn open(source: &Arc<CountingSource>, built: &BuiltHef, footer_dek: Option<&[u8; 32]>) -> HefFile {
    let source: Arc<dyn RangeSource> = source.clone();
    let header = HeaderCommitment::from_header_block(&built.bytes[..HEADER_BLOCK_LEN]).unwrap();
    HefFile::open_remote(source, &entry(built), &header, footer_dek).unwrap()
}

fn local(built: &BuiltHef, footer_dek: Option<&[u8; 32]>) -> HefFile {
    HefFile::open_with_keys(built.bytes.clone(), Some(&built.file_seal), footer_dek).unwrap()
}

fn intersects((offset, len): (u64, u64), start: u64, end: u64) -> bool {
    offset < end && start < offset + len
}

/// With `footer_len` in the manifest entry the open is one exact tail request, and a cold read of one column block
/// then costs at most two more (the stripe's marks pages, then the block), none of them the whole stripe.
#[test]
fn a_cold_point_read_costs_at_most_two_requests_once_the_footer_length_is_known() {
    let built = large_file();
    let source = CountingSource::new(&built.bytes);
    let file = open(&source, &built, None);
    assert_eq!(source.requests().len(), 1, "an exact tail open is a single request");

    let granule = built.footer.granules[0].granule_id;
    let read = file.read_column(column_ids::SEQUENCE, granule).unwrap();
    let requests = source.requests();
    assert!(
        requests.len() - 1 <= 2,
        "a cold point read issued {} requests",
        requests.len() - 1
    );
    let stripe = &built.footer.stripes[0];
    assert!(requests[1..].iter().all(|(_, len)| *len < stripe.byte_len));

    let expected = local(&built, None).read_column(column_ids::SEQUENCE, granule).unwrap();
    assert_eq!(read.data, expected.data);
    assert_eq!(read.presence, expected.presence);
}

/// Reading a granule in one stripe never asks for a byte of any other stripe, its marks pages included.
#[test]
fn a_pruned_stripe_is_never_fetched() {
    let built = multi_stripe_file(None);
    let source = CountingSource::new(&built.bytes);
    let file = open(&source, &built, None);
    let surviving = built.footer.granules[0];
    file.read_column(column_ids::SEQUENCE, surviving.granule_id).unwrap();

    let requests = source.requests();
    assert!(requests.len() - 1 <= 2);
    for stripe in built
        .footer
        .stripes
        .iter()
        .filter(|stripe| stripe.stripe_id != surviving.stripe_id)
    {
        let end = stripe.file_offset + stripe.byte_len;
        assert!(
            !requests
                .iter()
                .any(|request| intersects(*request, stripe.file_offset, end)),
            "stripe {} was pruned but fetched",
            stripe.stripe_id
        );
    }
}

/// A cold single-row payload read through the remote reader returns what the in-memory reader returns, and fetches
/// the proof leaves around that row's residual slot rather than the granule's whole residual arena.
#[test]
fn a_cold_single_row_payload_read_fetches_only_that_rows_residual_slot() {
    let built = large_file();
    let payload = built.footer.payload_granules[0];
    assert!(
        payload.residual_len > 2 * CHUNK_GROUP_BYTES as u64,
        "the arena spans several proof leaves"
    );
    let stripe = &built.footer.stripes[0];
    let base = if built.footer.required_feature_flags & required_features::STRIPE_RELATIVE_MARKS != 0 {
        stripe.file_offset
    } else {
        0
    };
    let residual_start = base + payload.residual_offset;
    let residual_end = residual_start + payload.residual_len;

    let source = CountingSource::new(&built.bytes);
    let file = open(&source, &built, None);
    let read = file.payload(1).unwrap();
    assert!(matches!(read, PayloadRead::Value(_)));
    assert_eq!(read, local(&built, None).payload(1).unwrap());
    assert!(
        source
            .requests()
            .iter()
            .all(|(offset, len)| !(*offset <= residual_start && residual_end <= offset + len)),
        "no request may fetch the whole residual arena"
    );
}

/// A fresh reader made from a remote one reads the same values without the first reader's fetched ranges.
#[test]
fn a_fresh_reader_of_a_remote_file_reads_the_same_values() {
    let built = multi_stripe_file(None);
    let source = CountingSource::new(&built.bytes);
    let file = open(&source, &built, None);
    let first = file.payload(5).unwrap();
    let fresh = file.fresh_reader().unwrap();
    assert_eq!(fresh.payload(5).unwrap(), first);
    assert_eq!(fresh.header().file_id, built.file_id);
    assert_eq!(fresh.header().row_count, built.header.row_count);
}
