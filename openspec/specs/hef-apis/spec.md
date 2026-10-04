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

### Requirement: Entity event scan
The reader SHALL read any stored row back as its event (the full envelope and the payload) from a published HEF file and from a LiveOverlay segment alike. It SHALL expose an entity scan that takes a tenant, an entity's identity hashes, an inclusive `(epoch, sequence)` range, a direction, and an optional limit, runs over a set of HEF files plus the LiveOverlay, and returns that entity's events in sequence order. Where the same sequence point appears more than once, the copy from the newest file generation SHALL be served, and a published copy SHALL win over the LiveOverlay. The scan SHALL skip rows a deletion vector deletes and SHALL serve each corrected event as its latest correction in the original's place: a replacement serves the correcting event, an amendment serves the original with the amendment's payload fields laid over its own, and a retraction drops the event; a correcting event SHALL NOT also be served at its own place. Deletion vectors and corrections SHALL be read through an interface the embedding application backs with its durable storage, never held by the scan itself. A correction whose correcting event is in none of the scanned sources SHALL be refused rather than serving the superseded original.

#### Scenario: Scan spans files and overlay
- **WHEN** one entity's events are spread over two published files and the LiveOverlay
- **THEN** the scan returns all of them, and only them, in sequence order, each with the envelope and payload it was written with

#### Scenario: Backwards scan with a limit
- **WHEN** a scan reads backwards with a limit
- **THEN** it returns the newest events first, crossing from the LiveOverlay into the files, and stops at the limit

#### Scenario: Deleted row skipped
- **WHEN** a deletion vector deletes one of the entity's rows
- **THEN** the scan never serves that row and still serves its neighbours

#### Scenario: Replacement correction returned instead of the original
- **WHEN** one of the entity's events has a replacement correction
- **THEN** the scan serves the correcting event in the original's place, never the original, and does not serve the correcting event a second time

