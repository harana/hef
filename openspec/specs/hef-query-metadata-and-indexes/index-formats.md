# HEF Query Metadata and Indexes — Metadata Hierarchy, SkipIndex, Bloom, and Bitmap Policy

Companion artifact for the `hef-query-metadata-and-indexes` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


HEF includes multiple metadata layers. The goal is to avoid file opens, then avoid footer work, then avoid stripe reads, then avoid granule reads, then avoid column/page reads, then avoid payload reads.

### Metadata hierarchy

```text
Manifest planning metadata:
  tiny per-file/per-projection summaries copied from HEF footer metadata;
  used to avoid opening files at all.

HEF header:
  fixed-size coarse rejection fields;
  used before footer range reads.

HEF footer:
  authoritative file-level metadata, directories, aggregate index, schema, checksums,
  projection directory, deletion-vector directory, and granule directory.

HEF granule directory:
  authoritative pruning and parallel-scan unit between stripe and page;
  maps sequence-ordered row ranges to marks and local metadata.

HEF marks directory:
  per-column, per-projection mapping from granule id to compressed offsets.

HEF page/mini-block metadata:
  local decode metadata below marks.

HEF index blocks:
  SkipIndex<kind, granularity>, bitmap, text, context, and vector locator structures.

PreparedView catalogue:
  semantic product metric readiness and materialization metadata.
```

Do not put all rollups directly in the manifest. Large rollups belong in HEF aggregate blocks or PreparedView outputs. The manifest carries only small summaries and pointers/capabilities.

### Manifest-copied file summaries

Used by the external manifest and coarse pruning:

```text
row_count
min/max occurred_at
min/max ingested_at
min/max epoch/sequence
source_id set or compact membership summary
event_type_id set or compact membership summary
entity_type_id set or compact membership summary
promoted column list
measure column list
aggregate_capabilities
index_capabilities
layout_class
projection_capabilities
physical_sort_order
has_deletion_vectors
has_late_events
file_blake3
granule_count
```

These summaries are not a substitute for HEF footer validation.

### Footer metadata

Footer metadata is the authoritative file-local planning source:

```text
format_version
required_feature_flags
optional_feature_flags
schema_fingerprint
logical_schema
physical_schema
column_directory
stripe_directory
granule_directory
marks_directory
projection_directory
skip_index_directory
bitmap_index_directory
aggregate_directory
sketch_directory
context_directory optional
embedding_directory optional internal only
deletion_vector_directory optional
encryption_metadata
checksum_directory
file_blake3
```

### Granule-level metadata

Used to avoid reading pages and to schedule parallel work:

```text
granule_id
stripe_id
row_count
first/last row ordinal
first/last epoch/sequence
min/max occurred_at
min/max ingested_at
null_count summary for selected columns
source/type/entity_type dictionaries or value-set refs
aggregate coverage summary
skip index refs
mark refs per column
payload granule refs
context projection availability
```

### Stripe-level metadata

Used to avoid reading stripes:

```text
row_count
granule_id range
min/max per ordered column
null_count per column
distinct_count estimate
source/type/entity_type dictionaries
entity_id filter summary
account_id filter summary
actor_id filter summary
event_id filter summary
promoted column min/max
low-cardinality value sets
aggregate coverage summary
context projection availability
```

### Page-level and mini-block metadata

Used inside selected granules:

```text
row_count
granule_id
offset
compressed_size
uncompressed_size
codec_pipeline_id
min/max
null_count
first epoch/sequence
last epoch/sequence
first occurred_at
last occurred_at
page_blake3 or page-range BLAKE3 proof
```

CRC-64/NVME is not required at page level. BLAKE3 is authoritative.

### Uniform SkipIndex model

HEF generalizes indexes into `SkipIndex<kind, granularity>` where possible.

```text
SkipIndex {
  index_id
  column_id
  projection_id
  kind
  granularity: file | stripe | granule | page | mini_block
  exactness: exact | inexact_no_false_negative
  fpr optional
  row_range_ref
  block_ref
  checksum_ref
}
```

Required kinds:

