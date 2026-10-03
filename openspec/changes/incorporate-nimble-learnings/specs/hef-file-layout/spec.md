## ADDED Requirements

### Requirement: Marks may alias identical extents
Two or more `ColumnMark`s within a stripe MAY reference the same byte extent when their encoded blocks are byte-identical, so a duplicate encoded block — a mirrored hash column, an `ingested_at` stream equal to `occurred_at` for an in-order tenant, a repeated constant-ish context block — is stored once. The writer MAY detect duplicates by hashing each finished block and confirming with a byte-exact comparison before aliasing; a hash match alone SHALL NOT alias. Aliasing SHALL be invisible to decode semantics: each aliased mark decodes exactly as if it owned a private copy, and the reader SHALL deduplicate its read regions so an extent aliased by several marks in one scan is fetched once. Aliasing SHALL be deterministic (the same rows always produce the same aliases and the same bytes) and its savings SHALL be surfaced in build statistics.

#### Scenario: Identical streams are stored once
- **WHEN** two columns of a stripe encode to byte-identical blocks
- **THEN** the second column's mark references the first block's extent, the stripe stores the bytes once, and both columns decode to their original values

#### Scenario: The reader fetches an aliased extent once
- **WHEN** a scan reads two columns whose marks alias one extent
- **THEN** the extent's bytes are fetched and verified once and served to both columns

#### Scenario: A hash collision cannot alias different bytes
- **WHEN** two finished blocks hash equal but differ in any byte
- **THEN** the byte-exact comparison rejects the alias and both blocks are stored

## MODIFIED Requirements

### Requirement: Footer serialization is pinned and sections decode bounded
The HEF footer SHALL be a serialized sectioned directory whose exact byte layout is fixed by this specification, not left to the implementation. All fixed-width integers in the footer SHALL be little-endian. The footer blob SHALL be laid out as `[preamble][section directory][section bytes …]`, closed by the `footer_len` (`u64`) and the 4-byte magic `HEF1` at the file tail. The preamble SHALL be, in order: format version major (`u16`), format version minor (`u16`), required-feature flags (`u64`), optional-feature flags (`u64`), schema fingerprint (32 bytes), and section count (`u32`). Each section-directory entry SHALL be exactly 52 bytes, in order: section id (`u32`), the section's byte offset relative to the start of the section area (`u64`), its byte length (`u64`), and its BLAKE3 checksum (32 bytes); the section directory doubles as the checksum directory, and a section's checksum SHALL verify before the section is used.

The set of section ids SHALL be closed and pinned by this specification: `COLUMNS = 1`, `STRIPES = 2`, `GRANULES = 3`, `MARKS = 4`, `DICTIONARIES = 5`, `PAGE_STATS = 6`, `EXACT_COUNTS = 7`, `PAYLOAD_GRANULES = 8`, `PRESENCE = 9`, `SHREDDED = 10`, `FREETEXT = 11`, `STRIPE_CHECKSUMS = 12`, `ESCAPE_HATCHES = 13`, `PAGE_DIRECTORY = 14`, `IO_ALIGNMENT = 15`, `CLUSTERING_METADATA = 16`, `PAGE_MINMAX = 17`, `FREETEXT_ROW_OFFSETS = 18`, `EMBEDDING_ROW_OFFSETS = 19`, `TEXT_TOKEN = 20`, `TEXT_TOKEN_OFFSETS = 21`, `STRIPE_NDV = 22`, `SPARSE_KEYS = 23`, `SHARED_DICTIONARIES = 24`. Sections 1–12 SHALL be present in every footer; sections 13–24 are optional extension blocks emitted only when they carry content — `TEXT_TOKEN` carries inline text-token filters only in files written before those filters moved to the data area, and `TEXT_TOKEN_OFFSETS` carries the relocated filters' byte ranges (see the hef-query-metadata-and-indexes requirement "Text-token filter bytes live in the data area"). `STRIPE_NDV` carries the per-stripe distinct-count estimates (see the hef-aggregation-metadata requirement "Per-stripe distinct-count estimates for the planner"), `SPARSE_KEYS` carries the sparse shredded key set (see the hef-column-design requirement "Sparse shredded columns below the promotion threshold"), and `SHARED_DICTIONARIES` carries the file-scope dictionary alphabets (see the hef-encodings-and-compression requirement "Dictionary alphabets may be shared at file scope"); each shared alphabet SHALL be strictly ascending — every shared-scope evaluator binary-searches it, so an unsorted or duplicated alphabet SHALL be rejected at decode as structural corruption. A new section id SHALL be allocated only by extending this specification, and a change to a section's interior encoding (such as the columnar marks form of the `MARKS` section) SHALL be governed by the feature directory, not by a new ad-hoc layout. A reader SHALL locate any footer section by its id through the directory without scanning, and two independent implementations SHALL agree byte-for-byte on the footer they serialize for the same logical directory.

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

#### Scenario: An unsorted shared alphabet is refused
- **WHEN** a footer's `SHARED_DICTIONARIES` section carries an alphabet whose values are not strictly ascending
- **THEN** the footer is rejected as structurally corrupt rather than decoded, so code-order predicate evaluation can never silently diverge from a decode-then-filter reference
