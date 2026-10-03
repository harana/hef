# HEF File Layout — Header, Stripe/Granule/Page Model, Directories, and Projections

Companion artifact for the `hef-file-layout` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


```text
+--------------------------------------------------+
| 4 KiB File Header                                |
+--------------------------------------------------+
| Primary rowset                                   |
|   Stripe 0                                       |
|     Granules                                     |
|     Column pages / mini-blocks                   |
|     Payload arena chunks                         |
|     Local SkipIndex blocks                       |
|     Local aggregate blocks                       |
|     Optional context / text / embedding blocks   |
|   Stripe N                                       |
+--------------------------------------------------+
| Optional projection rowsets                      |
|   Projection data, marks, indexes, aggregates    |
+--------------------------------------------------+
| File-level dictionaries                          |
+--------------------------------------------------+
| File-level index blocks                          |
+--------------------------------------------------+
| File-level aggregate / rollup / sketch blocks    |
+--------------------------------------------------+
| HEF-native deletion-vector blocks                |
+--------------------------------------------------+
| Optional file-level context / embedding blocks   |
+--------------------------------------------------+
| Footer metadata                                  |
+--------------------------------------------------+
| Footer length u32/u64                            |
+--------------------------------------------------+
| Magic "HEF1"                                     |
+--------------------------------------------------+
```

HEF keeps row-group semantics through sequence-ordered stripes and granules. It does not adopt an arbitrary self-describing layout tree. Block-level encodings live inside variant_shredded_field_blocks and other column pages; they do not change HEF stripe/granule/marks layout semantics.

### Header

The header must be fixed-size and aligned.

```text
HEFHeader {
  magic:                 "HEF1"
  version_major:         u16
  version_minor:         u16
  header_len:            u32
  file_id:               u128
  tenant_id:             u128 internal only
  generation_id:         u64
  layout_class:          compact | wide
  projection_count:      u32
  created_at:            TimestampValue physical-encoded
  min_occurred_at:       TimestampValue physical-encoded
  max_occurred_at:       TimestampValue physical-encoded
  min_ingested_at:       TimestampValue physical-encoded
  max_ingested_at:       TimestampValue physical-encoded
  min_epoch:             u64
  max_epoch:             u64
  min_sequence:          u64
  max_sequence:          u64
  row_count:             u64
  feature_flags:         u64
  footer_pointer_hint:   u64 optional
  header_crc64:          u64
  header_blake3:         [u8; 32]
}
```

The header allows quick rejection of irrelevant files before reading the footer. The header is not the authoritative aggregate source; it is a coarse rejection accelerator.

### Stripe, granule, page, and mini-block model

A stripe is the main row-order unit. A granule is the minimum unit of pruning and parallel scan.

Required targets:

```text
stripe target:                    ~256 MiB uncompressed
minimum normal stripe target:      64 MiB uncompressed
maximum stripe size:               512 MiB uncompressed
index_granularity:                 8192 rows
index_granularity_bytes:           10 MiB compressed
page/chunk target:                 variable per column, usually 64-256 KiB compressed
mini-block target:                 4-64 KiB compressed where random access matters
maximum page/chunk size:           1 MiB compressed
```

Granule rule:

```text
Granule row count = min(index_granularity rows, rows that fit index_granularity_bytes compressed).

A granule contains a contiguous, sequence-ordered row range within one stripe.

Envelope columns share the same granule row positions.

Per-column pages may be smaller or larger than a granule when marks preserve constant-time mapping from granule id to compressed offsets.

Payload arena granules may adapt to lower row counts for wide-payload tenants so one payload lookup does not fan out across many unrelated payload pages.
```

A writer must close the current HEF file before any stripe exceeds the maximum stripe size. This stripe clamp is a pure safety backstop, not the primary roll trigger — the file boundary is owned by publish policy (the dual byte/time roll trigger in the write path). A reader must reject a HEF file whose stripe or page/chunk exceeds the maximum size unless the file declares a known future required feature flag and the reader supports that feature.

Each stripe contains:

```text
Stripe {
  StripeHeader
  GranuleDirectory
  ColumnPages[]
  PayloadArenaPages[]
  MarksDirectoryRef
  SkipIndexes[]
  BitmapIndexes[]
  TextTokenIndex optional
  EmbeddingVectorBlock optional internal only
  ContextProjectionBlock optional
  AggregateBlocks[]
  StripeTrailer
}
```

### Granule directory and marks

Every HEF file must contain a footer-visible granule directory.

```text
GranuleDirectoryEntry {
  granule_id
  stripe_id
  first_row_ordinal
  row_count
  first_epoch
  first_sequence
  last_epoch
  last_sequence
  min_occurred_at
  max_occurred_at
  min_ingested_at
  max_ingested_at
  compressed_bytes_estimate
}
```

