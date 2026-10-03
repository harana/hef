# HEF Encodings and Compression — Adaptive Pipeline and Per-Type Encodings

Companion artifact for the `hef-encodings-and-compression` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


HEF uses sample-based adaptive encoding selection per column block. Static column-to-codec assignment is forbidden except where this spec marks an encoding mandatory for correctness.

The writer samples each candidate block and chooses the fastest valid pipeline that satisfies size, predicate-pushdown, random-access, and Arrow decode constraints. The selected pipeline is recorded in the page header and column marks.

### Adaptive encoding pipeline

```text
EncodingPipeline {
  sample_profile_id
  physical_type
  logical_type
  transforms[]
  compression_codec
  stats_without_full_decode
  partial_decode_supported
  arrow_decode_target
}
```

Selection order:

```text
1. apply mandatory logical representation rules, e.g. decimal128 for money and TimestampValue logical types;
2. sample candidate pages/mini-blocks;
3. test lightweight transforms before heavyweight compression;
4. prefer pipelines that expose min/max/null_count and predicate preselection without full decompression;
5. prefer zero-copy or low-copy Arrow array construction;
6. store the chosen pipeline id in marks and page metadata;
7. fall back to a required baseline codec only when no specialized pipeline wins.
```

### Timestamp, sequence, and monotonic columns

For `sequence`, `stream_sequence`, `occurred_at`, `ingested_at`, and other monotonic or near-monotonic columns, the required first-choice candidates are:

```text
FastLanes bitpacked FOR
FastLanes DELTA
FastLanes DELTA + bitpacking
base + offset mini-blocks
RLE for repeated values
delta-of-delta only when the sample proves it wins
```

LZ4-frame-only numeric/time storage is not a valid default for these columns. LZ4 may appear only as a trailing compression stage after the adaptive transform or as a measured fallback.

### FastLanes transposed bit-packing layout

Every bit-packed integer stream HEF produces uses the FastLanes *transposed* layout rather than a naive value-after-value bit stream. The naive form cannot be unpacked with vectorized (SIMD) instructions because adjacent values share bytes; the transposed form interleaves values across independent lanes so each lane decodes with nothing but a shift and a mask, reaching tens of GB/s on a SIMD or hardware decoder while a plain scalar loop remains the byte-for-byte reference.

Streams that use this layout:

```text
FastLanes FOR        the frame-of-reference deltas of a monotonic integer column
FastLanes DELTA      the (zigzag) delta / delta-of-delta residuals
ALP scaled integers  the integer stream ALP produces for an f64 metric column
dictionary codes     the per-row codes of a dictionary string block
```

FSST string blocks are compressed through a shared symbol table, not integer bit-packing, so they do not use this layout; their random-access decode is handled by marks/mini-block offsets.

On-disk shape of one bit-packed stream:

```text
count    u32   number of logical values (little-endian)
width    u8    bit width w of each value (0..=64); derived from the data, not configurable
body     [u64] ceil(count / 1024) * 16 * w little-endian 64-bit words

A zero width writes no body: every value is then zero and is reconstructed from count.
```

Layout of the body (the transpose):

```text
- Values are grouped into fixed vectors of 1024. The final vector is zero-padded
  to 1024; the padding is discarded on decode using count.
- A vector is read as 16 lanes of 64-bit words. Each lane holds 64 values packed
  into exactly w 64-bit words; the 16 lanes are interleaved word-by-word, so the
  k-th word of lane l sits at body index (vector * 16 * w) + k * 16 + l. Loading
  16 consecutive words is therefore one lane-parallel register.
- Within a vector the 1024 values are stored as eight 8x16 transposed sub-blocks
  placed in the FastLanes "04261537" order — the self-inverse bit-reversal of
  0..8 — which lets the identical bytes be unpacked at any lane width (8/16/32/64
  bits) with maximum independent work per lane.
- The value at (lane l, value-slot vl) of vector v is logical index
  v*1024 + ORDER[vl/8]*128 + (vl%8)*16 + l, with ORDER = [0,4,2,6,1,5,3,7].
```

