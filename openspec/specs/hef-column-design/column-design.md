# HEF Column Design — Required/Promoted Columns, Payload Arena, Context and Vector Columns

Companion artifact for the `hef-column-design` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


### Required physical columns

Every HEF file must include column chunks for:

```text
epoch
sequence
stream_id
stream_sequence
occurred_at
ingested_at
source_id
event_type_id
entity_type_id
entity_id_hash
payload_ref
flags
schema_version
```

`payload_ref`, `epoch`, and `sequence` are internal scan columns. Public APIs must receive opaque cursors or public-safe references instead.

### Optional promoted physical columns

Promoted attributes become physical columns when they pass usefulness thresholds.

Examples:

```text
amount_decimal
currency
status
stage
country
region
account_id_hash
opportunity_id_hash
campaign_id_hash
customer_id_hash
latency
score
```

Promoted columns must be strongly typed. Money uses fixed-scale decimal values. Durations use `DurationValue` logically even when encoded as integers physically.

### Payload arena and variant shredding

The payload arena stores canonical `harana_variant_v1` payload values. There is exactly one payload format; source formats do not survive ingest (see the hef-logical-event-model capability).

```text
PayloadArena {
  variant_residual_blocks[]
  variant_dictionary_blocks[]
  variant_path_mphf_block optional
  offset_table
  encryption_metadata optional
  checksum_directory
}
```

`variant_dictionary_blocks` are part of the required `payload_arena` feature: one shared key dictionary per granule (or per stripe when granule dictionaries would duplicate heavily), sorted, binary-searchable, and self-contained for the rows it governs. `variant_path_mphf_block` is an optional minimal-perfect-hash acceleration mapping hot path strings to field ids in O(1); it is rebuildable and never authoritative.

Shredding rule:

```text
At HEF publication and rewrite, per-path access and type statistics select frequently
accessed payload paths for shredding.

Selected paths are lifted into typed columns in variant_shredded_field_blocks, which are separate typed column chunks stored outside the payload arena.

Shredded columns are strongly typed, carry data-class labels and authorization metadata,
and use the adaptive encoding pipeline (see the hef-encodings-and-compression capability)
with adaptive cascades (FastLanes, DELTA/FOR/bitpacking, ALP/ALP RD, FSST, dictionaries).

Shredded scan-path columns must preserve random access in compressed form; whole-block
heavyweight compression that forces full-block decode before point access is forbidden
on shredded columns.

Shredded/residual placement follows Parquet Variant shredding semantics: for each row, a
shredded path's value lives in exactly one of the typed column or the residual value.
```

Residual rule:

```text
Paths not shredded remain inside the row's residual harana_variant_v1 value in
variant_residual_blocks.

Residual values resolve field ids against the granule's variant dictionary block; field
id resolution must not require reading any other granule.

Hot granules store residual values uncompressed for offset-jump point access. Cold or
rewritten granules may apply page-level Zstd-3, declared in marks.

Encrypt single-subject payload values under the subject content key and record the
content_key_id in encryption_metadata (see the hef-security-and-isolation capability).
```

Reconstruction rule:

```text
A row's full payload is the deterministic merge of its shredded typed values and its
residual value. Merge ordering, null semantics, and missing-versus-null distinctions
follow the Parquet Variant shredding specification.
```

Payload arena granularity is adaptive. Wide-payload tenants may use smaller `index_granularity_bytes` and fewer rows per payload granule than envelope columns, while marks preserve mapping from event row to payload offset.

Payload reads are late-materialized:

```text
1. use manifest/footer/granule/indexes (including path_presence) to find candidate rows;
2. read only needed envelope, promoted, and shredded payload columns;
3. apply security and field-level checks;
4. read residual variant values only for final matching rows that require unshredded paths;
5. extract only the requested paths from residual values; never parse sibling fields;
6. redact or suppress payload fields according to the caller's authorization.
```

Reference: <https://github.com/apache/parquet-format/blob/master/VariantShredding.md>

### Per-row offset index for wide typed columns

The residual arena's per-row slot — an `(offset: u32, len: u32)` entry per row, indexed directly by the row's position within the granule — extends to schema-declared free-text (FSST-class) blocks and to internal embedding/vector blocks. A reader that has the row's offset entry reads only that row's bytes, a small bounded number of IOPs, instead of decoding the whole granule block.

