## Purpose

Defines the manifest as the boundary that controls which data a query can see:

- The manifest entry and the rule that each new generation appears atomically (all-or-nothing).
- The snapshot fields and the rule for what is visible from HEF files vs. the LiveOverlay.
- Where each piece of metadata belongs across the manifest, footer, blocks, PreparedView, and LiveOverlay.

The concrete query-snapshot/watermark and metadata-placement detail are embedded in [manifest-integration-detail.md](manifest-integration-detail.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-manifest-integration/spec.md).
## Requirements
### Requirement: Atomic publication and consistent snapshot fields
Manifest publication SHALL be atomic with respect to HEF coverage, projections, deletion-vector generation, and watermarks; a query SHALL never observe a HEF file without its coverage watermark or a coverage watermark without its HEF file. The manifest SHALL expose the snapshot fields so the table provider can build a consistent HEF/LiveOverlay snapshot. The HEF source SHALL cover Active sequence ranges `<= snapshot_watermark`, the LiveOverlay source SHALL cover node-local ranges not covered by HEF and `<= snapshot_watermark`, and overlapping HEF/LiveOverlay ranges in one snapshot SHALL be forbidden.

#### Scenario: No watermark/file skew
- **WHEN** a HEF file is published
- **THEN** its coverage watermark is published in the same atomic step and never observed separately

### Requirement: Object-store conditional-write publication
Manifest publication on object storage SHALL use conditional writes with no external lock service or coordination database on the publication path. A new generation object SHALL be written with `If-None-Match: *` (create-only PUT) so a concurrent publisher loses deterministically and retries against the observed latest generation, and the current-manifest pointer SHALL be advanced with `If-Match` on the ETag the publisher read (compare-and-swap); a failed CAS SHALL cause the loser to re-read, rebase, and retry, never overwrite. Manifest objects SHALL be immutable once written (only the pointer advances), and readers SHALL resolve pointer → generation object → file set in one consistent pass. Object stores without conditional-write support SHALL require an explicit external commit lock as a documented degraded deployment mode.

HEF SHALL provide the object-store publication itself (`LivePublishedSet`), built only on an application-supplied object-store interface (create-only PUT, `If-Match` PUT, GET with ETag, stat, multipart upload, delete) so HEF carries no store client. Because generation ids are numbers while the pointer CAS compares ETags, an implementation MAY remember the ETag read with the head and use it for the next pointer advance, and SHALL report a lost race as `CasLost` carrying the generation the pointer now names whenever the pointer changed since that read. A store without pointer object SHALL read as the empty generation 0.

#### Scenario: Concurrent publishers race on the pointer
- **WHEN** two publishers attempt to advance the manifest pointer and one loses the `If-Match` CAS
- **THEN** the loser re-reads the latest generation, rebases its change, and retries rather than overwriting the winner

#### Scenario: Create-only generation write collides
- **WHEN** a publisher writes a generation object whose id another publisher already wrote
- **THEN** the write is refused as `GenerationExists` and the existing object is unchanged

### Requirement: Log-structured generation objects
A stored generation object SHALL NOT copy the whole file list on every publish. Every `CHECKPOINT_INTERVAL`th generation (64) SHALL be stored as a full checkpoint; every other generation SHALL store only its differences from the checkpoint before it (entries added or changed, file ids and index artifacts removed, plus the small per-generation fields) and SHALL name that checkpoint. A reader SHALL resolve any generation from its checkpoint plus its one delta to exactly the file set the publisher wrote.

#### Scenario: Generation size bounded as the catalogue grows
- **WHEN** a catalogue grows by one file per publish across several checkpoint intervals
- **THEN** each delta generation object stays the same size however many files the catalogue already holds

#### Scenario: Reader resolves a delta generation
- **WHEN** a reader opens a generation stored as a delta
- **THEN** it rebuilds the same file set, states, and artifacts that a full list would have held

### Requirement: Metadata placement discipline
Metadata SHALL be placed as follows: the manifest holds only small open/skip summaries, part_state, projection availability, deletion-vector generation, and the watermarks; the HEF footer holds authoritative file-level directories; stripe/granule/page blocks hold authoritative local aggregates and pruning metadata; PreparedView files hold cross-file semantic materializations; LiveOverlay holds exact fresh deltas for not-yet-HEF-covered events.

#### Scenario: Large rollup not in manifest
- **WHEN** a large rollup must be stored
- **THEN** it is placed in HEF aggregate blocks or a PreparedView output, not copied into the manifest

