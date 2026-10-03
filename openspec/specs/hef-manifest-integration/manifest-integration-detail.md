# HEF Manifest Integration — Snapshot/Watermarks and Metadata Placement

Companion artifact for the `hef-manifest-integration` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


HEF contains embedded file summaries, but visibility is controlled by an external manifest.

Manifest entry:

```text
file_id
generation_id
tenant_id internal only
storage_uri internal only
file_size
row_count
layout_class
projection_directory_ref
rowset_fingerprint
min/max occurred_at
min/max ingested_at
min/max epoch/sequence
source_id set or compact membership summary
event_type_id set or compact membership summary
entity_type_id set or compact membership summary
aggregate_capabilities
index_capabilities
physical_sort_order
file_blake3
journal_low_watermark
journal_high_watermark
commit_watermark
visibility_watermark
snapshot_watermark_upper_bound
deletion_vector_generation
correction_overlay_generation
part_state
created_at
published_at
```

The manifest is the query visibility boundary.

Required invariant:

```text
A query sees either the previous generation or the new generation, never a partial HEF file, partial projection, partial deletion-vector generation, or partial watermark update.
```

### Query snapshot and HEF/LiveOverlay watermarks

The manifest must expose enough information for the `QueryEngine` table provider to build a consistent HEF/LiveOverlay snapshot.

Required snapshot fields:

```text
manifest_generation
visible_file_set
selected_projection_plan
commit_watermark
visibility_watermark
snapshot_watermark
hef_sequence_ranges
live_sequence_ranges
schema_snapshot_id
deletion_vector_generation
correction_overlay_generation
```

Required HEF/LiveOverlay visibility rule:

```text
HEF source covers:       manifest-published Active HEF sequence ranges <= snapshot_watermark
LiveOverlay source covers: node-local queryable sequence ranges not covered by HEF and <= snapshot_watermark
forbidden:               overlapping HEF and LiveOverlay sequence ranges in one query snapshot
```

Manifest publication must be atomic with respect to HEF coverage, projections, deletion-vector generation, and watermarks. A query must never observe a HEF file without its coverage watermark, or a coverage watermark without the corresponding HEF file.

Manifest publication protocol on object storage:

```text
mechanism
  object-store conditional writes, no external lock service or coordination
  database on the publication path.

new generation
  write manifest object generation-N+1 with If-None-Match: * (create-only PUT);
  a concurrent publisher loses the race deterministically and retries against
  the observed latest generation.

pointer advance
  the current-manifest pointer object is advanced with If-Match on the ETag of
  the pointer version the publisher read (compare-and-swap); a failed CAS means
  another publisher won and this publisher must re-read, rebase, and retry.

requirements
  the object store must support conditional PUT (S3 If-None-Match since 2024,
  If-Match CAS since 2025; GCS and Azure preconditions equivalently);
  stores without conditional writes require an explicit external commit lock and
  are a documented degraded deployment mode;
  manifest objects are immutable once written; only the pointer advances;
  readers resolve pointer -> generation object -> file set in one consistent pass.
```

### Metadata placement rule

```text
Manifest:
  small summaries needed to decide whether to open a file;
  part_state;
  projection availability;
  deletion-vector generation;
  commit_watermark, visibility_watermark, and snapshot_watermark upper bound.

HEF footer:
  authoritative file-level metadata, granule directory, marks directory, projection directory,
  aggregate directory, and optional HEF-native deletion-vector directory.

HEF stripe/granule/page blocks:
  authoritative local aggregates and pruning metadata for partial-file queries.

PreparedView files:
  cross-file semantic product-level materializations.

LiveOverlay:
  exact fresh deltas for queryable HEJ-backed events not yet HEF-covered.
```

Alongside the deletion-vector generation records, a manifest generation MAY publish one
optional **footer-mirror object**: the footer sections of the files published in that
generation, keyed by `file_id` and compressed as one object. Each per-file section carries
its own authoritative BLAKE3 checksum and the `file_id` and generation it mirrors, so a
planner can validate a section on its own before use. On any mismatch — a failed BLAKE3, a
disagreeing `file_id` or generation, a missing section, or no mirror published — the planner
falls back to reading that file's footer directly from its own tail, the authoritative path,
with an identical result.

---

## ManifestEntry Metadata Layers

Each `ManifestEntry` (SuperHEF entry or HEF entry) has two layers. Harana must not maintain a narrower `ManifestEntry` schema than the active HEF/HEJ format spec; the grouping below is a non-defining semantic grouping of the full format-spec manifest contract by Harana use — byte layout, optionality, defaults, and exact field encodings stay in the format spec.

- **Required**: the full HEF/HEJ format-spec manifest contract, including HEF identity, path/SuperHEF generation, layout class, projection directory reference, rowset fingerprint, day, occurred/ingested time ranges, epoch/sequence ranges, source/type counts, row count, schema version, checksum, ordering hints, bloom/index summaries, aggregate and index permission summaries, journal watermarks, visibility watermarks, deletion-vector generation, correction-overlay generation, part state, and multi-part identity. Needed for visibility, correctness, snapshot construction, and query planning.
- **Feature metadata** (optional): period embeddings, pattern indexes, cause and revenue-anomaly snapshots, active-rule audit, alert summaries, record and association counts, monetary summaries, histograms, top records, and other feature-owned rollups. The owning service defines semantics; this layer defines placement and access only.

### Required Entry Fields

| Group | Fields | Purpose |
|---|---|---|
| Identity | `file_type` (`hef_file`, `generation_part`, or `derived_columns`), `generation_id`, `relative_part_path`, `object_store_path`, `compaction_id`, `part_index`, `hef_file_id_range` | Lifecycle, path resolution, multi-part dedup, reader catch-up. |
| Time / day | `partition_day`, `occurred_at_min/max` | Day and time pruning. |
| Pruning / cost | `row_count`, `sort_columns`, `field_cardinality`, `entity_id_bloom`, `event_stream_refs`, `entity_types`, `counts_by_stream_and_event_type` | Stream/event-type pruning, HEF pruning, cost estimation, trend/count rollups. Bloom sizing implementation-owned. |
| Integrity | `schema_version`, `checksum` | Schema dispatch and integrity checks. |
| Format-owned snapshot control | `layout_class`, `projection_directory_ref`, `rowset_fingerprint`, `ingested_at_min/max`, `epoch_min/max`, `sequence_min/max`, `aggregate_capabilities`, `index_capabilities`, `physical_sort_order`, `journal_low_watermark`, `journal_high_watermark`, `commit_watermark`, `visibility_watermark`, `snapshot_watermark_upper_bound`, `deletion_vector_generation`, `correction_overlay_generation`, `part_state` | HEF/LiveOverlay snapshot construction, projection selection, aggregate/index planning, rewrite state, and HEF Update visibility. |

### Feature Metadata Placement

Day-scoped feature metadata is written only to the compacted entry with `part_index = 0`; it is null on committed HEFs and non-zero SuperHEF entries. Query code must read it through typed manifest helpers. Per-HEF metadata is populated independently on each manifest entry.

Top-level SuperHEF manifest rollups come from entry fields and feature metadata; they answer period-level questions (daily rates, weekly monetary rollups, revenue-anomaly baselines, alert rates, cause-parameter history, cross-source co-occurrence) without opening compacted HEFs when possible. Committed HEF entries add counts/pruning metadata only; day-scoped feature rollups appear after compaction. UnifiedEvents pruning opens only HEFs that survive time, source, entity type, entity bloom, and precise `(source, event_type)` checks; HEF-open pruning follows the `ManifestPruningFilter`.

---
