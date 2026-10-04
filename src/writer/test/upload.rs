use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::error::StorageError;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::object_store::sim::{ObjectFault, SimObjectStore};
use crate::object_store::{ETag, StoredObject};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};
use std::sync::Mutex;

const KEY: &str = "hef/test.hef";

fn event(i: u64) -> EventInput {
    EventInput {
        envelope: EventEnvelope {
            event_id: EventId::new_test_id(0xCAFE + u128::from(i)),
            tenant_id: TenantId::new_test_id(9),
            stream_id: StreamId(1),
            stream_sequence: i,
            occurred_at: TimestampValue::from_physical_nanos(100 + i as i64),
            ingested_at: TimestampValue::from_physical_nanos(200 + i as i64),
            source: "crm".into(),
            event_type: "deal.updated".into(),
            entity_type: "opportunity".into(),
            entity_id_hash_low: i,
            entity_id_hash_high: 0,
            entity_id: Some(format!("opp-{i}")),
            actor_id_hash_low: 0,
            actor_id: None,
            account_id_hash_low: 0,
            account_id: None,
            trace_id_hash_low: 0,
            dedupe_hash_low: 1000 + i,
            dedupe_hash_high: 7,
            schema_version: 1,
            flags: EventFlags(0),
        },
        payload: PayloadInput::Variant(VariantValue::Int(i as i64)),
        source_schema: None,
        source_delivery: None,
        connector_delivery_hash_low: 5000 + i,
        connector_delivery_hash_high: 1,
        provenance: None,
        relationships: None,
    }
}

/// A file with several small stripes, so a tiny part minimum turns it into a multipart upload.
fn multi_stripe_file() -> BuiltHef {
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 1,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 0,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 8,
            index_granularity_bytes: 1 << 20,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
            stripe_target_bytes: 512,
        },
        tenant_id: TenantId::new_test_id(9),
    };
    let rows = (0..40)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: event(i),
        })
        .collect();
    let built = build_hef_file(rows, &config).unwrap();
    assert!(built.footer.stripes.len() >= 2, "test needs a multi-stripe file");
    built
}

/// Delegates to the simulation store while recording where each uploaded part starts.
#[derive(Default)]
struct PartRecorder {
    inner: SimObjectStore,
    part_lens: Mutex<Vec<u64>>,
}

impl ObjectStore for PartRecorder {
    fn get(&self, key: &str) -> Result<Option<StoredObject>, StorageError> {
        self.inner.get(key)
    }
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, StorageError> {
        self.inner.put_if_absent(key, bytes)
    }
    fn put_if_match(&self, key: &str, bytes: &[u8], etag: &ETag) -> Result<PutOutcome, StorageError> {
        self.inner.put_if_match(key, bytes, etag)
    }
    fn stat(&self, key: &str) -> Result<Option<ObjectStat>, StorageError> {
        self.inner.stat(key)
    }
    fn begin_multipart(&self, key: &str) -> Result<MultipartUpload, StorageError> {
        self.inner.begin_multipart(key)
    }
    fn upload_part(&self, upload: &MultipartUpload, part_number: u32, bytes: &[u8]) -> Result<ETag, StorageError> {
        self.part_lens.lock().unwrap().push(bytes.len() as u64);
        self.inner.upload_part(upload, part_number, bytes)
    }
    fn complete_multipart(&self, upload: &MultipartUpload, parts: &[UploadedPart]) -> Result<PutOutcome, StorageError> {
        self.inner.complete_multipart(upload, parts)
    }
    fn abort_multipart(&self, upload: &MultipartUpload) -> Result<(), StorageError> {
        self.inner.abort_multipart(upload)
    }
    fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.inner.delete(key)
    }
}

#[test]
fn parts_reach_the_minimum_and_cut_only_at_segment_ends() {
    // Segments: a 10-byte header, three stripes, and a final stripe carrying the footer.
    let segments = [10, 40, 40, 40, 25];
    assert_eq!(upload_parts(&segments, 50), vec![0..50, 50..130, 130..155]);
    // A file below the minimum is one part.
    assert_eq!(upload_parts(&segments, 1_000), vec![0..155]);
    // No minimum: one part per segment.
    assert_eq!(upload_parts(&segments, 0).len(), segments.len());
}

#[test]
fn a_multipart_upload_cuts_parts_on_stripe_boundaries() {
    let built = multi_stripe_file();
    let store = PartRecorder::default();

    upload_hef(&store, KEY, &built, 1).unwrap();

    assert_eq!(store.inner.object(KEY).unwrap(), built.bytes);
    let part_lens = store.part_lens.lock().unwrap().clone();
    assert!(part_lens.len() > 1, "a tiny part minimum must use multipart");
    let mut starts = Vec::new();
    let mut offset = 0u64;
    for len in &part_lens {
        starts.push(offset);
        offset += len;
    }
    // Every part after the first begins exactly where a stripe begins.
    for start in starts.iter().skip(1) {
        assert!(
            built.footer.stripes.iter().any(|stripe| stripe.file_offset == *start),
            "part starting at {start} does not begin a stripe"
        );
    }
}

#[test]
fn a_failed_part_aborts_the_multipart_upload() {
    let built = multi_stripe_file();
    let store = SimObjectStore::new();
    store.inject(ObjectFault::FailUploadPart);

    let result = upload_hef(&store, KEY, &built, 1);

    assert!(matches!(result, Err(PublishFailure::Storage(_))));
    assert_eq!(store.open_uploads(), 0, "the upload was aborted");
    assert!(store.object(KEY).is_none());
}

#[test]
fn a_failed_completion_aborts_the_multipart_upload() {
    let built = multi_stripe_file();
    let store = SimObjectStore::new();
    store.inject(ObjectFault::FailCompleteMultipart);

    assert!(upload_hef(&store, KEY, &built, 1).is_err());
    assert_eq!(store.open_uploads(), 0);
    assert!(store.object(KEY).is_none());
}

#[test]
fn an_object_already_at_the_key_is_verified_not_uploaded_again() {
    let built = multi_stripe_file();
    let store = SimObjectStore::new();
    upload_hef(&store, KEY, &built, u64::MAX).unwrap();
    // A second upload would fail if attempted; the existing matching object is accepted instead.
    store.inject(ObjectFault::FailPutIfAbsent);
    upload_hef(&store, KEY, &built, u64::MAX).unwrap();

    // Different bytes already at the key are refused rather than published.
    let other = SimObjectStore::new();
    other.put_if_absent(KEY, b"not the built file").unwrap();
    assert!(matches!(
        upload_hef(&other, KEY, &built, u64::MAX),
        Err(PublishFailure::Verification(_))
    ));
}
