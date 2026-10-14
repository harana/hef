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
The reader SHALL read any stored row back as its event (the full envelope and the payload) from a published HEF file and from a LiveOverlay segment alike. It SHALL expose an entity scan that takes a tenant, an entity's identity hashes, an inclusive `(epoch, sequence)` range, a direction, and an optional limit, runs over a set of HEF files plus the LiveOverlay, and returns that entity's events in sequence order. Where the same sequence point appears more than once, the copy from the newest file generation SHALL be served, and a published copy SHALL win over the LiveOverlay. The scan SHALL skip rows a deletion vector deletes and SHALL serve each corrected event as its latest correction in the original's place: a replacement serves the correcting event, an amendment serves the original with the amendment's payload fields laid over its own, and a retraction drops the event; a correcting event SHALL NOT also be served at its own place. A correction that was itself corrected SHALL be followed to the end of the chain, applying each hop in order (amendments laid over the version before them, a replacement swapping in its event), and a retraction anywhere on the chain SHALL drop the event. A deletion vector's mark on any row whose payload the served version is built from (the original under an amendment, each amendment, the last replacement) SHALL drop the event, so a deleted payload is never served; a mark on a row that a later replacement discarded reads as its supersession (as a latest-corrected deletion vector records it), so the replacement is still served once. A chain that loops back to an event already on it SHALL be refused. Deletion vectors and corrections SHALL be read through an interface the embedding application backs with its durable storage, never held by the scan itself. A correction whose correcting event is in none of the scanned sources SHALL be refused rather than serving the superseded original.

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

#### Scenario: Correction chain followed to its end
- **WHEN** an event is replaced by a second event that is itself replaced by a third
- **THEN** the scan serves the third event in the original's place, and serves neither the original nor the second event

#### Scenario: Deleted or retracted correction drops the event
- **WHEN** an event's replacement has its row deleted, or is itself retracted
- **THEN** the scan serves neither the replacement nor the superseded original

#### Scenario: Deleted amended original drops the event
- **WHEN** an amended event's original row is deleted
- **THEN** the scan does not serve the event, so the deleted payload is never served under the amendment

### Requirement: Graph read API
The reader SHALL expose an `EventGraph` API over graph-profile files: `parents(subject, sn, kind)` and `children(subject, sn, kind)` for `kind` `prev` or `auth`, their batch forms over many events, and `walk(subject, start, kind, limit)`, a breadth-first stream that yields each next level of ancestors not seen before and stops after `limit` events. Children SHALL be those stored with the parent's file: in-file children and inbound children folded in by its last compaction. The remote implementation SHALL gather every edge block one step needs across the subject's files, prove each against the file's authenticated checksums, join ranges whose gap is at most 1 MiB into requests of at most 8 MiB, send at most the caller's in-flight cap of requests per batch, send a duplicate batch when one runs past the 95th percentile of recent batch latencies (never sooner than the caller's floor, and only when both copies fit under the cap) and take the first successful answer, and hold fetched blocks so a later step needing the same granule sends nothing.

#### Scenario: One request per file per walk level
- **WHEN** a walk reaches a level whose events span several granules of two files
- **THEN** that level costs at most one request per file, each covering every edge block it needs

#### Scenario: In-flight cap splits a step
- **WHEN** one step needs more separate requests than the in-flight cap
- **THEN** they are sent in successive batches of at most the cap

#### Scenario: Slow batch hedged
- **WHEN** a batch has not answered within the hedge delay
- **THEN** an identical batch is sent and the first successful answer is used

### Requirement: Graph event id, chain cover and state read API
Beside `EventGraph` the reader SHALL expose, over the same graph-profile files: `EventIds` with `resolve_ids(subject, ids)`, turning protocol event ids into sequence numbers (`None` for an id no event of the subject carries); `AuthChains` with `chain_labels(subject, sns)`, each event's chain-cover label (`None` when its file stores no chain cover), and `chain_links(subject, chains)`, the sorted, distinct links that start on the given chains; and `SubjectState` with `state_root(subject, sn)`, the state root recorded after a state event (`None` for any other event), `state_get(root, key)` and `state_diff(before, after)` over the tenant's state chunks. Asking for an event no file of the subject holds SHALL fail. In the remote implementation, `resolve_ids` SHALL look an id up only in files whose event-id index holds its hash and confirm every candidate against the id stored in its row, fetching first the marks pages of the stripes the candidates lie in and then their granules' external-id blocks, each step through the same coalesced, capped and hedged batches as an edge step; `chain_labels` SHALL fetch the edge blocks that carry the labels exactly as an edge step does, so labels and edges of one granule cost one fetch between them; `chain_links` and `state_root` SHALL read only footers; and `state_get` and `state_diff` SHALL read chunks from a store the caller supplies with the tenant's state chunk key.

#### Scenario: Ids resolved in batches
- **WHEN** ids of events spread over two files are resolved together
- **THEN** each file costs at most one batch for its marks pages and one for its external-id blocks, and nothing is fetched outside those batches

#### Scenario: Index rules a file out
- **WHEN** an id's hash is absent from a file's event-id index
- **THEN** resolving it sends that file no request

#### Scenario: Labels share the edge fetch
- **WHEN** the chain labels of events in two files are read and then their parents
- **THEN** the labels cost one request per file and the parents cost none

#### Scenario: State read at a state event
- **WHEN** a caller reads the state root after a state event, one key of that state, and its diff against an earlier state
- **THEN** the root costs no request, the key reads one chunk per tree level, and the diff never reads a subtree the two states share

