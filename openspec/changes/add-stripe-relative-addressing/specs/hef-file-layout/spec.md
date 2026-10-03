## MODIFIED Requirements

### Requirement: Granule directory and authoritative marks
Every HEF file SHALL contain a footer-visible granule directory with per-granule coverage stats, and every required and promoted column SHALL have a `ColumnMark` for every granule in which it is materialized per the schema-version-keyed presence map. For granules predating a column's promotion schema version, readers SHALL fall back to the variant payload blocks or an authorized payload scan rather than treating the column as silently NULL. Marks SHALL be the authoritative random-access directory for `(column, projection, granule)`; page metadata is local detail below marks and SHALL NOT substitute for marks. When a file declares `stripe_relative_addressing`, a mark's offsets SHALL be interpreted in the stripe-relative offset domain of the mark's stripe (see "Stripe-relative intra-stripe addressing"): a reader SHALL resolve a block's file-absolute position by adding the base offset of the mark's stripe — held once in the stripe base-offset directory — to the mark's stripe-relative offset, and SHALL NOT assume such a file's mark offsets are file-absolute.

#### Scenario: Random access via marks
- **WHEN** a reader needs a specific `(column, projection, granule)`
- **THEN** it resolves the compressed offset/size from the column marks, not from page metadata alone

#### Scenario: Mark resolves through the stripe base offset
- **WHEN** a reader resolves a mark whose file declares `stripe_relative_addressing`
- **THEN** it looks up the mark's stripe in the stripe base-offset directory and reads the block at `stripe_base_offset + stripe_relative_offset`, and the bytes are identical to a file-absolute resolution of the same block

## ADDED Requirements

### Requirement: Stripe-relative intra-stripe addressing
A HEF file MAY record intra-stripe structures in a **stripe-relative offset domain** so that a stripe is a relocatable byte range. When a file declares the `stripe_relative_addressing` feature, every intra-stripe offset — each `ColumnMark` compressed/uncompressed offset, each per-page directory entry offset, and each payload-arena granule's dictionary, offsets-table, and residual offsets — SHALL be measured from the **base offset of the owning stripe**, and the stripe base-offset directory SHALL hold each stripe's file-absolute base offset exactly once. The file-absolute position of any intra-stripe structure SHALL be that stripe's base offset plus the structure's stripe-relative offset. Relocating a stripe (a splice, a compaction that moves an unchanged stripe, or a repack) SHALL rewrite **only** that stripe's one base-offset directory entry; no mark, page entry, or payload offset SHALL change, and the stripe's per-stripe BLAKE3 SHALL be unchanged because its bytes are unchanged. The stripe base-offset directory itself SHALL remain file-absolute — it is the anchor the relative offsets are measured from.

`stripe_relative_addressing` SHALL be a **required, refuse** feature, not an ignorable optional acceleration: a reader that does not understand the flag SHALL refuse and SHALL NOT serve the file, because reading a stripe-relative offset as file-absolute would return wrong bytes. A file that does not declare the flag SHALL retain the legacy file-absolute offset domain, so the feature is adopted under a staged, refusing rollout (as with columnar marks). The stripe-relative domain SHALL introduce no new physical file shape — a stripe-relative file is the same base event file with a stated offset domain — and SHALL compose with columnar marks: because a stripe-relative offset is bounded by the stripe size, the mark offset arrays become smaller integers that delta-encode better than file-absolute offsets.

#### Scenario: Relocating a stripe rewrites one directory entry
- **WHEN** a rewrite relocates an unchanged stripe to a new position in the output file
- **THEN** only that stripe's base-offset directory entry is rewritten, every mark, page entry, and payload offset for the stripe is unchanged, and the stripe's per-stripe BLAKE3 still verifies against the unchanged stripe bytes

#### Scenario: Reader without the feature refuses
- **WHEN** a reader that does not implement `stripe_relative_addressing` opens a file that declares it as a required feature
- **THEN** the reader refuses and does not serve the file, rather than reading any stripe-relative offset as a file-absolute position

#### Scenario: Legacy file keeps the file-absolute domain
- **WHEN** a file does not declare `stripe_relative_addressing`
- **THEN** its mark, page, and payload offsets are file-absolute and are read without adding a stripe base offset, exactly as before this feature existed