```text
minmax
  replaces ZoneMapIndex; min/max/null_count over selected columns.

sequence_range
  epoch/sequence range -> granule/page ranges.

time_range
  occurred_at or ingested_at bucket/range -> granule/page ranges.

learned_position
  compact piecewise model (PGM/RMI-style) mapping a sorted key to its approximate
  row position with a recorded error bound; the reader seeks near the start row and
  confirms the boundary with a bounded local search. Optional and feature-gated.

ribbon_filter
  high-cardinality equality membership where supported.

split_block_bloom_filter
  high-cardinality equality membership fallback.

binary_fuse_filter
  static high-cardinality equality membership for immutable HEF blocks;
  build-once 3-wise or 4-wise binary fuse with ~1.08-1.13 bits/key overhead at
  matched false-positive rate; never used for mutable or incremental state.

range_filter
  succinct range-emptiness filter over sortable keys (entity_id_hash buckets,
  occurred_at buckets, sequence sub-ranges); answers "does this granule/page
  contain any key in [lo, hi]" with no false negatives.
  Static HEF blocks use a Grafite-style succinct range filter.
  LiveOverlay segments may use a Memento-style dynamic range filter.
  exactness is always inexact_no_false_negative.

entity_hash_filter
  entity_id_hash -> granule/page ranges; implemented as ribbon_filter or split_block_bloom_filter with granularity 1 where warranted.

account_hash_filter
  account_id_hash -> granule/page ranges.

opportunity_hash_filter
  opportunity_id_hash -> granule/page ranges.

context_locator
  investigation/evidence grouping key -> candidate rows/context projection ranges.

text_token
  token bloom or compact inverted index for selected summary/search fields.

path_presence
  payload path field_id -> granule/page presence; prunes residual-value reads for
  queries over rare paths; exact or inexact_no_false_negative per block.
```

Bitmap indexes remain distinct because they are exact compressed row-selection structures, not only skip summaries.

Embedding vector blocks remain distinct because they are approximate internal retrieval structures, not exact skip indexes.

### Ribbon, split-block Bloom, binary fuse, and range filter policy

Probabilistic filters are workload-adaptive, not automatic for every column.

Use Ribbon, binary fuse, or split-block Bloom filters for:

```text
high-cardinality equality predicates;
entity/account/actor/event/opportunity/customer lookups;
columns with high prune value and bounded metadata cost.
```

Selection rule:

```text
Binary fuse filter is preferred for immutable HEF point-membership blocks when the key set is fully known at build time; it is simpler than Ribbon, builds in one pass, and reaches ~1.08 bits/key overhead.

Ribbon filter is preferred when writer CPU and implementation support are available and binary fuse construction is unavailable or the target false-positive rate is met with lower bytes.

Split-block Bloom is the required fallback because it is simple, SIMD-friendly, and range-read friendly.

Classic unstructured Bloom bitsets are not a default and may be written only as a measured compatibility fallback.
```

Range-filter rule:

```text
Use range_filter SkipIndexes when min/max pruning is weak because granule key
ranges overlap heavily (high-churn entity workloads, interleaved tenants in a
shared epoch, wide occurred_at spread per granule).

Grafite-style succinct range filters are the static HEF representation; they give
guaranteed false-positive bounds for range-emptiness queries, unlike prefix-Bloom
or SuRF-style tries whose range FPR is input-sensitive.

Memento-style dynamic range filters are permitted only in LiveOverlay where the
key set grows; HEF rewrite replaces them with the static Grafite-style form.

Point-membership filters must not be used to answer range predicates.
```

Avoid probabilistic filters for:

```text
low-cardinality fields better served by bitmaps or value sets;
columns rarely used in filters;
columns with extreme churn where metadata cost exceeds scan savings;
raw arbitrary payload paths unless promoted or shredded into variant_shredded_field_blocks.
```

Probabilistic filters may have false positives but no false negatives. Pushdown using them is inexact unless combined with an exact predicate check above or inside the scan.

### Bitmap index policy

Use exact bitmap indexes for low-cardinality dimensions:

```text
source_id
event_type_id
entity_type_id
status
stage
currency
country
region
boolean flags
selected revenue lifecycle states
```

Bitmaps must be compressed and directly intersectable without materializing row ids. If an encoding cannot intersect compressed bitmaps directly, the planner must not use that bitmap path. Bitmap blocks are optional HEF feature blocks. The planner may use bitmap acceleration only when the file declares the bitmap feature and the bitmap block checksum verifies.

---