Every required and promoted column must have marks for every granule in which it is materialized per the schema-version-keyed presence map; for granules predating a column's promotion schema version, readers fall back to the variant payload blocks or an authorized payload scan rather than treating the column as NULL.

```text
ColumnMark {
  column_id
  projection_id
  granule_id
  compressed_offset
  compressed_size
  uncompressed_offset
  uncompressed_size
  row_count
  page_count
  codec_pipeline_id
  first_value_offset optional
}
```

Marks are the authoritative random-access directory for `(column, projection, granule)`. Page metadata is local detail below marks, not a substitute for marks.

### Columnar marks and two-level per-stripe marks section

When a file declares the `columnar_marks` required feature, marks are stored columnar rather than as row-oriented `ColumnMark` structs: for each `(projection, column)` the footer holds parallel arrays of the mark fields (`compressed_offset`, `compressed_size`, `row_count`, and the other fixed-width fields), each array encoded with the format's own integer encodings (FastLanes / DELTA — offsets are monotonic, sizes are small integers, exactly as a data column would be encoded). Decoding the columnar arrays for a `(projection, column, granule)` produces the identical logical directory entry the row-oriented struct form would have produced; only the physical encoding changes. `columnar_marks` is required and refuse — a reader that does not understand it must not serve a file that declares it — and a writer may dual-emit both the row-oriented and columnar forms during migration.

The marks section itself is two-level and per-stripe: the footer's marks directory holds one entry per `(projection, column, stripe)`, each pointing at an independently-fetchable marks page (co-located with the stripe or held in the footer region) rather than one flat directory spanning the whole file. A stripe's granule-level marks are parsed only when that stripe survives pruning; a pruned stripe is never fetched or decoded and so costs zero marks bytes, whereas the row-oriented, single-level form charged every file its full share of marks bytes at open regardless of which stripes a query touched.

The per-page directory (per-page compressed/uncompressed offset and length, row range, and per-page stats) is stored columnar within this same per-stripe marks page and, like the granule-level marks, is parsed only for stripes that survive pruning.

### Stripe-relative offset domain

When a file declares the `stripe_relative_addressing` required feature, intra-stripe offsets are **stripe-relative** rather than file-absolute: each `ColumnMark` compressed/uncompressed offset, each `PageDirectoryEntry` offset, and each `PayloadGranule` dictionary/offsets-table/residual offset is measured from the base offset of the owning stripe, not from the start of the file.

`StripeEntry.file_offset` — the stripe base-offset directory — is the single file-absolute anchor those relative offsets are measured from. A block's file-absolute position is:

```text
file_absolute_position = stripe_base_offset + stripe_relative_offset
```

Relocating a stripe therefore rewrites only that stripe's one base-offset directory entry; no mark, page entry, or payload offset changes, and the stripe's per-stripe BLAKE3 stays valid because the stripe's bytes are unchanged.

A file that does not declare `stripe_relative_addressing` keeps the legacy file-absolute offset domain for these same offsets, and the stripe base-offset directory itself is always file-absolute regardless of the flag. `stripe_relative_addressing` is required and refuse: a reader that does not recognize it must not serve the file, since reading a stripe-relative offset as file-absolute would resolve to the wrong bytes.

### Pinned footer serialization

The footer's byte layout is fixed by the `hef-file-layout` spec, not left to the implementation: two independent implementations must agree byte-for-byte on the footer they serialize for the same logical directory content.

The footer blob is `[preamble][section directory][section bytes …]`, closed by `footer_len` (`u64`) and the 4-byte magic `HEF1` at the file tail. All fixed-width integers are little-endian.

```text
FooterPreamble {
  version_major:            u16
  version_minor:            u16
  required_feature_flags:   u64
  optional_feature_flags:   u64
  schema_fingerprint:       [u8; 32]
  section_count:            u32
}
```

Each section-directory entry is exactly 52 bytes and doubles as the checksum directory; a section's checksum must verify before the section is used:

```text
SectionDirectoryEntry {
  section_id:        u32
  section_offset:    u64  relative to the start of the section area
  section_length:    u64
  blake3_checksum:   [u8; 32]
}
```

The set of section ids is closed and pinned:

```text
COLUMNS = 1               STRIPES = 2               GRANULES = 3
MARKS = 4                 DICTIONARIES = 5          PAGE_STATS = 6
EXACT_COUNTS = 7          PAYLOAD_GRANULES = 8      PRESENCE = 9
SHREDDED = 10             FREETEXT = 11             STRIPE_CHECKSUMS = 12
ESCAPE_HATCHES = 13       PAGE_DIRECTORY = 14       IO_ALIGNMENT = 15
CLUSTERING_METADATA = 16  PAGE_MINMAX = 17          FREETEXT_ROW_OFFSETS = 18
EMBEDDING_ROW_OFFSETS = 19  TEXT_TOKEN = 20         TEXT_TOKEN_OFFSETS = 21
```

