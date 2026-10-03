## MODIFIED Requirements

### Requirement: Index, bitmap, and payload compression families
Bitmaps SHALL use Roaring native compression, Ribbon filters their compact representation, and split-block Bloom filters their split-block representation with outer compression only when range reads still work. `variant_shredded_field_blocks` SHALL use adaptive encoding that preserves random access in compressed form (whole-block heavyweight compression forbidden on shredded scan-path columns); `variant_residual_blocks` SHALL be uncompressed in hot granules with page-level Zstd-3 allowed for cold/rewritten granules (optionally with trained ZDICT dictionaries keyed by internal `(tenant_id, event_type_id)`, rebuilt at rewrite when the ratio regresses); `variant_dictionary_blocks` SHALL use dictionary+FSST supporting key→field_id binary search without full-block decode; text token indexes and footer metadata SHALL use Zstd level 1 or a better adaptive choice.

A cold/rewritten `variant_residual_blocks` arena SHALL store its Zstd-3 bytes in the **Zstandard Seekable Format**: a series of frames each compressing at most a declared bound of plaintext independently, followed by a seek table in a skippable frame recording every frame's start in both the compressed and the decompressed stream. The bound SHALL be small relative to a granule's arena so that a reader inflates a fraction of it, and the stored bytes SHALL remain an ordinary Zstandard stream — a decoder unaware of the seekable format SHALL recover the whole arena byte for byte by concatenating the frames and skipping the table. A reader resolving a row's residual span SHALL inflate only the frames covering that span, never the whole arena, and MAY cache an inflated frame under the same byte budget as its other decoded caches. The arena's compression SHALL be declared per granule in the payload index, its domain closed: a reader that does not recognize a declared residual compression SHALL refuse the file rather than misread the bytes. Arenas written as whole-arena Zstd-3 before the seekable form SHALL stay readable under their own declaration. Framing changes where the compression boundaries fall and nothing else: the level, the lifecycle rule that admits compression only for cold/rewritten granules, the rule that an arena the compression does not shrink stays uncompressed, and the reconstructed payload bytes are all unchanged, and each frame being an ordinary Zstandard frame keeps the optional accelerated (de)compression path of "Optional acceleration with mandatory software parity" available unchanged.

#### Scenario: Bitmap compression
- **WHEN** a low-cardinality dimension is stored as a bitmap index
- **THEN** it uses Roaring native compression

#### Scenario: Point read inflates one frame of a cold arena
- **WHEN** a point read needs one row's residual bytes from a cold granule whose arena spans several frames
- **THEN** the reader resolves the span through the seek table and inflates only the frames covering it, leaving the rest of the arena untouched

#### Scenario: A conforming Zstandard decoder recovers the whole arena
- **WHEN** a decoder with no knowledge of the seekable format reads a cold granule's stored arena
- **THEN** it decompresses the frames and skips the seek table, recovering the arena byte for byte

#### Scenario: Unknown residual compression refuses the file
- **WHEN** a reader opens a file whose payload index declares a residual compression it does not recognize
- **THEN** it refuses the file rather than reading the arena under a compression it does not implement
