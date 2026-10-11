## Purpose

Defines how the bytes of an HEF file (Harana's on-disk event-storage file) are arranged:

- The header at the front, and how event data is split into stripes, granules, pages, and mini-blocks.
- The directory and marks used to jump straight to the right granule.
- The two size-based layout variants (compact vs. wide), where optional features are recorded, and layout projections.

The concrete header, stripe/granule/page model, directory, and projection layouts are embedded in [file-layout.md](file-layout.md).
## Requirements
### Requirement: Fixed aligned file header
Each HEF file SHALL begin with a fixed-size aligned header (`HEFHeader`) carrying magic `"HEF1"`, version, `file_id`, internal `tenant_id`, `generation_id`, `layout_class`, coverage min/max for occurred_at/ingested_at/epoch/sequence, `row_count`, `feature_flags`, and `header_crc64`/`header_blake3`. The header SHALL serve as a coarse rejection accelerator and SHALL NOT be treated as the authoritative aggregate source. The header MAY carry an optional `footer_pointer_hint`, but that hint and the header itself are a **local-open and recovery-scan accelerator only**: a remote reader SHALL NOT read the front-of-file header to locate or open the file and SHALL NOT rely on `footer_pointer_hint`, because a header read is a round trip to the wrong end of the object. `footer_pointer_hint` is therefore deprecated for remote reads; everything a remote reader needs to size the tail range is mandated in the manifest entry (`footer_len`, `tree_len`, and `file_size` — see the hef-manifest-integration requirement "ManifestEntry metadata layers and required entry fields"), and every summary a remote reader needs to prune is mandated in the manifest summaries (see the hef-manifest-integration requirement "Metadata placement discipline"). The header stays present and unchanged for local opens and recovery scans, where the file bytes are already in hand. The file SHALL end with footer metadata, a footer length, and the magic `"HEF1"`.

#### Scenario: Quick rejection from header
- **WHEN** a query's time/sequence range does not intersect the header's min/max coverage
- **THEN** the file is rejected before its footer is read

#### Scenario: Remote reader ignores the header and its footer pointer hint
- **WHEN** a remote reader opens a HEF from object storage
- **THEN** it sizes the tail range from the manifest's `file_size`, `footer_len`, and `tree_len` and never issues a front-of-file header read or reads `footer_pointer_hint`

### Requirement: Remote reads fetch only the ranges a read needs
A reader SHALL be able to open and read a HEF that lives in object storage without holding the whole object. The byte source SHALL be a synchronous range-source interface the embedding application implements (`read_range(object, offset, len)`, plus a batched form that returns several ranges in one call so the application can merge neighbours or fetch them in parallel); the application owns any asynchronous edge. A remote open SHALL issue one request for the tail, sized exactly from the manifest entry's `size_bytes`, `footer_len`, and `tree_len` (a speculative tail plus at most one exact retry when `footer_len` is absent), and SHALL bind the footer to the manifest seal through the publisher's recorded header commitment, never by reading the front-of-file header. After the open, every marks page, column block, and payload slot a read touches SHALL be fetched as the stripe range covering it, widened only to the outboard proof leaves that verify it (the whole stripe when the stripe has no proof tree or its tree is unusable), and no byte SHALL be served before it is proven against the authenticated stripe root. A stripe's co-located marks pages SHALL be fetched as their one contiguous extent. A single-row payload read from an uncompressed residual arena SHALL fetch that row's residual slot, not the whole arena. A stripe that pruning never reaches SHALL never be fetched.

#### Scenario: Cold point read after an exact tail open
- **WHEN** a reader opens a remote HEF whose manifest entry records `footer_len` and reads one column block of one granule
- **THEN** the open is one request and the block read issues at most two more (the stripe's marks extent, then the block), none of them the whole stripe of a stripe that carries a proof tree

#### Scenario: Pruned stripe is never fetched remotely
- **WHEN** a remote reader reads granules of one stripe only
- **THEN** no request touches any byte of any other stripe, its marks pages included

#### Scenario: Single-row payload fetches its slot
- **WHEN** a remote reader reconstructs one row's payload from a granule whose uncompressed residual arena spans several proof leaves
- **THEN** it fetches the proof leaves around that row's residual slot and never the whole arena, and returns the same value an in-memory reader returns

### Requirement: Stripe, granule, page, and mini-block model
HEF SHALL preserve row-group semantics through sequence-ordered stripes and granules and SHALL NOT adopt an arbitrary self-describing layout tree. Stripes SHALL target ~256 MiB uncompressed (min 64 MiB, max 512 MiB); granules SHALL be `min(index_granularity rows=8192, rows fitting index_granularity_bytes=10 MiB compressed)` and contain a contiguous sequence-ordered row range within one stripe with shared row positions across envelope columns. A writer SHALL close the file before any stripe exceeds the maximum stripe size as a pure safety backstop — the stripe clamp SHALL NOT be the primary file-roll trigger, which is owned by publish policy (the dual byte/time roll trigger in the write path) — and a reader SHALL reject a file whose stripe/page exceeds the maximum unless a supported future feature flag is declared.

#### Scenario: Stripe exceeds maximum
- **WHEN** writing would cause a stripe to exceed 512 MiB uncompressed
- **THEN** the writer closes the current HEF file first

#### Scenario: Oversized page on read
- **WHEN** a reader encounters a page/chunk above the maximum size with no recognized declared feature flag
- **THEN** it rejects the file

### Requirement: Granule directory and authoritative marks

Every HEF file SHALL contain a footer-visible granule directory with per-granule coverage stats, and every required and promoted column SHALL have a `ColumnMark` for every granule in which it is materialized per the schema-version-keyed presence map. For granules predating a column's promotion schema version, readers SHALL fall back to the variant payload blocks or an authorized payload scan rather than treating the column as silently NULL. Marks SHALL be the authoritative random-access directory for `(column, projection, granule)`; page metadata is local detail below marks and SHALL NOT substitute for marks.

Each `ColumnMark` SHALL record both the compressed on-disk length and the true uncompressed (decoded) byte length of its `(column, projection, granule)` extent. The uncompressed length SHALL be the number of bytes the extent decodes to, not a copy of the compressed length, so a reader can budget decode memory and make read-coalescing decisions from the marks alone without decoding the block. A build SHALL NOT store the compressed length in the uncompressed-length field; if a build genuinely cannot record a meaningful uncompressed length for an extent, it SHALL omit the field rather than populate it with the compressed length.

Marks SHALL be stored columnar, not as row-oriented per-granule structs: per `(projection, column)` the footer SHALL hold parallel arrays of the mark fields (`compressed_offset`, `compressed_size`, `row_count`, and the other fixed-width fields), each array encoded with the format's own integer encodings (FastLanes / DELTA, exactly as a data column: offsets are monotonic and sizes are small integers). The decoded logical directory SHALL be identical to the row-oriented form; only the physical encoding changes. This columnar marks encoding SHALL be governed by a `columnar_marks` required feature declared in the feature directory: a file that stores marks columnar SHALL declare `columnar_marks`, a reader that does not understand it SHALL refuse on that file, and a writer MAY dual-emit the row-oriented and columnar forms during migration.

The footer's marks section SHALL be two-level and per-stripe: it SHALL hold one directory entry per `(projection, column, stripe)` pointing at an independently-fetchable marks page (co-located with the stripe or in the footer region), and the granule-level marks for a stripe SHALL be parsed only when that stripe survives pruning. A stripe rejected by pruning SHALL cost zero marks bytes — its marks page SHALL NOT be fetched or decoded — whereas the row-oriented form charged every file its full share of marks at open.

#### Scenario: Random access via marks
- **WHEN** a reader needs a specific `(column, projection, granule)`
- **THEN** it resolves the compressed offset/size from the column marks, not from page metadata alone

#### Scenario: Decode budgeting from the mark's uncompressed length
- **WHEN** a reader plans a decode over a column extent and reads its `ColumnMark`
- **THEN** the mark's uncompressed length equals the number of bytes the extent decodes to, so the reader can size its decode buffer and coalesce reads without first decoding the block

#### Scenario: Marks decode from columnar arrays
- **WHEN** a reader resolves the offset/size/row_count for a `(projection, column, granule)` from a file that declares `columnar_marks`
- **THEN** it decodes the value from the per-`(projection, column)` FastLanes/DELTA-encoded arrays and obtains the same directory it would have read from the row-oriented struct form

#### Scenario: Pruned stripe costs zero marks bytes
- **WHEN** a query prunes a stripe before reading it
- **THEN** the reader never fetches or decodes that stripe's granule-level marks page, and only surviving stripes' marks pages are parsed

#### Scenario: Old reader refuses on columnar marks
- **WHEN** a reader that does not understand `columnar_marks` opens a file that declares it as required
- **THEN** it refuses and does not serve the file

### Requirement: Compact and wide layout classes
HEF SHALL have one format with two automatic physical layout classes: `compact` (default below `min_bytes_for_wide_part`, default 10 MiB; interleaved pages with one marks directory) and `wide` (one section per column family/group with per-column marks for direct range reads). The compact/wide crossover SHALL be automatic, and HEF rewrite MAY convert between them without creating a new format.

#### Scenario: Small file uses compact
- **WHEN** the estimated file size is below `min_bytes_for_wide_part`
- **THEN** the writer selects `layout_class = compact`

### Requirement: File profile declared in header, footer, and manifest entry
Every HEF file SHALL declare a `profile` naming what kind of data it holds: `event` (the default), `graph`, `snapshot`, `audit`, `log`, or `inbox`. The profile SHALL be stored as an explicit one-byte code (`event` = 0, `graph` = 1, `snapshot` = 2, `audit` = 3, `log` = 4, `inbox` = 5) that never follows enum declaration order, in the fixed header, in the footer (section `PROFILE`), and in the manifest entry. An `event` file SHALL keep the original header layout and carry no footer profile section, so its bytes are identical to a file written before profiles existed; every other profile SHALL use the header layout that adds the profile byte, whose different `header_len` makes a reader that predates profiles refuse the file. A reader SHALL treat an unknown profile code as a structural error and SHALL refuse a file whose header, footer, or manifest entry disagree on the profile. A manifest entry without a profile SHALL read as `event`. The profile is independent of `layout_class`; graph files are normally `compact`.

Graph files SHALL declare the required feature bits for the graph blocks they carry: `graph_dense_ids` (bit 23), `graph_csr_forward` (24), `graph_csr_reverse` (25), `graph_event_id_index` (26), `graph_chain_cover` (27), and `graph_state_roots` (28). These bits are not part of the set every file declares, so a reader that predates them refuses a graph file rather than reading it as plain events.

#### Scenario: Profile round-trips
- **WHEN** a file is written with any profile and read back
- **THEN** the header, footer, and manifest entry all report that profile

#### Scenario: Unknown profile code
- **WHEN** a reader decodes a header or footer whose profile code it does not know
- **THEN** it fails the file with a structural error

#### Scenario: Old reader refuses a graph file
- **WHEN** a reader that predates the graph feature bits opens a file declaring any of them
- **THEN** it refuses the file as declaring an unknown required feature

### Requirement: Inbox files group rows by recipient and carry a recipient directory
An `inbox` file holds per-recipient queues (to-device messages, device inboxes, an outgoing federation queue). Its recipient SHALL be the row's entity, identified by `(entity_id_hash_low, entity_id_hash_high)`, and the writer SHALL sort its rows by `(entity_id_hash_low, entity_id_hash_high, epoch, sequence)` whatever order they arrive in, so each recipient's rows are one contiguous run in queue order. The footer SHALL carry section `INBOX_RECIPIENTS` (id 36) under optional feature `inbox_recipient_directory` (bit 12): one 48-byte entry per recipient, sorted by recipient, giving its first row ordinal, row count, and newest `(epoch, sequence)` point. The directory is droppable acceleration: a reader without it SHALL find a recipient's rows through the entity hash bounds and return the identical rows. Each recipient's acknowledgement watermark (the newest point it has acknowledged) is durable application state read through an interface the application backs with its own storage. Reading a recipient's queue SHALL never return a row at or below that recipient's watermark, nor a row a deletion vector deletes. An inbox file is built only from all of its rows; a sparse update that holds only some of them is refused.

#### Scenario: Recipient directory locates each queue
- **WHEN** an inbox file is built from interleaved rows for several recipients
- **THEN** each recipient's rows are contiguous in queue order and the directory records each recipient's row range and newest point

#### Scenario: Acknowledged rows are never returned
- **WHEN** a recipient has acknowledged through a point and its queue is read
- **THEN** only its rows after that point are returned, and none once it has acknowledged its newest row

#### Scenario: Deleted inbox rows are skipped
- **WHEN** a deletion vector deletes some of a recipient's unacknowledged rows
- **THEN** reading the queue omits them

#### Scenario: Missing directory gives the same queue
- **WHEN** the same rows are read from a file that carries no recipient directory
- **THEN** the queue read returns the identical rows

### Requirement: Graph files store forward and reverse edge blocks per granule
A graph-profile file MAY carry its subject's event graph. Such a file SHALL hold one subject (1 to 255 bytes) whose events have dense, gap-free sequence numbers in row order, so row `i` is event `first_sn + i`; the writer takes each row's `prev` and `auth` parents as sequence numbers, plus inbound edges from later files supplied when compaction rewrites the file, and SHALL refuse a parent not strictly earlier than its event, a repeated edge, an inbound edge that does not run from a later file into this one, a row list that does not match the file, or edges on any build that is not a whole graph-profile build in sequence order. Each granule SHALL carry one edge block in the data area right after its payload arenas, holding three compressed sparse row tables (forward parents; reverse children built at seal from in-file forward edges; inbound children from later files) whose offsets restart at the granule's first row, one slot per `(row, kind)` with `prev` before `auth`. A target SHALL be stored as `source_sn - target_sn`, and offsets and targets SHALL go through the ordinary column block encoder. Footer section `GRAPH_EDGES` (id 31) SHALL record the subject, `first_sn`, and each block's stripe-relative range. A file carrying edges SHALL declare both `graph_csr_forward` and `graph_csr_reverse`; a reader SHALL refuse the section without both bits or on a non-graph profile, the bits without the section, and a block whose offsets do not run in order from zero to its edge count or whose targets break the direction rules.

#### Scenario: Edges round trip
- **WHEN** a graph file is built with edges and read back
- **THEN** each event's parents come back in the order given and its children in ascending order, in-file children before inbound ones

#### Scenario: Corrupt edge block refused
- **WHEN** an edge block's offsets are out of order or its targets point at the event itself, a later event, or outside the file for an in-file child
- **THEN** the reader fails the read instead of serving edges

#### Scenario: Compact edges
- **WHEN** a room-shaped graph of 100,000 events is stored in 8192-row granules
- **THEN** its edge blocks cost at most 6 bytes per edge, forward and reverse copies together (measured: about 3.0)

### Requirement: Graph files number events densely and keep unresolved parents
A graph file's rows SHALL carry a dense, gap-free per-subject event sequence number `event_sn`, sorted in row order, stored as the footer's `first_sn` plus the row ordinal: the frame-of-reference base of a column whose bit-packed deltas from the row ordinal are all zero, so no per-row bytes are stored. A reader SHALL return any row's `event_sn` and the row of any `event_sn` the file holds. References MAY name an event of the same subject in the `subject_sequence` target space, and resolving one against a graph file SHALL return the row holding that sequence number. The writer SHALL resolve each row's `prev` and `auth` references to sequence numbers through an interface the embedding application backs with durable storage, asking for every reference not already in the `subject_sequence` space in one call; it SHALL hold no mapping of its own. A reference the store cannot resolve SHALL be kept as declared, in footer section `GRAPH_UNRESOLVED` (id 37): a `u32` count, then per entry, sorted by child, the child's sequence number `u64`, the edge kind `u8` (0 `prev`, 1 `auth`), and the reference's column value as `length u16 | bytes`. A row SHALL read as having unresolved edges exactly when the list names it. A file carrying edges SHALL declare `graph_dense_ids` and carry `GRAPH_UNRESOLVED` (possibly empty) beside `GRAPH_EDGES`; a reader SHALL refuse the bit without the sections, either section without the other, and an entry that names a child outside the file, is out of order, or names its parent by sequence number.

#### Scenario: Dense and gap-free per subject
- **WHEN** two consecutive graph files of one subject are written and read back
- **THEN** their rows' sequence numbers run from the first file's `first_sn` to the second file's last event with no gap or repeat

#### Scenario: Unresolved targets survive a round trip
- **WHEN** an event names a `prev` the application's store has not numbered
- **THEN** the written file returns that reference exactly as declared, flags the event as having unresolved edges, and stores no edge for it

### Requirement: Graph files index event ids by sequence number
A graph file whose rows carry protocol ids (the external-id column) SHALL carry footer section `GRAPH_EVENT_IDS` (id 32) and declare `graph_event_id_index`: one `(xxh3_64(id), event_sn)` pair per such row, sorted by hash then sequence number, stored as two `u64` arrays through the ordinary column block encoder, followed by a binary fuse filter (8-bit fingerprints) over the same hashes as `length u32 | bytes`. Looking an id up SHALL binary-search the hashes and confirm every candidate against the id stored in its row before answering, so an id that only shares a hash never answers. The filter SHALL be copied into the file's manifest entry so a planner can skip files that cannot hold an id. A reader SHALL refuse the section without the bit or on a file without graph edges, the bit without the section, an empty index, arrays of different lengths, pairs out of order or repeated, a sequence number outside the file, and filter bytes that do not decode.

#### Scenario: Hit, miss, and forced collision
- **WHEN** a graph file is looked up by an id it holds, an id it does not hold, and an id whose hash is shared with another event's
- **THEN** the first answers its sequence number, the second answers nothing, and the third answers the event whose stored id matches

#### Scenario: Small index, rare false positives
- **WHEN** 100,000 Matrix-shaped ids are indexed and 200,000 absent ids are probed against the filter
- **THEN** the section costs under 12 bytes per event (measured: about 10.0, of which the filter is about 1.23) and the filter admits under 2% of absent ids (measured: about 0.7%)

### Requirement: Graph files store chain-cover labels and link tables
A graph file MAY carry its events' auth chain cover as the embedding application computed it from its view of the whole subject: one `(chain_id u32, seq u32)` label per row and a link table of `(from_chain, from_seq, to_chain, to_seq)` entries. The writer SHALL refuse a label list that does not hold one label per row and any chain whose `seq` does not strictly increase in row order. Each granule's labels SHALL follow the three tables of its edge block, as the chain ids and then the seqs through the ordinary column block encoder, so the read that returns a granule's edges returns its labels. Footer section `GRAPH_CHAIN_COVER` (id 33) SHALL hold the summary, a `u32` chain count then `chain_id | min_seq | max_seq` (`u32` each) per chain ascending, and the links, a `u32` count then, when non-zero, four encoded blocks: `from_chain` delta coded (each the difference from the link before), `from_seq`, `to_chain`, `to_seq`, the links sorted and distinct so those from one chain are adjacent. A file carrying it SHALL declare `graph_chain_cover`. A reader SHALL return the labels of given events, the links from given chains, and whether the file may hold an event on a chain with `seq` in a range, answering the last from the footer alone so files outside the range are skipped. A reader SHALL refuse the section without the bit or without graph edges, the bit without the section, chains out of order, a range whose minimum passes its maximum, links out of order or repeated, and an edge block whose labels are not one `u32` pair per row.

#### Scenario: Non-monotonic seq rejected at write
- **WHEN** a build supplies labels in which some chain's `seq` repeats or goes back in row order
- **THEN** the build fails and no file is written

#### Scenario: Link table round trip
- **WHEN** a graph file is built with links in any order, some repeated
- **THEN** the links from a chain read back sorted, each once

#### Scenario: File pruning by chain range
- **WHEN** a reader asks whether a file may hold chain C with `seq` in a range wholly below or above the file's range for C, or C does not occur in the file
- **THEN** the answer is no, without reading any edge block

### Requirement: Snapshot files answer a keyed point lookup from one granule
A `snapshot` file SHALL hold one epoch's whole keyed state, one row per key, keyed by the row's `entity_id` and sorted strictly ascending by its bytes; the writer SHALL refuse a snapshot whose rows lack a key, repeat a key, are out of key order, or span more than one epoch, and SHALL give every granule its own stripe. The writer SHALL record a key index in footer section `SNAPSHOT_KEYS` (id 34) and declare the optional feature `snapshot_key_index` (bit 11): the bytes every key shares, a learned position model over the next eight key bytes (each distinct model key at the first row holding it), the longest run of rows sharing one model key, and every granule's first key. A lookup SHALL predict the key's row window from the model, widened by the longest run, and pick the last granule in that window whose first key does not exceed the key, so the footer alone names the one granule that can hold the key; it then fetches that granule's stripe in one request and confirms the exact key there. The index is droppable acceleration: a reader that does not use it searches the granules in turn and returns the same answer. A lookup on a file whose profile is not `snapshot` SHALL fail.

#### Scenario: Remote lookup costs one tail read and one granule read
- **WHEN** a snapshot file is opened remotely and one key is looked up
- **THEN** the source sees exactly one request for the tail and one request for the stripe of the granule holding the key

#### Scenario: Absent key
- **WHEN** a key the snapshot does not hold is looked up
- **THEN** the lookup returns nothing after at most one granule read

#### Scenario: Unsorted snapshot rows
- **WHEN** a snapshot is built from rows whose keys repeat or are out of order
- **THEN** the build fails with a structural error

### Requirement: Feature directory governs capabilities
An HEF file SHALL declare capabilities through a feature directory, not through lifecycle/freshness/archive labels. Every file SHALL include the required feature flags (envelope columns, payload arena, stripe/granule directories, per-column marks, page metadata, min/max/sequence/time skip indexes, exact counts, checksum/footer directories, layout class). Unknown required features SHALL refuse; unknown optional features SHALL be ignored. The planner SHALL use an optional feature only when the footer declares it and its block checksum verifies; absence of an optional feature SHALL leave the file valid with queries falling back to another valid plan.

#### Scenario: Unknown required feature
- **WHEN** a reader encounters an HEF file declaring a required feature it does not understand
- **THEN** it refuses and does not serve the file

#### Scenario: Missing optional acceleration
- **WHEN** a query wants an optional acceleration the file does not declare
- **THEN** the planner falls back to another valid plan or an authorized column scan

### Requirement: Layout projections are read alternatives
The manifest MAY reference multiple projections (primary sequence-major plus optional time-/entity-/source-type-/context-/revenue-metric-major) for the same logical coverage, each with its own marks and column data but sharing the same logical granule rowset. Projections SHALL include both horizontal (re-sorted copies of the same columns) and vertical (column-subset files row-aligned by ordinal to the base sort order, including promotion-backfill sidecars and derived-columns sibling files carrying model outputs under a settle horizon). Projections SHALL be read alternatives, not additive data; a query snapshot SHALL select exactly one projection plan per logical range so events cannot be double-counted, and vertical projections SHALL carry the same deletion-vector and correction generations as their base.

#### Scenario: One projection per range
- **WHEN** a query snapshot covers a logical range that has both a primary and a time-major projection
- **THEN** it selects exactly one projection plan for that range

#### Scenario: Vertical projection joined by ordinal
- **WHEN** a query needs a column that lives in a vertical projection rather than the base file
- **THEN** the `QueryEngine` reads the column from the projection row-aligned by ordinal against the base scan, at the same deletion-vector and correction generations

### Requirement: Closed set of file shapes, open producers and queries
The physical file shapes SHALL be a small closed set: (1) the base event file (and the other declared file profiles, which reuse the same container — see "File profile declared in header, footer, and manifest entry"; `log` is the exception: its small write-ahead log objects use the compact log framing in hef-physical-artifacts), (2) re-sorted horizontal projections, (3) extra-column vertical projection / derived-columns sibling files, (4) aggregated PreparedView materializations, and (5) membership interval records — plus indexes over any of them. New analytical capability SHALL NOT add new physical shapes or a catch-all auxiliary container; openness SHALL live in producers (any service MAY attach a derived-columns sibling against a base HEF, declared in the manifest with lineage, with zero format change) and in queries (graph and cause-chain queries via recursive self-join on parent-position columns, semantic queries via embedding columns plus vector indexes, and tenant-defined dimensions via automatic workload-driven promotion, with a re-sorted copy when a dimension becomes a hot grouping key).

#### Scenario: New service attaches derived columns
- **WHEN** a new analytical service wants to publish per-event model outputs
- **THEN** it attaches a derived-columns sibling file against the base HEF declared in the manifest with producer lineage, with no change to the file format

#### Scenario: Exotic query needs no exotic file
- **WHEN** a tenant query groups by a previously unindexed payload dimension
- **THEN** the dimension is served by workload-driven promotion (and optionally a re-sorted copy), not by a new file shape

### Requirement: Pages are independently addressable within a granule
The granule directory and column marks SHALL let a reader address and read an individual page within a granule without reading the rest of that granule's column block. For each `(column, projection, granule)` mark, the footer SHALL record, per page, the page's compressed and uncompressed byte offset and length and its row range (first row ordinal and row count), so a reader can fetch exactly one page or a contiguous run of pages. The footer SHALL also record per-page statistics — min/max, null_count, row_count, and first/last sequence and occurred_at — at the same `page` granularity as the existing granule-level stats, so that a page is the smallest independently prunable and readable unit per column while the granule remains the coarse layer above it. The per-page directory SHALL be stored columnar within the same per-stripe marks page as the granule-level marks and encoded with the format's own integer encodings, and it SHALL be parsed only for stripes that survive pruning, so a pruned stripe costs zero per-page directory bytes. Per-page addressing SHALL be additive: it is governed by the feature directory, a reader that does not declare the feature SHALL fall back to granule-granularity reads and return identical results, and it SHALL NOT change the per-granule "participates in" materialization or the `schema_version` presence-map and payload-fallback semantics.

#### Scenario: Read one page without the rest of the granule
- **WHEN** a scan needs only the rows in one page of a column within a surviving granule
- **THEN** the reader uses the per-page offset and length from the mark to fetch only that page's bytes, and the bytes are identical to decoding the whole granule block and slicing the page's row range

#### Scenario: Older reader falls back to granule granularity
- **WHEN** a reader that does not declare the per-page-marks feature opens a file that carries them
- **THEN** the reader ignores the per-page directory, reads at granule granularity, and returns identical results

#### Scenario: Per-page directory parses only for surviving stripes
- **WHEN** a query prunes a stripe
- **THEN** the reader never parses that stripe's per-page directory, and the per-page directories of surviving stripes decode to the same page offsets, lengths, and row ranges as the row-oriented form

### Requirement: Pages align to a recorded IO granularity and decode independently
A page SHALL be independently decodable: decompressing and decoding one page SHALL NOT require decompressing any other page of the same column block. The writer SHALL align page boundaries to an IO granularity recorded in the footer (for example 4 KiB, the device logical block size, or a huge-page size), so a reader can satisfy a page fetch with an aligned direct read (`O_DIRECT` or a registered-buffer read) that transfers only the requested pages with no read amplification beyond the alignment unit. This requirement SHALL be consistent with the existing residual-block compression model (hot blocks uncompressed for offset-jump access, cold blocks page-level compressed); it constrains page boundaries and per-page decode independence, not the choice of codec.

The footer's recorded IO granularity (`io_alignment_bytes`) SHALL carry the real alignment the writer applied: when the writer aligns page boundaries to a device or page granularity it SHALL record that nonzero value, and it SHALL NOT record a placeholder zero that silently disables the aligned direct-read path. A reader MAY take the aligned `O_DIRECT` or registered-buffer read path only when `io_alignment_bytes` is nonzero and the page offsets it needs are congruent to that granularity; a file that records zero (or a granularity a page offset does not satisfy) SHALL be read through the buffered path instead, with an identical decoded result. Where the writer has probed a device IO-alignment requirement (for example via `STATX_DIOALIGN`), it SHALL align to at least that granularity so the recorded value is usable for a direct read on that device.

#### Scenario: Aligned fetch of surviving pages
- **WHEN** pruning leaves three non-adjacent surviving pages in a granule's column block and the reader issues aligned direct reads for them
- **THEN** only those pages' aligned byte ranges are transferred, each page decodes without touching neighbouring pages, and the result matches a full-block read

#### Scenario: Recorded alignment gates the direct-read path
- **WHEN** a reader opens a file whose footer records a nonzero `io_alignment_bytes` and the page offsets it needs are congruent to that granularity
- **THEN** the reader may issue aligned `O_DIRECT` or registered-buffer reads for those pages, and a file that instead records zero is read through the buffered path with an identical decoded result

### Requirement: Footer serialization is pinned and sections decode bounded
The HEF footer SHALL be a serialized sectioned directory whose exact byte layout is fixed by this specification, not left to the implementation. All fixed-width integers in the footer SHALL be little-endian. The footer blob SHALL be laid out as `[preamble][section directory][section bytes …]`, closed by the `footer_len` (`u64`) and the 4-byte magic `HEF1` at the file tail. The preamble SHALL be, in order: format version major (`u16`), format version minor (`u16`), required-feature flags (`u64`), optional-feature flags (`u64`), schema fingerprint (32 bytes), and section count (`u32`). Each section-directory entry SHALL be exactly 52 bytes, in order: section id (`u32`), the section's byte offset relative to the start of the section area (`u64`), its byte length (`u64`), and its BLAKE3 checksum (32 bytes); the section directory doubles as the checksum directory, and a section's checksum SHALL verify before the section is used.

The set of section ids SHALL be closed and pinned by this specification: `COLUMNS = 1`, `STRIPES = 2`, `GRANULES = 3`, `MARKS = 4`, `DICTIONARIES = 5`, `PAGE_STATS = 6`, `EXACT_COUNTS = 7`, `PAYLOAD_GRANULES = 8`, `PRESENCE = 9`, `SHREDDED = 10`, `FREETEXT = 11`, `STRIPE_CHECKSUMS = 12`, `ESCAPE_HATCHES = 13`, `PAGE_DIRECTORY = 14`, `IO_ALIGNMENT = 15`, `CLUSTERING_METADATA = 16`, `PAGE_MINMAX = 17`, `FREETEXT_ROW_OFFSETS = 18`, `EMBEDDING_ROW_OFFSETS = 19`, `TEXT_TOKEN = 20`, `TEXT_TOKEN_OFFSETS = 21`. Sections 1–12 SHALL be present in every footer; sections 13–21 are optional extension blocks emitted only when they carry content — `TEXT_TOKEN` carries inline text-token filters only in files written before those filters moved to the data area, and `TEXT_TOKEN_OFFSETS` carries the relocated filters' byte ranges (see the hef-query-metadata-and-indexes requirement "Text-token filter bytes live in the data area"). The optional extension blocks `EXTERNAL_IDS = 28` (the file's external-id index: a `u32` count, then sorted `(id hash u64, row ordinal u64)` pairs) and `REFERENCE_FILTERS = 29` (a `u32` count, then per entry relationship column id `u32`, granule id `u32`, filter length `u32`, and the split-block Bloom filter bytes, sorted by `(column id, granule id)`) are emitted only when the file carries external ids, or relationship columns respectively; `REFERENCE_FILTERS` is emitted even when empty so a reader can tell "no granule holds this kind" from "this file predates the filters". The profile extension blocks are `PROFILE = 30` (one profile code byte, emitted only for a non-event profile), `GRAPH_EDGES = 31` (a graph file's edge block directory), `GRAPH_EVENT_IDS = 32` (a graph file's event-id index), `GRAPH_CHAIN_COVER = 33` (a graph file's chain-cover summary and links), `SNAPSHOT_KEYS = 34` (a snapshot file's key index), `AUDIT = 35` (an audit file's chain ends and retention), `INBOX_RECIPIENTS = 36` (an inbox file's recipient directory), and `GRAPH_UNRESOLVED = 37` (a graph file's unresolved parents), each described in its own requirement. A new section id SHALL be allocated only by extending this specification, and a change to a section's interior encoding (such as the columnar marks form of the `MARKS` section) SHALL be governed by the feature directory, not by a new ad-hoc layout. A reader SHALL locate any footer section by its id through the directory without scanning, and two independent implementations SHALL agree byte-for-byte on the footer they serialize for the same logical directory.

Every footer section SHALL decode either zero-copy (read in place, no per-element allocation) or arena-bounded: before allocating for a section, a decoder SHALL read the element count and byte extents the directory records for that section and SHALL bound every allocation and loop by those recorded counts (the `bounded_count` discipline), so a corrupt or hostile footer can never drive an unbounded allocation or read. A section id a reader does not understand SHALL be skipped using its directory-recorded length, and a section whose checksum does not verify SHALL be rejected rather than decoded.

#### Scenario: Section located by id through the directory
- **WHEN** a reader needs the marks section (or any other section) of a footer
- **THEN** it reads the section's offset and length from the footer directory entry for that section id and reads exactly those bytes, without scanning the footer

#### Scenario: Byte-identical footer across implementations
- **WHEN** two independent implementations serialize the footer for the same logical directory content
- **THEN** the two footers are byte-for-byte identical, including section ids, directory-entry layout, and section ordering

#### Scenario: Bounded decode of a hostile footer
- **WHEN** a decoder reads a footer section whose recorded element count or byte extent is larger than the bytes actually present, or otherwise inconsistent
- **THEN** it bounds its allocation and iteration by the directory-recorded counts and the verified section length, and rejects the footer instead of allocating or reading unboundedly

#### Scenario: Unknown section skipped, checksum failure rejected
- **WHEN** a reader encounters a directory entry whose section id it does not know, and another entry whose section bytes do not hash to the entry's recorded BLAKE3 checksum
- **THEN** it skips the unknown section using its directory-recorded length, and rejects the checksum-failing section rather than decoding it


### Requirement: Audit files chain their rows by hash and are never rewritten
An `audit` profile file SHALL chain every row to the one before it. A row's digest SHALL be the BLAKE3 of a fixed encoding of the row: its epoch and sequence, every envelope field in field-name order (integers little-endian, strings length-prefixed, absent optional strings as a zero tag), and its payload (none, an external reference, or the variant encoding against a dictionary of the payload's own keys). A row's hash SHALL be the BLAKE3 derive-key hash, under the context `pulse HEF v1 audit row hash`, of its `prev_row_hash` followed by its digest. The first row of a file SHALL chain from a caller-supplied start: the last row hash of the previous audit file, or 32 zero bytes for the first file of a trail. An audit row SHALL carry only its envelope and payload, and an audit file SHALL carry no analytical columns, so every stored row field is under the chain.

The footer SHALL carry the optional section `AUDIT = 35`: chain start (32 bytes), chain end (the last row's hash, 32 bytes), an S3 Object Lock legal-hold byte (0 or 1), and a retention option tag followed, when present, by the retention mode (`governance` = 0, `compliance` = 1) and the retain-until time in signed nanoseconds. The file SHALL declare the required feature `audit_hash_chain` (bit 29), which is not part of the set every file declares, so a reader or rewriter that predates audit files refuses one, remote readers included. A reader SHALL refuse a footer where the section, the bit, and the `audit` profile do not appear together. The manifest entry carries no chain fields: its `file_seal` authenticates the footer, and with it the chain ends.

A verifier given audit files in manifest order SHALL check that each file's chain start equals the previous file's chain end (or the expected start) and SHALL recompute every row's hash from the stored rows and compare the result with the recorded chain end. Every writer path that would replace an existing file (an update, a stripe-reusing rewrite or splice, an entity projection, a compaction publish) SHALL refuse an audit file as its source or its output.

#### Scenario: Tampered or dropped row
- **WHEN** an audit file's rows are altered or one is dropped, whether its recorded chain end is kept or recomputed
- **THEN** the verifier reports that the file's rows do not hash to its recorded chain end, or that the next file does not continue the chain

#### Scenario: Reordered or missing file
- **WHEN** audit files are verified in an order other than the one they were written in, or with one missing
- **THEN** the verifier reports the first file whose chain start does not equal the previous file's chain end

#### Scenario: Rewrite refused
- **WHEN** an update, splice rewrite, entity projection, or compaction is asked to replace an audit file or to produce one
- **THEN** it refuses with an error and writes nothing

#### Scenario: Old reader refuses an audit file
- **WHEN** a reader whose known required set ends at bit 28 opens an audit file
- **THEN** it refuses the file as declaring an unknown required feature, while every event file stays readable by it
