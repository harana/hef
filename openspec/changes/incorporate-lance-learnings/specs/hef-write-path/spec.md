## ADDED Requirements

### Requirement: The builder streams at stripe scope with bounded memory
Building an HEF file SHALL NOT require materializing the whole file in memory. The builder SHALL encode stripe by stripe: each finished stripe SHALL be handed off for upload as its stripe-aligned multipart part (or appended to the staging file) as soon as it is sealed, after which the builder SHALL retain only the accumulated footer directories, so peak builder memory is bounded by stripe scope — the stripe being encoded plus the directories — and never grows with file size. The footer SHALL be written last from the accumulated directories.

The integrity chain SHALL be computed incrementally over the streamed bytes: per-stripe BLAKE3 as each stripe is laid out, the whole-file BLAKE3 and CRC-64/NVME as running digests over the bytes in stream order, and the outboard verified-streaming tree composed from the streamed chunk-group digests — never by re-reading the finished file in a second hashing pass. Streaming SHALL change only when bytes are written, never which bytes: the streamed build SHALL produce byte-identical files to a fully materialized build of the same rows, preserving the deterministic-encode discipline and content-derived file identity. Splice rewrites SHALL compose with this bound: a reused stripe is copied server-side and SHALL never enter builder memory, so a rewrite's peak memory is bounded by one rebuilt stripe.

#### Scenario: A day-scale rewrite runs in stripe-scope memory
- **WHEN** compaction rewrites a multi-GiB SuperHEF range
- **THEN** the builder's peak memory stays bounded by one stripe plus the accumulated footer directories, and never approaches the output file's size

#### Scenario: Streaming changes nothing about the bytes
- **WHEN** the same rows are built once through the streamed stripe-scope path and once through a fully materialized build under the serial executor
- **THEN** the two files are byte-identical, with the same `file_id`, per-stripe BLAKE3, `file_blake3`, and outboard tree

#### Scenario: A spliced stripe never enters builder memory
- **WHEN** a splice rewrite reuses a stripe unchanged
- **THEN** the stripe is copied server-side into the replacement object and the builder holds only the stripe it is actually rebuilding
