# HEF Required APIs — Writer, Reader, Aggregation, LiveOverlay, QueryEngine, Context, Introspection

Companion artifact for the `hef-apis` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


### Writer API

```rust
trait EventJournal {
    fn append_batch(&self, batch: HEJCompactBatchV1) -> CommitReceipt;
    fn durable_cursor(&self) -> JournalCursor;
    fn replay_from(&self, cursor: JournalCursor) -> JournalReplayStream;
}

trait EventFileWriter {
    fn begin_file(&mut self, spec: EventFileSpec) -> Result<()>;
    fn append_event_batch(&mut self, batch: DecodedEventBatch) -> Result<()>;
    fn finish(self) -> Result<HefEventFile>;
}
```

### Reader API

```rust
trait EventFileReader {
    fn read_header(&self) -> Result<EventFileHeader>;
    fn read_footer(&self) -> Result<EventFileFooter>;
    fn plan_scan(&self, query: EventQuery) -> Result<ScanPlan>;
    fn read_columns(&self, plan: ScanPlan) -> Result<RecordBatchStream>;
    fn read_payloads(&self, refs: &[PayloadRef]) -> Result<PayloadBatch>;
}
```

### Aggregation API

```rust
trait EventAggregateReader {
    fn can_answer_exact(&self, query: AggregateQuery) -> bool;
    fn answer_exact(&self, query: AggregateQuery) -> Result<AggregateResult>;

    fn can_answer_approx(&self, query: AggregateQuery) -> bool;
    fn answer_approx(&self, query: AggregateQuery) -> Result<AggregateResult>;
}
```

### LiveOverlay API

```rust
trait LiveOverlay {
    fn publish_segment(&self, segment: LiveArrowSegment) -> Result<()>;
    fn snapshot(&self, range: SequenceRange, schema: SchemaSnapshotId) -> Result<LiveOverlaySnapshot>;
    fn commit_watermark(&self) -> JournalCursor;
    fn visibility_watermark(&self) -> JournalCursor;
    fn evict_covered(&self, coverage: &[SequenceRange]) -> Result<()>;
    fn rebuild_from_journal(&self, from: JournalCursor, to: JournalCursor) -> Result<()>;
}

struct LiveArrowSegment {
    sequence_range: SequenceRange,
    occurred_at_range: TimeRange,
    ingested_at_range: TimeRange,
    schema_snapshot_id: SchemaSnapshotId,
    batches: Vec<RecordBatch>,
    metadata: LiveSegmentMetadata,
    aggregate_deltas: Option<LiveAggregateDeltas>,
    context_projection: Option<RecordBatch>,
}
```

### QueryEngine integration API

The production integration registers one logical table:

```rust
struct HaranaEventsTableProvider {
    manifest: Arc<ManifestCatalog>,
    journal: Arc<dyn EventJournal>,
    live: Arc<dyn LiveOverlay>,
    schema_registry: Arc<SchemaRegistry>,
    deletion_vectors: Arc<DeletionVectorCatalog>,
    corrections: Arc<CorrectionOverlay>,
    authorizer: Arc<EventColumnAuthorizer>,
}
```

The provider scan builds a single HEF/LiveOverlay snapshot and returns a physical plan equivalent to:

```text
UnionExec
  HefExec
  LiveOverlayExec
```

but with provider-owned snapshot semantics, filter/projection pushdown, aggregate shortcuts, authorization, and deletion-vector/correction handling.

Required scan behaviour:

```text
1. derive projection and filters from QueryEngine;
2. authorize requested columns;
3. take QuerySnapshot;
4. choose manifest layout plan;
5. prune manifest HEF files;
6. select disjoint HEF and LiveOverlay sequence ranges;
7. select projection plan and granule pruning plan;
8. construct HefExec and LiveOverlayExec with identical schema;
9. avoid overlapping sequence ranges;
10. apply visible HEF-native deletion vectors and corrections;
11. report exact versus inexact filter pushdown correctly;
12. report physical statistics, partitioning, ordering, and granule pruning;
13. return a RecordBatch stream to QueryEngine.
```

### Context/evidence API

```rust
trait EventContextReader {
    fn plan_context_scan(&self, query: ContextQuery) -> Result<ContextScanPlan>;
    fn read_context_packets(&self, plan: ContextScanPlan) -> Result<ContextPacketStream>;
}

struct ContextPacket {
    public_event_ref: PublicEventRef,
    occurred_at: TimestampValue,
    source: PublicSourceFacet,
    event_type: PublicEventTypeFacet,
    entity_ref: Option<PublicEntityRef>,
    title: Option<String>,
    summary: Option<String>,
    metric_values: Vec<PublicMetricValue>,
    evidence_lineage: PublicEvidenceLineageRef,
}
```

Context APIs must never return raw storage paths, internal sequences, payload offsets, or internal tenant identifiers.

---

### Introspection API and system tables

Harana must expose storage introspection through QueryEngine-visible system tables for operators and benchmark tooling. These tables are internal/admin surfaces, not ordinary public event output.

```text
system.hef_files
  file_id, generation_id, part_state, layout_class, projection_count, row_count,
  granule_count, file_size, min/max occurred_at, min/max epoch/sequence,
  deletion_vector_generation, file_blake3, created_at, published_at.

system.hef_columns
  file_id, projection_id, column_id, logical_type, physical_type, codec_pipeline,
  compressed_bytes, uncompressed_bytes, null_count, marks_count, min/max summary.

system.hef_granules
  file_id, projection_id, granule_id, stripe_id, row_count, first/last epoch/sequence,
  min/max occurred_at, compressed_bytes_estimate, skip_index_refs, aggregate_refs.

system.hef_rewrites
  rewrite_id, input_file_ids, output_file_ids, reason, started_at, finished_at,
  input_rows, output_rows, deletion_vectors_applied, projections_rebuilt,
  compact_wide_transition, status.
```

System tables must enforce tenant/admin authorization and must not expose raw object-store credentials, local filesystem paths, payload bytes, or embedding values.

---

