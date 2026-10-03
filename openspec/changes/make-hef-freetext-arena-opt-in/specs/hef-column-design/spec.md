## MODIFIED Requirements

### Requirement: Free-text shredded by schema declaration
Schema-declared free-text payload fields SHALL be shredded into their own columnar family at write time by declaration, not by access statistics, because their consumers (whole-corpus re-extraction during release migration and per-subject erasure) never generate query-path access signals. The free-text family SHALL be stored in its own blocks with text-tuned compression (FSST-class) so bulk single-field reads range-read only free-text blocks sequentially without touching the residual variant arena, and the blocks SHALL be individually cacheable.

A single-row point lookup of a declared free-text field SHALL reach that row without decoding the whole granule's free-text block whenever the block's stored form is per-value addressable — an FSST-class block carries an offset table over its stored values, so one row's byte range is resolvable from the block itself with no additional stored bytes. A file that carries a per-row byte-offset index MAY instead resolve the row through that index. Both paths SHALL return byte-identical values, so a file carrying no index SHALL still meet the guarantee through its block. Where the writer's chosen stored form is not per-value addressable, the reader SHALL still return the correct value by decoding the granule's block, and that decode SHALL be shared across the granule's rows rather than repeated per row. The bulk-egress path (sequential range reads for re-extraction and erasure) is unchanged and uses none of these.

#### Scenario: Re-extraction reads only free-text blocks
- **WHEN** a release migration re-runs extraction over historical free-text
- **THEN** the job range-reads only the free-text column blocks sequentially, not the residual variant arena

#### Scenario: Point lookup reads one free-text row from the block alone
- **WHEN** a query needs a single declared free-text field for one matching row from a file that carries no per-row byte-offset index and whose free-text block is stored in a per-value-addressable form
- **THEN** the reader resolves that row's byte range from the free-text block's own per-value offset table and decodes only that row's value, without decoding the rest of the granule's free-text block

#### Scenario: Point lookup reads one free-text row through an index the file carries
- **WHEN** a query needs a single declared free-text field for one matching row and the file carries the per-row byte-offset index
- **THEN** the reader resolves that row's byte range from the index and reads only that row's bytes, and the value is identical to the one the block's own per-value path yields

### Requirement: Per-row byte-offset index makes wide typed columns point-accessible
Wide typed columns — schema-declared free-text (FSST-class) blocks and internal embedding/vector blocks — SHALL support single-row point access without decoding the whole containing granule block. For a column whose block encoding is itself per-value addressable, the block's own offset table SHALL satisfy this. For a column whose block encoding is not per-value addressable, a per-row byte-offset index MAY carry it: an `(offset: u32, len: u32)` entry per row indexed directly by the row's position within the granule, mirroring the residual variant arena's existing per-row slot. For a column that carries the index, a reader resolving one row's value SHALL read that row's offset entry and then read only that row's bytes — a small, bounded number of IOPs — and SHALL NOT decode the rest of the granule block to reach it. Every path SHALL decode to the same row bytes, so results are byte-identical whichever one a reader takes.

The per-row byte-offset index SHALL be additive and governed by the feature directory as a `typed_column_row_offsets` optional feature: a writer MAY emit it, a reader that declares the feature SHALL use it for point access, and a reader that does not understand it SHALL fall back to decoding the whole granule block and SHALL return identical values. The index SHALL be droppable acceleration state — a file without it stays fully readable, and rebuilding or discarding the index SHALL never change any query result. The index SHALL NOT replace or alter the bulk-egress path for free-text or the approximate-retrieval path for vectors; it adds the per-row exact-fetch path beside them.

Because the index stores a second, uncompressed copy of every value its block already holds, a writer SHALL NOT emit it for declared free-text columns by default; a build MAY opt in where cold single-row free-text reads are measured to warrant the duplicate. Turning the index off for a column SHALL NOT change any value that column returns, and SHALL NOT require a format-version bump, a required feature, or a rewrite of files already written: a file written with the index keeps declaring it and keeps being read through it.

#### Scenario: Free-text point lookup skips the whole-granule decode
- **WHEN** a query reads a single declared free-text field for one row from a file that carries the per-row byte-offset index
- **THEN** the reader resolves the row's `(offset, len)` from the index and reads only that row's bytes, without decoding the rest of the granule's free-text block, and obtains the same bytes it would have read by decoding the whole block

#### Scenario: Free-text written without the index still points-accesses
- **WHEN** a writer publishes a file with declared free-text fields, does not opt into the per-row byte-offset index, and stores those columns in a per-value-addressable form
- **THEN** the file declares no `typed_column_row_offsets` index for those columns, carries no duplicate copy of their values, and a single-row read of one of those fields still reaches the row through the block's own per-value offset table without decoding the whole block

#### Scenario: Single vector fetched by row ordinal
- **WHEN** a caller fetches one row's stored embedding/vector by row ordinal (for example, late-materialized re-ranking of a single candidate) from a file that carries the index
- **THEN** the reader reads the row's offset entry and then that row's vector bytes in a bounded number of IOPs, without decoding the whole vector block

#### Scenario: Reader without the feature falls back identically
- **WHEN** a reader that does not declare `typed_column_row_offsets` reads a wide typed column from a file that carries the index
- **THEN** it ignores the per-row index, decodes the whole granule block as before, and returns byte-identical values

#### Scenario: A file already written keeps its index
- **WHEN** a reader opens a file published before the writer default changed, which carries a free-text per-row byte-offset index and declares `typed_column_row_offsets`
- **THEN** the reader uses that index for free-text point access exactly as before, and the values it returns are identical to those a file without the index returns for the same rows
