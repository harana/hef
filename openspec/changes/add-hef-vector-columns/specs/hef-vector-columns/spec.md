## ADDED Requirements

### Requirement: Vector column metadata
A vector column SHALL be stored with `ColumnKind::Vector` and one footer metadata entry naming its dimension, element
type (`F16`, `F32`, `I8`, `U1`), metric (cosine on normalized vectors, dot, L2), model id, model digest and the logical
embedding column it represents. A file with vector columns SHALL declare the `VECTOR_COLUMNS` required feature. A file
without vector columns SHALL encode exactly as before vector columns existed.

#### Scenario: Metadata round-trips
- **WHEN** a footer with a vector column of any element type and metric is encoded and decoded
- **THEN** the decoded metadata equals the written metadata

#### Scenario: Old footers are unchanged
- **WHEN** a footer without vector columns is decoded and encoded again
- **THEN** the bytes are identical

### Requirement: Fixed-width aligned rows
Each vector row SHALL take exactly `row_width(element, dimension)` bytes, quantized rows ending in their per-row
trailer (int8: `scale f32 | offset f32`; 1-bit: `norm f32 | alignment f32`), so row `n` of a granule starts at
`n × width`. Every vector block SHALL start on a 64-byte file offset. The writer SHALL refuse a row of the wrong width,
a non-finite value, and, for cosine, a vector whose length is not 1 within tolerance.

#### Scenario: Wrong width rejected
- **WHEN** a row's length differs from the declared dimension
- **THEN** the write fails

#### Scenario: Single row read
- **WHEN** a reader asks for one row
- **THEN** it returns the stored bytes at `row × width` without an offset index

### Requirement: Linked representations
Every stored copy of the same vectors SHALL name the same logical embedding column, so a reader finds the 1-bit, int8,
f16 and f32 copies without naming conventions.

#### Scenario: Lookup
- **WHEN** a reader lists the representations of an embedding column
- **THEN** it gets every stored copy, coarsest first

### Requirement: Model and metric are checked
A reader SHALL refuse to read or scan a vector column with a model id, digest, dimension or metric other than the
caller's.

#### Scenario: Wrong model
- **WHEN** a scan names a different model id
- **THEN** it fails instead of returning scores

### Requirement: Filtered top-k scan
The scan kernel SHALL score rows by `dimension − Hamming distance` for 1-bit columns and by dot product (or negated
squared distance for L2) for int8, f16 and f32 columns, SHALL skip rows outside the caller's row filter during the scan,
and SHALL return the best `k` rows, ties to the lower row.

#### Scenario: Matches brute force
- **WHEN** a scan runs on random data with and without a filter
- **THEN** its result equals a brute-force reference and contains no filtered-out row

### Requirement: Whole-file encryption
Every block and the footer SHALL be sealed with the pinned AEAD under the file's random data key, each on a nonce made
from the file's nonce prefix and the block's index, with the index bound as associated data. Only magic, version, the
nonce prefix and the data key wrapped under a caller-supplied key SHALL be in the clear.

#### Scenario: No plaintext in the file
- **WHEN** a segment is written
- **THEN** no column value, room id, model id or footer field appears in its bytes

#### Scenario: Ranged reads
- **WHEN** a reader reads one row
- **THEN** it decrypts only that row's block

#### Scenario: Tampering
- **WHEN** a block is altered, swapped or the file truncated, or the wrong key is used
- **THEN** opening or reading fails

### Requirement: Segment manifest
The sealed footer SHALL carry a manifest of model, window (room, first and last event), row count, producer device,
stored representations and the BLAKE3 hash of the sealed blocks.

#### Scenario: Content hash
- **WHEN** a downloaded segment is verified
- **THEN** the BLAKE3 hash of its sealed blocks equals the manifest's

### Requirement: Multi-segment search and re-embed listing
A search over many segments SHALL prune segments and granules by room, sender and time statistics before decrypting
any of their vector blocks, scan the coarsest copy, re-score candidates with the finest, and merge the top `k`.
`segments_to_reembed` SHALL list every segment built with a model other than the current one.

#### Scenario: Pruned segments are not read
- **WHEN** a search filters to one room
- **THEN** segments of other rooms decrypt no blocks

### Requirement: Prefix reads
A reader SHALL return the first `n` dimensions of one f16 or f32 row decoded to f32, and SHALL refuse a prefix read of
an int8 or 1-bit column or a prefix longer than the model's dimension.

#### Scenario: Leading dimensions
- **WHEN** a reader asks for the first 8 dimensions of an f32 row
- **THEN** it returns exactly the first 8 values written

### Requirement: Segment centroids
The sealed footer SHALL carry the mean of the segment's vectors, normalized for cosine, or none for an empty segment.
`nearest_segments` SHALL rank segments by the query's score against their centroid, closest first, with segments that
have no centroid ranked first. A footer written without a centroid SHALL still open.

#### Scenario: Closest segment first
- **WHEN** segments are ranked for a query equal to one segment's only vector
- **THEN** that segment ranks ahead of the others, after any segment without a centroid

### Requirement: Small-segment merge
`merge_segments` SHALL write one segment holding every row of its inputs in order, with vectors re-encoded from each
input's finest representation, and SHALL refuse inputs with different columns or a different model.

#### Scenario: Same search results
- **WHEN** two segments are merged
- **THEN** a search over the merged file returns the same event ids as a search over the two inputs

### Requirement: WebAssembly read path
The vector segment reader and writer SHALL build for WebAssembly and read files written on any other platform.

#### Scenario: Cross-platform file
- **WHEN** a WebAssembly build opens a segment written natively
- **THEN** it authenticates, verifies the content hash and returns the written event ids