The index is carried under the `typed_column_row_offsets` optional feature (see the hef-reader-compatibility capability's feature directory): a writer may emit it for free-text and embedding/vector blocks; a reader that declares the feature uses it for point access; a reader that does not falls back to the existing whole-granule decode and returns byte-identical values. Like other optional acceleration state, the index is additive and droppable — a file without it stays fully readable at whole-granule-decode cost, and rebuilding or discarding the index never changes a query result.

A block whose own stored form is per-value addressable does not need the index to be point-accessible. An FSST-class free-text block carries an offset table over its stored values, so one row's byte range is resolvable from the block itself; the index would only store, uncompressed, a second copy of values that block already holds. So the index is off by default for free-text and a build opts into it, and the two paths return byte-identical values either way:

```text
block is per-value addressable   -> the row's byte range comes from the block's own offset
   (FSST, dictionary)               table; no extra stored bytes, no whole-granule decode.

block is not addressable per      -> the reader decodes the granule's block once and shares
   value (long/opaque values)        that decode across the granule's rows; correct and
                                     bounded, but not per-value access.

file carries the per-row index   -> the row's byte range comes from the index instead, at the
   (opt-in for free-text)            cost of a second uncompressed copy of every value.
```

The index adds a point-access path beside each column family's existing bulk path rather than replacing it:

```text
free-text:  point access via the offset index sits beside the sequential bulk-egress
            range reads used for re-extraction and erasure (see "Free-text shredded by
            schema declaration" above); bulk egress does not use the index.

vector:     exact per-row fetch by row ordinal via the offset index sits beside the
            approximate ANN retrieval path; ANN candidate search does not use the
            index, and exact event-query correctness never depends on it.
```

### Context projection columns

Chat, investigations, summaries, and LLM tools must not require raw payload scans for common evidence retrieval. HEF may include compact context projections:

```text
context_title
context_summary
context_entity_label
context_source_label
context_metric_label
context_status_label
context_amount_display
context_period_ref
context_snippet_ref
context_lineage_ref
```

These columns are derived, public-safe only when authorized, and must carry data-class labels. They are optimized for evidence cards, investigation timelines, and LLM context packing.

### Internal embedding/vector columns

HEF may store internal embedding columns or quantized vector blocks for semantic retrieval, clustering, and Memory/Chat tooling.

Rules:

```text
embedding/vector blocks are HEF-native internal analytical blocks;
ordinary public event APIs must not expose them;
support bundles and exports must exclude them unless an owner-defined safe representation exists;
vector indexes must preserve tenant isolation and data-class labels;
single-subject embedding/vector blocks are encrypted under the subject content key (see the hef-security-and-isolation capability) so crypto-shredding the subject destroys them, and rebuildable vector indexes reproduce erasure-aware without the shredded subject;
exact event-query correctness must not depend on approximate vector retrieval;
embedding/vector blocks must not be moved to Puffin sidecars.
```

Exact per-row fetch of one stored vector by row ordinal (for example, late-materialized re-ranking of a single candidate) uses the per-row offset index described above under "Per-row offset index for wide typed columns" when the file carries it; this exact-fetch path is distinct from, and sits beside, the approximate ANN index that finds candidate rows.

Named techniques:

```text
quantization
  RaBitQ binary/extended quantization is the default quantized representation;
  it carries per-query unbiased error bounds, which lets the retrieval path
  report distance-estimate confidence instead of opaque approximation.

disk-resident index
  DiskANN/Vamana graph layout for vector blocks larger than the memory budget;
  graph adjacency and quantized vectors are HEF-native internal blocks; full
  vectors are late-materialized for re-ranking only.

memory-resident index
  IVF + RaBitQ for small/medium tenant partitions where a graph is not justified.

dimensionality
  Matryoshka-style truncatable embeddings are permitted so one stored vector
  serves coarse filtering (truncated prefix) and fine re-ranking (full length).

rebuild rule
  all vector indexes are rebuildable acceleration state derived from the
  committed event snapshot; index loss or erasure never loses authoritative data.
```

---
