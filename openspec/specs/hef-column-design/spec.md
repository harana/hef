## Purpose

Defines the columns stored inside an HEF file:

- The columns every file must have, and optional "promoted" columns pulled out of the payload for speed.
- The payload arena that holds canonical `harana_variant_v1` residual values plus shredded typed field blocks, the per-granule variant dictionaries, context projection columns, and internal embedding/vector columns.

The concrete required/promoted column, payload-arena, context, and vector-column definitions are embedded in [column-design.md](column-design.md).
## Requirements
### Requirement: Required physical columns
Every HEF file SHALL include column chunks for the required envelope columns (`event_id`, `epoch`, `sequence`, `stream_id`, `stream_sequence`, `occurred_at`, `ingested_at`, `source_id`, `event_type_id`, `entity_type_id`, `entity_id_hash`, `payload_ref`, `flags`, `schema_version`), matching the logical envelope, the LiveOverlay v1 schema, and the HEJ fixed record so no representation requires a column another omits. In a published HEF file, `payload_ref` SHALL be a file-scoped offset/reference addressing the row's residual `harana_variant_v1` value within that file's payload arena; this is distinct from the segment-scoped `payload_ref = (row_index << 32) | payload_offset` encoding, which is valid only inside a LiveOverlay segment. `payload_ref`, `epoch`, and `sequence` SHALL be internal scan columns; public APIs SHALL receive opaque cursors or public-safe references instead.

#### Scenario: Envelope column set matches every representation
- **WHEN** a HEF file is written
- **THEN** it includes an `event_id` column chunk alongside the other required envelope columns, matching the logical envelope, the LiveOverlay schema, and the HEJ fixed record

#### Scenario: Internal scan columns withheld
- **WHEN** a public API returns event rows
- **THEN** `epoch`, `sequence`, and `payload_ref` are replaced by opaque cursors or public-safe references

### Requirement: Strongly typed promoted columns
Promoted attributes SHALL become physical columns only when they pass usefulness thresholds, and SHALL be strongly typed. Money SHALL use fixed-scale decimal values; durations SHALL use `DurationValue` logically even when physically encoded as integers. The projection sort keys named by the `hef-layout-and-clustering` capability SHALL resolve to defined promoted columns: `evidence_group_key` (a coded grouping identifier over the context/evidence dimension), `metric_period` (a fixed-scale time-bucket key), and `metric_key` (a coded revenue-metric dimension). A file SHALL carry the `context_major` or `revenue_metric_major` projection only when it has promoted the columns that projection's sort order references.

#### Scenario: Money column typing
- **WHEN** an `amount_decimal` column is promoted
- **THEN** it is stored as a fixed-scale decimal value rather than a float

#### Scenario: Context projection requires its promoted sort columns
- **WHEN** a file declares a `context_major` projection
- **THEN** it has promoted the `evidence_group_key` column its sort order references, and a file lacking that column does not declare the projection

### Requirement: Payload arena stores canonical variant values with statistics-driven shredding
The payload arena SHALL store canonical `harana_variant_v1` payload values (there is exactly one payload format; source formats do not survive ingest). At HEF publication and rewrite, per-path access/type statistics SHALL select frequently accessed payload paths and lift them into strongly typed columns in `variant_shredded_field_blocks` that carry data-class/authorization labels, use the adaptive encoding pipeline, and preserve random access in compressed form (whole-block heavyweight compression that forces full-block decode before point access SHALL be forbidden on shredded scan-path columns). Paths not shredded SHALL remain in the row's residual `harana_variant_v1` value in `variant_residual_blocks`, resolving field ids against the granule's `variant_dictionary_block` without reading any other granule; hot granules SHALL store residual values uncompressed for offset-jump point access. For each row a shredded path's value SHALL live in exactly one of the typed column or the residual value (Parquet Variant shredding semantics), and a row's full payload SHALL be the deterministic merge of the two. The shredded-field block type SHALL be named `variant_shredded_field_blocks` in every capability and companion; `variant_payload_field_blocks` SHALL NOT be used as a synonym. The payload arena SHALL contain the residual (`variant_residual_blocks`) and dictionary (`variant_dictionary_block`) blocks, while `variant_shredded_field_blocks` SHALL be separate typed column chunks outside the arena. Payload reads SHALL be late-materialized — residual values read only for final matching rows after pruning, envelope/promoted/shredded-column reads, and authorization/filter/deletion-vector/correction application, extracting only the requested paths and never parsing sibling fields, then redacting per the caller's authorization.

#### Scenario: Hot path shredded into a typed column
- **WHEN** per-path statistics show a payload path is frequently filtered or projected
- **THEN** it is lifted into a strongly typed `variant_shredded_field_block` at publication/rewrite, that path's value is removed from the residual value, and the column preserves random access in compressed form

#### Scenario: Rare path read from residual without sibling decode
- **WHEN** a query needs an unshredded payload path
- **THEN** the `QueryEngine` reads the residual `harana_variant_v1` value only for final matching rows, resolves field ids against the granule variant dictionary, and extracts only the requested path without parsing sibling fields

