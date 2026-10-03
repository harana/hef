## Purpose

Defines how HEF handles deletes, corrections, and late-arriving events without ever editing files already written:

- Deletes are recorded as separate immutable deletion vectors, not by removing data.
- A correction is a new event that supersedes the old one.
- Late events become visible by sequence order rather than wall-clock time.

The concrete deletion-vector, correction, and late-event detail are embedded in [deletes-detail.md](deletes-detail.md).
## Requirements
### Requirement: Deletes via immutable deletion vectors
HEF files SHALL be immutable; deletes SHALL be represented using Iceberg v3 deletion-vector *semantics* (positional roaring bitmaps) in a HEF-native container — stored as HEF-native blocks or manifest-native entries, not the Iceberg Puffin wire container and no Puffin sidecars — never by mutating published data pages. HEF shares the deletion-vector *semantics* with Iceberg v3, not its wire format: an external Iceberg reader is not expected to consume HEF deletion vectors, and HEF SHALL NOT claim Puffin wire compatibility. Row-level deletion vectors SHALL be immutable once published, SHALL select row positions in the primary-rowset ordinal domain (projection positions mapping through the projection row map), and SHALL carry the deletion-vector identity fields (`target_file_id`, optional `target_projection_id`, `target_sequence_range`, `deletion_vector_generation`, encoding, `row_position_domain = primary_rowset_ordinal`, `deleted_count`, a block or manifest ref, and `blake3`). Multi-subject erasure SHALL use a field-level deletion vector (`FieldDeletionVectorRef` with `redacted_columns[]`) that redacts only its listed data-class-labeled fields for its row positions, so a multi-subject event survives for co-mentioned subjects while the forgotten subject's fields are removed on rebuild. A query SHALL apply all visible row-level and field-level deletion vectors before returning rows. Each published deletion vector SHALL co-publish an immutable BLAKE3-verified `DeletionAggregateDelta` block (BLAKE3 is the sole integrity authority); aggregate shortcuts SHALL subtract that block for invertible aggregates and apply the granule-extreme rule for MIN/MAX, falling back to scan only when neither covers a needed aggregate. HEF rewrite MAY physically remove deleted events only when retention, legal hold, and correctness rules allow.

#### Scenario: Deleted rows excluded
- **WHEN** a query reads a file with a visible deletion vector
- **THEN** the deleted row positions are excluded before rows are returned

#### Scenario: Field-level redaction keeps co-mentioned subjects
- **WHEN** a `FieldDeletionVectorRef` lists a forgotten subject's PII columns on a multi-subject event
- **THEN** only those columns are redacted on rebuild and the event survives for the co-mentioned subjects

#### Scenario: Aggregate over deleted rows
- **WHEN** an exact aggregate shortcut covers rows affected by a deletion vector
- **THEN** the shortcut subtracts the co-published `DeletionAggregateDelta` for invertible aggregates and applies the granule-extreme rule for MIN/MAX, falling back to a scan only when neither covers a needed aggregate

#### Scenario: No external Iceberg reader expectation
- **WHEN** an integrator inspects HEF's deletion-vector format
- **THEN** it finds Iceberg v3 positional-roaring-bitmap semantics in a HEF-native container with no Puffin sidecar, and does not rely on an external Iceberg reader consuming the HEF deletion vector

### Requirement: Corrections as superseding events
Corrections SHALL be represented as new events plus supersession metadata (`corrects_event_id`, correction epoch/sequence, type, generation). The query layer SHALL decide whether to show raw history or the latest-corrected view; aggregate shortcuts SHALL handle a correction as delete-old plus add-new (the delete-old half reusing the superseded event's `DeletionAggregateDelta` and the add-new half being the correcting event's own aggregate contribution), so no separate correction-aggregate block is required. A latest-only view MAY create a deletion-vector entry for the superseded event, while raw-history views keep both events visible unless a true delete vector removes one.

#### Scenario: Latest-corrected view
- **WHEN** a query selects the latest-corrected view and an event has been corrected
- **THEN** the superseded event is suppressed (via supersession/deletion-vector) and the correction is shown

### Requirement: Corrections and deletes invalidate model-derived columns
A correction or delete SHALL, in addition to its base-file effects, invalidate the model-derived (Tier B) column values for the affected event and for its model-neighbor rows (the producer-declared dependency closure: e.g. Hawkes parent/children, co-cluster members, trailing-window co-members) and enqueue recompute jobs for them. Until recompute lands, invalidated rows SHALL read as explicit pending under the per-column materialization watermark — never the stale pre-correction value and never silently NULL — and a scan SHALL NOT be used to recompute model-derived values on the query path.

#### Scenario: Stale score never served after correction
- **WHEN** a correction supersedes an event whose published anomaly score has model-neighbors
- **THEN** the affected derived values read as pending until the enqueued recompute republishes them, and no query serves the invalidated numbers

### Requirement: Late events visible by sequence order
Late events SHALL be accepted; ingest order is `epoch + sequence` while event time is `occurred_at`. Queries over `occurred_at` SHALL include late-event files and LiveOverlay ranges, with live inclusion determined by `snapshot_watermark` first and event-time pruning second. HEF rewrite SHALL eventually recluster late events into the selected time-oriented projection.

