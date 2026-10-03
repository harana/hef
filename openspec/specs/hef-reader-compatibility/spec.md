## Purpose

Defines how HEF stays compatible as the file format evolves:

- Forward-compatible extension blocks and version metadata in the footer.
- The reader rule: ignore optional things you don't understand, but refuse on required things you don't.

The concrete reader-compatibility and versioning detail are embedded in [reader-compatibility-detail.md](reader-compatibility-detail.md).
## Requirements
### Requirement: Forward-compatible versioned footer
HEF SHALL support forward-compatible extension blocks and the footer SHALL carry `format_version`, required and optional feature flags, `schema_fingerprint`, logical/physical schema, the directory set, encryption metadata, and a checksum directory. Feature flags SHALL indicate use of optional blocks (context projections, vector blocks, sparse cubes, encrypted footers, projections, layout class, HEF-native deletion vectors, `variant_shredded_field_blocks`, `variant_path_mphf_blocks`, `path_presence_indexes`, Ribbon/binary-fuse/range filters, split-block Bloom filters).

#### Scenario: Older reader, newer optional block
- **WHEN** a reader opens a file declaring an optional feature it predates
- **THEN** it can still read the file by ignoring that optional block

### Requirement: Refuse required, ignore-optional, validate-known
A reader SHALL fail the file on an unknown required feature, and SHALL validate checksums before using any known feature. For an **optional** feature block it does not understand, a reader SHALL either ignore the block, or — when the block carries a forward-compatibility escape hatch (a `min_reader_version` the reader meets and a `portable_decoder_ref` the fleet resolves to a conformance-passing portable decoder, per "Optional-block forward-compatibility escape hatch") — read the block through that portable decoder. Whether it ignores the block or decodes it through a portable decoder, the reader SHALL fall back to a scan path or another block that still returns correct, complete results, and SHALL NEVER surface unverified or misdecoded bytes. This optional-block relaxation SHALL NOT extend to required features: an unknown required feature SHALL always fail the file.

#### Scenario: Unknown required feature fails file
- **WHEN** a reader encounters an unknown required feature flag
- **THEN** it fails the file rather than producing partial results

#### Scenario: Unknown optional block ignored or read via portable decoder
- **WHEN** a reader encounters an optional feature block it does not natively understand
- **THEN** it ignores the block and falls back to a correct scan, unless the block carries a `min_reader_version` the reader meets and a conformance-passing `portable_decoder_ref` the fleet resolves, in which case it MAY read the block through that portable decoder, and in either case it returns correct results and never surfaces unverified bytes

#### Scenario: Checksum validated before use
- **WHEN** a reader uses a known optional feature block
- **THEN** it validates the block checksum first and does not use the block if validation fails

### Requirement: Optional-block forward-compatibility escape hatch
HEF SHALL provide a narrow forward-compatibility path so a new encoding used in an **optional** block can ship before every reader in the fleet has gained the native code to decode it. This path applies to optional blocks only; it SHALL NOT apply to required features, which remain refuse per the "Refuse required, ignore-optional, validate-known" requirement.

For an optional block, the footer MAY declare, alongside that block's optional feature flag, a `min_reader_version` and a `portable_decoder_ref`. The `portable_decoder_ref` is a versioned, fleet-resolvable decoder identity (a decoder id the fleet resolves to a portable decoder — for example a sandboxed module the fleet ships out of band — NOT necessarily WASM and NOT bytes the file dictates the reader to execute). When both are present they describe how a reader that lacks the native encoding can still obtain correct rows from that optional block. The footer fields that carry this escape hatch SHALL be ordered `min_reader_version`, then `portable_decoder_ref`, then the block's existing optional feature flag (alphabetical), and SHALL be covered by the checksum directory like any other footer field.

A reader that opens an optional block whose encoding it does not natively understand SHALL take exactly one of two paths, and SHALL NEVER surface unverified or misdecoded bytes:

- **Resolve a portable decoder.** If the block carries a `portable_decoder_ref` that the reader's fleet can resolve to a conformance-passing portable decoder at or above the block's `min_reader_version`, the reader MAY decode the block through that portable decoder. The reader SHALL validate the block checksum before decoding and SHALL admit the decoded rows only if the portable decoder passes the same conformance and software-parity gate required of any HEF decoder, so the rows are byte-for-byte what a native decoder would have produced.
- **Skip to a correct fallback.** Otherwise the reader SHALL cleanly skip the optional block as if it were absent and answer the query from a scan path (or another available block) that still returns correct, complete results. Skipping an optional acceleration block SHALL only cost performance, never correctness, consistent with the droppable-acceleration invariant in `hef-core-invariants`.

The escape hatch is conformance-gated: a `portable_decoder_ref` SHALL NOT be used to read rows unless the portable decoder it resolves to has passed the HEF conformance suite and software-parity check for that encoding; a reader that cannot confirm this SHALL fall back to skip-and-scan rather than trust the decoder. The presence of a `portable_decoder_ref` SHALL NOT change query results, ordering, visibility, checksums, security policy, or public output relative to a fleet that decodes the same block natively.

#### Scenario: Old reader reads a new optional encoding via a portable decoder
- **WHEN** a reader that lacks the native code for an optional block's encoding opens a file whose footer declares a `min_reader_version` the reader meets and a `portable_decoder_ref` its fleet resolves to a conformance-passing portable decoder
- **THEN** the reader validates the block checksum, decodes the block through the portable decoder, and admits rows only because they are byte-for-byte identical to what a native decoder would produce

#### Scenario: Old reader skips the optional block to a correct scan
- **WHEN** a reader meets neither the block's `min_reader_version` nor a resolvable conformance-passing `portable_decoder_ref` for an optional block it cannot natively decode
- **THEN** it skips that optional block as if absent and answers the query from a scan path or another block, returning correct, complete results and never surfacing unverified or misdecoded bytes

#### Scenario: Escape hatch never touches required features
- **WHEN** a file declares an unknown **required** feature flag, with or without a `portable_decoder_ref` present anywhere in the footer
- **THEN** the reader fails the file rather than reading it, because the escape hatch applies to optional blocks only and required features stay refuse

#### Scenario: Unconformant portable decoder is not trusted
- **WHEN** a `portable_decoder_ref` resolves to a decoder that has not passed the HEF conformance suite and software-parity check for that encoding
- **THEN** the reader declines the portable decoder and falls back to skipping the optional block to a correct scan, rather than surfacing its output