Sections 1–12 are present in every footer; sections 13–21 are optional extension blocks emitted only when they carry content. `TEXT_TOKEN` carries inline text-token filters only in files written before those filters moved to the data area; `TEXT_TOKEN_OFFSETS` carries the relocated filters' byte ranges. A new section id may only be allocated by extending the spec; a change to a section's interior encoding (such as the columnar form of the `MARKS` section) is governed by the feature directory, not by a new ad-hoc layout. A reader locates any section by its id through the directory, without scanning.

Every section decodes either zero-copy (read in place, no per-element allocation) or arena-bounded: before allocating for a section, a decoder reads the element count and byte extents the directory records for that section and bounds every allocation and loop by those recorded counts (the `bounded_count` discipline), so a corrupt or hostile footer can never drive an unbounded allocation or read. A section id a reader does not understand is skipped using its directory-recorded length; a section whose checksum does not verify is rejected rather than decoded.

### Layout class: compact and wide

HEF has one file format with two physical layout classes:

```text
layout_class = compact
  default when estimated file size < min_bytes_for_wide_part;
  default min_bytes_for_wide_part = 10 MiB;
  column pages, payload pages, and local indexes may be interleaved in one section;
  one marks directory covers all columns;
  optimized to avoid small-file column overhead.

layout_class = wide
  default when estimated file size >= min_bytes_for_wide_part;
  one section per column family or high-value column group;
  per-column marks allow direct range reads;
  optimized for analytical scans and object-store range reads.
```

The compact/wide crossover is automatic. HEF rewrite may convert compact files to wide files or wide files to compact files when benchmarked policy says it is beneficial. This does not create a new HEF format.

### HEF feature directory

HEF has one file format. A HEF file declares capabilities through a feature directory, not through lifecycle, freshness, historical, archive, or profile labels.

Every HEF file must include these required features:

```text
required_feature_flags:
  envelope_columns
  payload_arena
  stripe_directory
  granule_directory
  marks_per_column
  page_metadata
  stripe_relative_addressing
  columnar_marks
  minmax_skip_indexes
  sequence_skip_index
  time_skip_index
  exact_file_counts
  exact_source_type_entity_counts
  checksum_directory
  footer_directory
  layout_class
```

Optional features are declared independently:

```text
optional_feature_flags:
  low_cardinality_bitmap_indexes
  ribbon_filters
  split_block_bloom_filters
  binary_fuse_filters
  range_filters
  trained_zstd_residual_dictionaries
  entity_locator_skip_index
  account_locator_skip_index
  opportunity_locator_skip_index
  sparse_dimension_cubes
  time_rollup_blocks
  approximate_sketch_blocks
  text_token_index
  context_projection_blocks
  context_packet_blocks
  internal_embedding_vector_blocks
  period_feature_blocks
  graph_extraction_support_blocks
  cause_snapshot_locator_blocks
  quality_input_stats_blocks
  rule_evaluation_support_blocks
  alert_event_locator_blocks
  classification_propagation_blocks
  hef_native_deletion_vectors
  hef_late_events
  variant_shredded_field_blocks
  variant_path_mphf_blocks
  path_presence_indexes
  projections
```

Feature rules:

```text
Unknown required features refuse.

Unknown optional features are ignored.

The planner may use a feature only when the HEF footer declares the feature and the feature block checksum verifies.

A file without an optional feature remains valid. Queries requiring that acceleration must fall back to another valid plan or scan authorized columns.

HEF publication is independent of feature richness. Freshness is provided by HEJ-backed LiveOverlay, not by a separate HEF file type.
```

### Layout projections

The manifest may reference multiple projections for the same logical coverage when benchmarks justify it. A projection is a sort-order alternative inside the same HEF file or manifest file-set, with its own marks and column data, sharing the same logical granule rowset.

```text
primary projection:
  required; sequence-major by default.

time-major projection:
  dashboard scans, counts, rollups, period comparisons.

entity-major projection:
  entity/account/customer/opportunity timelines.

source-type-major projection:
  alert/rule scans and event-type dashboards.

context-major projection:
  investigation evidence retrieval and context packet assembly.

revenue-metric projection or PreparedView output:
  repeated Revenue Intelligence metric queries.
```

The manifest must prevent double-counting. A query snapshot selects one projection plan per logical range. Projections are read alternatives, not additive data.

---
