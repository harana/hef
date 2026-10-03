## Purpose

Defines the programming interfaces (APIs) the HEF/HEJ storage engine must expose:

- Writing events, reading them back, aggregation, and the LiveOverlay for recent not-yet-filed data.
- The QueryEngine integration (`HaranaEventsTableProvider`), the context/evidence API, and introspection system tables for inspecting internals.

The concrete Writer/Reader/Aggregation/LiveOverlay/QueryEngine/Context/Introspection API signatures are embedded in [api-signatures.md](api-signatures.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-apis/spec.md).
## Requirements
### Requirement: Bulk-egress reader path
The reader SHALL expose a bulk-egress path, distinct from point-lookup late materialization, for authorized jobs that sequentially read one declared column family (notably free-text) across a large historical range — release-migration re-extraction and per-subject erasure. The bulk path SHALL range-read only the requested family's blocks in sequential order (no residual-arena reads, no per-row point access), SHALL preserve tenant isolation, data-class labels, and authorization, and SHALL be available only to internal authorized jobs, never public routes.

#### Scenario: Migration re-extraction uses bulk egress
- **WHEN** a release migration re-runs the extraction model over historical free-text
- **THEN** it reads via the bulk-egress path, streaming only free-text blocks sequentially, rather than issuing point lookups through the late-materialization path

### Requirement: Zero-copy string column scans
The reader SHALL expose a whole-column string read that decodes a string column block into an Arrow string-view representation backed by the block's shared buffers, so a scan that reads whole string columns pays no per-value allocation materializing them. The view read SHALL yield exactly the rows, nulls, and values the materializing column read decodes for the same block, along with the same presence bitmap, so the two reads are interchangeable row for row. The view read is an optimisation, never a correctness dependency: for a block it does not cover — a non-string column, or a block shape outside the view decoder's fast path — it SHALL decline and the caller SHALL fall back to the materializing read with identical results.

#### Scenario: View read matches the materializing read
- **WHEN** a whole string column is read once through the zero-copy view read and once through the materializing column read
- **THEN** the two reads agree row for row — values, nulls, row count, and presence bitmap — with the view read allocating no per-value strings

#### Scenario: Uncovered blocks fall back identically
- **WHEN** the view read is asked for a non-string column or a block shape outside its fast path
- **THEN** it declines rather than guessing, and the caller falls back to the materializing read with identical results

### Requirement: Context/evidence and introspection APIs stay public-safe
The `EventContextReader` API (`plan_context_scan`, `read_context_packets`) and its `ContextPacket` SHALL never return raw storage paths, internal sequences, payload offsets, or internal tenant identifiers. Introspection system tables (`system.hef_files`, `system.hef_columns`, `system.hef_granules`, `system.hef_rewrites`) SHALL enforce tenant/admin authorization and SHALL NOT expose raw object-store credentials, local filesystem paths, payload bytes, or embedding values.

#### Scenario: System table withholds secrets
- **WHEN** an operator queries `system.hef_files`
- **THEN** results enforce tenant/admin authorization and omit object-store credentials, local paths, payload bytes, and embedding values

