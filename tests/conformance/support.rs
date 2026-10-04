//! Shared fixtures for the HEF storage-core conformance tests: events with shreddable payloads, an ingest world over
//! the simulation interfaces, and publish/build helpers. Everything is seed-deterministic.

use hef::artifacts::batch::{EventInput, PayloadInput};
use hef::artifacts::overlay::LiveOverlayStore;
use hef::artifacts::watermark::WatermarkTracker;
use hef::events::variant::VariantValue;
use hef::events::*;
use hef::invariants::ShardId;
use hef::invariants::sim::{SimClock, SimJournalStorage};
use hef::layout::LayoutTargets;
use hef::typed_id::TypedIdTestExt;
use hef::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};
use hef::writer::pipeline::{FlushReason, RouteDependency, WorkerCommitPipeline};
use hef::writer::reserve::SequenceAllocator;
use hef::writer::sim::SimSafeRetryStore;
use std::collections::BTreeMap;

pub const SHARD: ShardId = ShardId(0);

pub fn tenant() -> TenantId {
    TenantId::new_test_id(9)
}

/// One event with a payload rich enough to drive shredding statistics.
pub fn event(i: u64) -> EventInput {
    let mut payload = BTreeMap::new();
    payload.insert("kind".to_owned(), VariantValue::String(format!("k{}", i % 4)));
    payload.insert("amount".to_owned(), VariantValue::Int(1_000 + i as i64));
    payload.insert(
        "note".to_owned(),
        VariantValue::String(format!("free text body number {i}")),
    );
    if i.is_multiple_of(3) {
        payload.insert("rare".to_owned(), VariantValue::Bool(true));
    }
    EventInput {
        envelope: EventEnvelope {
            event_id: EventId::new_test_id(0xC0FFEE + u128::from(i)),
            tenant_id: tenant(),
            stream_id: StreamId(1),
            stream_sequence: i,
            occurred_at: TimestampValue::from_physical_nanos(1_000_000 + i as i64 * 1_000),
            ingested_at: TimestampValue::from_physical_nanos(2_000_000 + i as i64 * 1_000),
            source: ["crm", "billing"][i as usize % 2].to_owned(),
            event_type: "deal.updated".to_owned(),
            entity_type: "opportunity".to_owned(),
            entity_id_hash_low: i,
            entity_id_hash_high: 1,
            entity_id: Some(format!("opp-{i}")),
            actor_id_hash_low: 3,
            actor_id: None,
            account_id_hash_low: 4,
            account_id: None,
            trace_id_hash_low: 5,
            dedupe_hash_low: 1_000 + i,
            dedupe_hash_high: 7,
            schema_version: 2,
            flags: EventFlags(0),
        },
        payload: PayloadInput::Variant(VariantValue::Object(payload)),
        source_schema: Some("json".to_owned()),
        source_delivery: Some(format!("delivery-{i}")),
        connector_delivery_hash_low: 5_000 + i,
        connector_delivery_hash_high: 1,
        provenance: None,
        relationships: None,
    }
}

/// An ingest world over the simulation interfaces.
pub struct World {
    pub allocator: SequenceAllocator,
    pub clock: SimClock,
    /// Index of the next event `ingest` submits, so every ingested event is distinct and none is deduplicated as a
    /// retry.
    pub next_event: u64,
    pub overlay: LiveOverlayStore,
    pub retry: SimSafeRetryStore,
    pub storage: SimJournalStorage,
    pub watermarks: WatermarkTracker,
    pub worker: WorkerCommitPipeline,
}

impl World {
    pub fn new(seed: u64) -> Self {
        Self {
            clock: SimClock::new(seed),
            storage: SimJournalStorage::new(),
            allocator: SequenceAllocator::new(1),
            next_event: 0,
            watermarks: WatermarkTracker::new(),
            overlay: LiveOverlayStore::new(),
            retry: SimSafeRetryStore::new(),
            worker: WorkerCommitPipeline::new(1, SHARD, tenant(), RouteDependency::AppendOnly, 1 << 20),
        }
    }

    /// Submits and flushes `count` events as one frame; returns its range.
    pub fn ingest(&mut self, count: u64) -> SequenceRange {
        for i in self.next_event..self.next_event + count {
            self.worker.submit(event(i), 1, &mut self.retry, &self.clock).unwrap();
        }
        self.next_event += count;
        self.worker
            .flush(
                FlushReason::Target,
                &mut self.allocator,
                &mut self.storage,
                &mut self.watermarks,
                &mut self.retry,
                &mut self.overlay,
                &self.clock,
            )
            .unwrap()
            .range
    }
}

/// Small layout targets so a few dozen rows exercise granule and stripe formation.
pub fn small_targets() -> LayoutTargets {
    LayoutTargets {
        index_granularity: 16,
        index_granularity_bytes: 1 << 20,
        stripe_target_bytes: 4096,
        max_stripe_bytes: 512 * 1024 * 1024,
        min_bytes_for_wide: 10 * 1024 * 1024,
    }
}

pub fn build_config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: hef::security::FooterEncryption::Plaintext,
        freetext: hef::columns::FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: hef::columns::PromotionPlan::default(),
        targets: small_targets(),
        tenant_id: tenant(),
    }
}

/// Builds a small in-memory HEF file directly from sample rows.
pub fn built_file(rows: u64) -> BuiltHef {
    let rows: Vec<HefRow> = (0..rows)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: event(i),
        })
        .collect();
    build_hef_file(rows, &build_config()).unwrap()
}