### Requirement: Schema-version-keyed presence map for promoted columns
A `(tenant, day)` HEF file mixes streams and spans intra-stream schema promotions, so column absence SHALL be handled by separating two heterogeneities. Cross-stream sparsity (a column promoted by one stream and not applicable to rows of other streams) SHALL read as genuine NULL with no special handling. Intra-stream promotion SHALL be tracked by a `schema_version`-keyed presence map in the footer recording, per promoted column, the schema version `V_promote` at and after which the column is materialized. A query reading a promoted column SHALL check each granule's `schema_version` min/max: for granules entirely at or above `V_promote` it SHALL read the column directly (NULL is a real NULL); for granules predating `V_promote` it SHALL fall back to the `variant_shredded_field_blocks` block or an authorized payload scan for just those granules, and SHALL NOT silently return NULL for a value that may exist in the payload.

#### Scenario: Granule predates promotion
- **WHEN** a query reads a promoted column over a granule whose `schema_version` range predates the column's `V_promote`
- **THEN** the `QueryEngine` falls back to the variant shredded field blocks or an authorized payload scan for that granule rather than returning NULL

#### Scenario: Cross-stream sparsity reads as NULL
- **WHEN** a query reads a column promoted by stream X over rows of streams that never had the field
- **THEN** those rows read as genuine NULL without any payload fallback

### Requirement: Promotion backfill via manifest-native vertical projection
A newly promoted column SHALL be embedded in new HEF files at write time. Historical backfill SHALL NOT rewrite immutable HEF files; it SHALL be published as a manifest-native vertical projection — a column-subset HEF-format file with the same sort order as the base, row-aligned by ordinal, with its own marks, skip indexes, and aggregates — carrying the same deletion-vector and correction generations as the base and published atomically in one manifest generation. The next SuperHEF compaction SHALL fold the vertical projection into the rewritten base file and drop the sidecar. While neither embedded column nor vertical projection exists for a range, the planner SHALL fall back to the payload per the presence-map rules so queries stay correct.

#### Scenario: Historical backfill without payload rewrite
- **WHEN** a column is promoted and historical files need backfill
- **THEN** a manifest-native vertical projection is published for the historical range without rewriting the base files' payload arenas

#### Scenario: Compaction folds the sidecar
- **WHEN** SuperHEF compaction rewrites a base range that has a vertical projection
- **THEN** the promoted column is embedded in the new base file and the vertical projection is dropped

### Requirement: Free-text shredded by schema declaration
Schema-declared free-text payload fields SHALL be shredded into their own columnar family at write time by declaration, not by access statistics, because their consumers (whole-corpus re-extraction during release migration and per-subject erasure) never generate query-path access signals. The free-text family SHALL be stored in its own blocks with text-tuned compression (FSST-class) so bulk single-field reads range-read only free-text blocks sequentially without touching the residual variant arena, and the blocks SHALL be individually cacheable.

#### Scenario: Re-extraction reads only free-text blocks
- **WHEN** a release migration re-runs extraction over historical free-text
- **THEN** the job range-reads only the free-text column blocks sequentially, not the residual variant arena

### Requirement: Context projection columns avoid raw payload scans
HEF MAY include compact context projection columns (title, summary, entity/source/metric/status labels, amount display, period/snippet/lineage refs) so chat, investigations, summaries, and LLM tools do not require raw payload scans for common evidence retrieval. These columns SHALL be derived, public-safe only when authorized, and SHALL carry data-class labels.

#### Scenario: Evidence card from context columns
- **WHEN** an investigation requests an evidence card
- **THEN** it is assembled from authorized context projection columns rather than a raw payload scan

### Requirement: Internal embedding/vector columns isolated from public output
HEF MAY store internal embedding columns or quantized vector blocks (e.g. RaBitQ quantization with DiskANN/Vamana or IVF layouts, optionally Matryoshka-truncatable) for semantic retrieval and tooling. Ordinary public event APIs SHALL NOT expose them, support bundles and exports SHALL exclude them unless an owner-defined safe representation exists, vector indexes SHALL preserve tenant isolation and data-class labels, and exact event-query correctness SHALL NOT depend on approximate vector retrieval. Single-subject embedding/vector blocks SHALL be encrypted under the subject content key so that crypto-shredding the subject destroys them, and all vector indexes SHALL be rebuildable acceleration state derived from the committed event snapshot whose loss or erasure never loses authoritative data.

#### Scenario: Export excludes embeddings
- **WHEN** a support bundle or export is generated
- **THEN** internal embedding/vector blocks are excluded unless an owner-defined safe representation exists

#### Scenario: Crypto-shredding destroys subject vectors
- **WHEN** a subject's content key is destroyed for erasure
- **THEN** that subject's single-subject embedding/vector blocks become unrecoverable and rebuildable vector indexes are reproduced erasure-aware without the shredded subject

