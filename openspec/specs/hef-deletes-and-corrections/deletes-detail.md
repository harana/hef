# HEF Deletes, Corrections, and Late Events

Companion artifact for the `hef-deletes-and-corrections` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


HEF files are immutable. Deletes and corrections are represented by immutable metadata plus rewrite, never by mutating published HEF data pages.

### HEF-native deletion vectors

Deletes are represented as Iceberg v3-compatible binary deletion-vector semantics, stored as HEF-native blocks or manifest-native entries. Puffin sidecars are not used.

Deletion-vector identity:

```text
DeletionVectorRef {
  target_file_id
  target_projection_id optional
  target_sequence_range
  deletion_vector_generation
  encoding = roaring_binary_deletion_vector_v1
  row_position_domain = primary_rowset_ordinal
  deleted_count
  block_ref or manifest_ref
  blake3
}
```

Multi-subject erasure uses a field-level deletion vector, which redacts specific data-class-labeled fields of specific rows rather than removing whole rows:

```text
FieldDeletionVectorRef {
  target_file_id
  target_projection_id optional
  target_sequence_range
  deletion_vector_generation
  redacted_columns[]            // data-class-labeled fields to redact on rebuild
  row_position_domain = primary_rowset_ordinal
  redacted_count
  block_ref or manifest_ref
  blake3
}
```

Required fields per delete event or delete record:

```text
event_id
epoch
sequence
delete_reason
deleted_at: TimestampValue
deletion_vector_generation
```

Rules:

```text
Deletion vectors are immutable once published.
Deletion vectors select row positions in the primary rowset ordinal domain.
Projection row positions must map through the projection row map or shared granule rowset.
A query must apply all visible row-level and field-level deletion vectors before returning rows.
A field-level deletion vector redacts only its listed columns for its row positions, so a multi-subject event survives for co-mentioned subjects while the forgotten subject's fields are removed on rebuild.
Erasure-aware rebuild renders a block whose per-subject content key has been destroyed (see the hef-security-and-isolation capability) as a tombstone and reproduces rebuildable accelerators without the shredded subject.
Each published deletion vector must co-publish an immutable, checksum-verified DeletionAggregateDelta block (see the hef-aggregation-metadata capability) carrying its deleted rows' contribution to all invertible aggregates, computed once from the immutable file and deletion vector. Aggregate shortcuts subtract that block for invertible aggregates and apply the granule-extreme rule for MIN/MAX; only when neither covers a needed aggregate does the query fall back to scan.
HEF rewrite may physically remove deleted events only when retention, legal hold, and correctness rules allow it; until then aggregate correctness comes from the DeletionAggregateDelta block and the granule-extreme rule, never from rewrite.
```

### Corrections

Corrections are represented as new events plus supersession metadata:

```text
corrects_event_id
correction_epoch
correction_sequence
correction_type
correction_generation
```

The query layer decides whether to show raw history or latest-corrected view. Aggregate shortcuts handle a correction as delete-old plus add-new (see the hef-aggregation-metadata capability): the delete-old half reuses the superseded event's DeletionAggregateDelta and the add-new half is the correcting event's own aggregate contribution, so no separate correction-aggregate block is required.

Corrections may create deletion-vector entries for the superseded event when the selected view is latest-only. Raw-history views keep both events visible unless a true delete vector removes one.

### Late events

Late events are accepted.

Rules:

```text
ingest order is epoch + sequence
event time is occurred_at
queries over occurred_at must include late-event files and LiveOverlay ranges
HEF rewrite eventually reclusters late events into the selected time-oriented projection
```

Live inclusion is by snapshot_watermark first, then event-time pruning.

---