Decode is the mirror: read the words once, then for each logical value take its lane's word(s) at the bit offset `(vl % 64-per-lane) * w`, shift down, mask to `w` bits, and (when a value straddles two words) OR in the high part from the next word of the same lane. The scalar path here is the correctness reference; any SIMD/hardware decoder must produce byte-identical values (see "Optional acceleration with mandatory software parity").

### Dictionary and low-cardinality columns

Use dictionary/RLE/bitpacking candidates for:

```text
source_id
event_type_id
entity_type_id
status
stage
currency
country
region
low-cardinality promoted attributes
context labels
```

Dictionary pages must expose dictionary min/max or value-set metadata where useful for pruning. Low-cardinality dimensions should prefer exact bitmap indexes over probabilistic filters.

### Numeric measures

Use:

```text
fixed-width integer encoding
decimal128 for money
FastLanes FOR/DELTA/bitpacking for integer-like measures
RLE for repeated or sparse measures
ALP or ALP RD for f64 metric columns
Zstd or LZ4 trailing compression only when the sample proves a benefit
```

Money must use fixed-scale decimal aggregation, not float. Floating metric columns may use ALP/ALP RD only when round-trip and aggregate correctness pass the declared tolerance contract for the metric.

### Strings

Use:

```text
dictionary encoding for low-cardinality strings
FSST for high-cardinality short strings such as event_id, entity_id, actor_id, account_id, and public-safe refs
front coding for sorted strings
raw byte arena plus offsets for long or opaque strings
optional token index for selected summary/search fields
payload arena for full text or raw documents
```

FSST pages must preserve random-access decode through marks or mini-block offsets. A reader must be able to decode selected granules without decoding unrelated string pages.

### Payload compression

```text
variant_shredded_field_blocks: adaptive encoding by physical field type; pipelines must
  preserve random access in compressed form (FastLanes/ALP/FSST/dictionary
  cascades); whole-block heavyweight compression is forbidden on shredded scan-path columns
variant_residual_blocks: uncompressed in hot granules; page-level Zstd level 3 allowed for
  cold or rewritten granules, declared in marks; cold/rewritten granules should use
  trained Zstd dictionaries (ZDICT) keyed by (tenant_id internal, event_type_id) when
  residual values are small and structurally similar; trained dictionaries are HEF-native
  dictionary blocks referenced from marks, are versioned, and are rebuilt at HEF rewrite
  when sampled compression ratio regresses
variant_dictionary_blocks: dictionary plus FSST encoding; must support key -> field_id
  binary search without full-block decode
text token indexes: Zstd level 1 or better adaptive choice
footer metadata: Zstd level 1 or better adaptive choice
```

### Index and bitmap compression

```text
bitmaps: Roaring bitmap native compression
Ribbon filters: native compact representation
split-block Bloom filters: native split-block representation with optional outer compression only when range reads still work
sketches: adaptive lightweight encoding plus Zstd level 1 fallback
```

### Hardware acceleration compatibility

Encodings and compression must expose large-enough contiguous buffers to benefit from optional accelerators, but they must not require them.

Eligible acceleration:

```text
QPL/IAA:
  compatible compression/decompression and scan/filter preselection;

DSA:
  large buffer movement, fill/copy steps, and scan-output assembly;

QAT:
  compatible compression or crypto only when byte-for-byte and security parity is preserved;

SIMD:
  FastLanes decode, ALP decode, FSST decode, dictionary decoding, bitmap intersection,
  zone-map checks, hash probes, and filter kernels.
```

Fallback rule:

```text
If an accelerator is absent, unhealthy, incompatible, slower, or fails parity checks,
use the software path without changing query results, ordering guarantees, visibility,
checksums, security policy, or public output.
```

No operator-facing config key selects accelerators. Backend selection is a local runtime decision surfaced through diagnostics and metrics only.

---