#### Scenario: Late event with old occurred_at
- **WHEN** a late event with an old `occurred_at` is durable
- **THEN** it is included by snapshot watermark and then pruned by event time, not excluded by recency

### Requirement: Per-subject erasure via crypto-shredding
Right-to-erasure (e.g. GDPR Art. 17 / CCPA) SHALL be satisfied by crypto-shredding rather than mutating immutable HEJ frames or published HEF files. Raw event payloads SHALL be encrypted in the payload arena under per-data-subject content keys (tiered: a per-subject key for single-subject records, with multi-subject events relying on per-field encryption/redaction keyed by the data-classification PII labels), and erasing a subject SHALL destroy that subject's content key so the bytes become unrecoverable everywhere they persist (HEJ, HEF, and backups) within the legal deadline, independent of HEF-rewrite timing. Subject keys SHALL be assigned at extraction/resolution time; payloads ingested before subject resolution SHALL remain under the tenant key with a durable erasure-reconciliation backlog that re-keys them once the subject is resolved. A query SHALL apply deletion vectors for query-time hiding, but a shredded subject's bytes SHALL NOT be served even where a deletion vector is absent. Rebuild SHALL be erasure-aware: a payload whose subject key has been destroyed SHALL rebuild as a tombstone (its non-erased structural metadata MAY remain) and SHALL NOT block or corrupt replay.

#### Scenario: Erased subject after key destruction
- **WHEN** a subject's content key has been destroyed and a query or rebuild encounters that subject's payload bytes
- **THEN** the bytes are unrecoverable and the payload rebuilds as a tombstone rather than being served or blocking replay

#### Scenario: Multi-subject event keeps co-mentioned data
- **WHEN** a forgotten subject is crypto-shredded from a multi-subject event
- **THEN** only that subject's PII fields are rendered unrecoverable/redacted and the event survives for co-mentioned subjects

### Requirement: Erasure as severance and de-identification, per jurisdiction
Erasure SHALL be **severance and de-identification, not annihilation**, governed by an entity classification and a per-jurisdiction profile. Canonical entities SHALL be classified as **data subjects** (natural persons: `Person`/`Contact`/`Lead`), **business/financial records** (`Order`/`Invoice`/`Deal`/`Subscription`), or **legal entities** (`Account`). Erasing a data subject SHALL crypto-shred that subject's identity (including its UnifiedDataService candidate keys and embeddings); business/financial records that reference the subject SHALL be **de-identified and retained** (null the personal foreign key and redact personal fields, keeping the transaction) where a lawful retention basis such as tax/accounting applies; legal entities are not data subjects and SHALL be untouched. Revenue and analytics computed over retained transactions SHALL survive de-identified.

A **jurisdiction erasure profile** SHALL declare the disposition per entity class — `crypto_shred`, `physical_destruction`, `restrict_suppress`, or `retain`. Under the never-purge/crypto-shred substrate each disposition SHALL have a defined mechanism: `crypto_shred` SHALL destroy the subject's content key so the bytes become unrecoverable in place; `physical_destruction` SHALL crypto-shred the subject's content key immediately (satisfying the legal deadline) and additionally mark the underlying immutable bytes for physical removal at the next HEF rewrite once retention, legal hold, and correctness rules allow, so byte destruction is guaranteed rather than deferred indefinitely; `restrict_suppress` SHALL suppress the subject's rows and fields from all query results via row- and field-level deletion vectors and access-restrict them while retaining the bytes under an unshredded key, for regimes that mandate retention-with-restriction rather than destruction; `retain` SHALL leave the record in place. A profile SHALL NOT assign `physical_destruction` or `restrict_suppress` without the mechanism above being available. Profiles SHALL be closed, versioned, System-Admin-governed, shipped in the release bundle, and validated; only the **assignment** of jurisdiction is configurable. Applicable jurisdiction SHALL follow the **data subject** (residency; it MAY resolve to a set of overlapping regimes); when regimes conflict, **retention carve-outs SHALL NOT be overridden, and otherwise the most-protective disposition SHALL win**. The applied jurisdiction-profile version SHALL be pinned into the erasure event so erasure-aware reprojection reproduces the exact, auditable outcome.

#### Scenario: Transaction de-identified, not deleted
- **WHEN** a data subject is erased but referenced by an invoice under a tax-retention obligation
- **THEN** the invoice is retained with the personal foreign key nulled and personal fields redacted rather than deleted, and revenue still aggregates de-identified

#### Scenario: Conflicting jurisdictions resolved
- **WHEN** a subject falls under one regime mandating broad erasure and another mandating retention of financial records
- **THEN** the retained records keep their lawful-retention disposition and everything else is shredded (most-protective wins, subject to non-overridable retention carve-outs)

#### Scenario: Physical destruction crypto-shreds now and destroys bytes at rewrite
- **WHEN** a jurisdiction profile assigns `physical_destruction` to an erased subject
- **THEN** the subject's content key is destroyed immediately and the underlying immutable bytes are scheduled for physical removal at the next HEF rewrite once retention and legal hold clear

#### Scenario: Restrict-suppress retains but never serves
- **WHEN** a jurisdiction profile assigns `restrict_suppress` to an erased subject
- **THEN** the subject's rows and fields are suppressed from all query results via deletion vectors and access-restricted, while the bytes are retained under an unshredded key

