## ADDED Requirements

### Requirement: Per-row byte-offset index makes wide typed columns point-accessible
Wide typed columns — schema-declared free-text (FSST-class) blocks and internal embedding/vector blocks — SHALL support single-row point access without decoding the whole containing granule block, mirroring the residual variant arena's existing per-row slot (an `(offset: u32, len: u32)` entry per row indexed directly by the row's position within the granule). For a column that carries the index, a reader resolving one row's value SHALL read that row's offset entry and then read only that row's bytes — a small, bounded number of IOPs — and SHALL NOT decode the rest of the granule block to reach it. The index SHALL decode to the same row bytes the full-granule-decode path yields, so results are byte-identical whichever path a reader takes.

The per-row byte-offset index SHALL be additive and governed by the feature directory as a `typed_column_row_offsets` optional feature: a writer MAY emit it for free-text and embedding/vector blocks, a reader that declares the feature SHALL use it for point access, and a reader that does not understand it SHALL fall back to decoding the whole granule block and SHALL return identical values. The index SHALL be droppable acceleration state — a file without it stays fully readable at the whole-granule-decode cost, and rebuilding or discarding the index SHALL never change any query result. The index SHALL NOT replace or alter the bulk-egress path for free-text or the approximate-retrieval path for vectors; it adds the per-row exact-fetch path beside them.

#### Scenario: Free-text point lookup skips the whole-granule decode
- **WHEN** a query reads a single declared free-text field for one row from a file that carries the per-row byte-offset index
- **THEN** the reader resolves the row's `(offset, len)` from the index and reads only that row's bytes, without decoding the rest of the granule's free-text block, and obtains the same bytes it would have read by decoding the whole block

#### Scenario: Single vector fetched by row ordinal
- **WHEN** a caller fetches one row's stored embedding/vector by row ordinal (for example, late-materialized re-ranking of a single candidate) from a file that carries the index
- **THEN** the reader reads the row's offset entry and then that row's vector bytes in a bounded number of IOPs, without decoding the whole vector block

#### Scenario: Reader without the feature falls back identically
- **WHEN** a reader that does not declare `typed_column_row_offsets` reads a wide typed column from a file that carries the index
- **THEN** it ignores the per-row index, decodes the whole granule block as before, and returns byte-identical values

## MODIFIED Requirements

### Requirement: Free-text shredded by schema declaration
Schema-declared free-text payload fields SHALL be shredded into their own columnar family at write time by declaration, not by access statistics, because their consumers (whole-corpus re-extraction during release migration and per-subject erasure) never generate query-path access signals. The free-text family SHALL be stored in its own blocks with text-tuned compression (FSST-class) so bulk single-field reads range-read only free-text blocks sequentially without touching the residual variant arena, and the blocks SHALL be individually cacheable. A single-row point lookup of a declared free-text field SHALL use the per-row byte-offset index when the file carries it, reading only that row's bytes rather than decoding the whole granule block; the bulk-egress path (sequential range reads for re-extraction and erasure) is unchanged, and a reader without the index SHALL fall back to the whole-block decode and return identical values.

#### Scenario: Re-extraction reads only free-text blocks
- **WHEN** a release migration re-runs extraction over historical free-text
- **THEN** the job range-reads only the free-text column blocks sequentially, not the residual variant arena

#### Scenario: Point lookup reads one free-text row
- **WHEN** a query needs a single declared free-text field for one matching row and the file carries the per-row byte-offset index
- **THEN** the reader resolves that row's byte range from the index and reads only that row's bytes, without decoding the rest of the granule's free-text block

### Requirement: Internal embedding/vector columns isolated from public output
HEF MAY store internal embedding columns or quantized vector blocks (e.g. RaBitQ quantization with DiskANN/Vamana or IVF layouts, optionally Matryoshka-truncatable) for semantic retrieval and tooling. Ordinary public event APIs SHALL NOT expose them, support bundles and exports SHALL exclude them unless an owner-defined safe representation exists, vector indexes SHALL preserve tenant isolation and data-class labels, and exact event-query correctness SHALL NOT depend on approximate vector retrieval. Single-subject embedding/vector blocks SHALL be encrypted under the subject content key so that crypto-shredding the subject destroys them, and all vector indexes SHALL be rebuildable acceleration state derived from the committed event snapshot whose loss or erasure never loses authoritative data. Exact per-row fetch of one stored vector by row ordinal SHALL use the per-row byte-offset index when the file carries it — reading that row's offset entry and its vector bytes in a bounded number of IOPs, without decoding the whole vector block — which is distinct from the approximate ANN index that finds candidate rows; a reader without the index SHALL fall back to the whole-block decode and return identical vectors.

#### Scenario: Export excludes embeddings
- **WHEN** a support bundle or export is generated
- **THEN** internal embedding/vector blocks are excluded unless an owner-defined safe representation exists

#### Scenario: Crypto-shredding destroys subject vectors
- **WHEN** a subject's content key is destroyed for erasure
- **THEN** that subject's single-subject embedding/vector blocks become unrecoverable and rebuildable vector indexes are reproduced erasure-aware without the shredded subject

#### Scenario: Exact single-vector fetch avoids full-block decode
- **WHEN** re-ranking late-materializes one candidate's stored vector by row ordinal and the file carries the per-row byte-offset index
- **THEN** the reader fetches that row's vector in a bounded number of IOPs via the index, without decoding the whole vector block, and the vector is identical to the whole-block-decode result
