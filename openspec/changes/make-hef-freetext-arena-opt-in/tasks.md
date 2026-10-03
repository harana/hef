# Tasks — make-hef-freetext-arena-opt-in

> Archive after `add-hef-wide-column-point-access`: both deltas modify
> `hef-column-design` "Per-row byte-offset index makes wide typed columns
> point-accessible" and "Free-text shredded by schema declaration", and this
> change's MODIFIED text is the complete replacement for the version that change
> installs.
>
> The measurements this change rests on are in `design.md` §0, taken on
> `crates/format-benchmark`'s HEF dataset at 100,000 rows and 8,192 rows per
> granule. Re-take them if the free-text encoding, the trailing compression gate,
> or `decode_fsst_string_range` changes.

## hef-column-design — the guarantee moves to the block, the arena becomes opt-in

- [x] MODIFY "Free-text shredded by schema declaration" so the point-lookup
      guarantee is stated as "does not decode the whole granule block", satisfied
      by the free-text block's own per-value byte-range path or by a per-row index
      the file carries, with byte-identical values either way and the bulk-egress
      path unchanged. Implements `hef-column-design` — "Free-text shredded by
      schema declaration".
- [x] MODIFY "Per-row byte-offset index makes wide typed columns point-accessible"
      so a per-value-addressable block satisfies point access on its own, the
      writer SHALL NOT emit the index for free-text by default, opting out changes
      no value and needs no format-version bump or rewrite, and a file already
      written keeps declaring and using its index. Implements `hef-column-design`
      — "Per-row byte-offset index makes wide typed columns point-accessible".

## Code — write path (req: hef-column-design "Per-row byte-offset index makes wide typed columns point-accessible")

- [x] `crates/storage/src/hef/writer/build.rs`: add the opt-in build knob and gate
      the free-text arena on it, so a default build assembles no per-row bytes or
      offsets for declared free-text columns and declares no
      `TYPED_COLUMN_ROW_OFFSETS` for them.
- [x] `crates/storage/src/hef/layout/reader.rs`: unchanged. `freetext_value_for_row`
      already falls through to `shredded_value_for_row` when the file carries no
      index, and that path already takes the per-value byte-range fast path for an
      FSST block.

## Tests

- [x] `crates/storage/src/hef/writer/test/build.rs`: a default build with declared
      free-text emits no per-row index and does not declare
      `TYPED_COLUMN_ROW_OFFSETS`; the same rows built with the knob on emit one
      index entry per granule and declare it.
- [x] `hef-column-design` conformance: a file built without the index answers a
      single-row free-text point lookup from the block's own per-value offsets,
      without decoding the whole granule block, and returns values byte-identical
      to the same file built with the index. The fixture's bodies are sized so the
      encoder picks FSST, since that is the encoding the scenario is about.
- [x] The three `hef-column-design` conformance tests that are about a file
      carrying the index now opt into it.

## Companion edits (applied at archive, not part of the requirement deltas)

- [x] `openspec/specs/hef-column-design/column-design.md`: record that a
      per-value-addressable block satisfies point access on its own, that the
      per-row index is opt-in for free-text and off by default, and why (the index
      stores a second uncompressed copy of the block's values).

## Verification

- [x] Re-measured against the fixed reader after `ff827a83` (the FastLanes padding
      and zstd-framing fix): file 14,522,366 B with the arena against 8,181,058 B
      without; cold read of one row from a granule 459 ns against 172 µs (medians
      over 13 granules, one reader); repeat read once resident 65 ns against
      2.4 µs. The pre-fix figures this change was first written against were
      inflated by a stripe-marks defect unrelated to free text; `design.md` §0
      records what changed and why.

- [x] `openspec validate make-hef-freetext-arena-opt-in --strict` green; every
      `### Requirement:` in the deltas carries SHALL or MUST and at least one
      `#### Scenario:`.
- [x] `cargo fmt --check` clean; `cargo clippy -p storage --features write --lib
      --tests` reports nothing under `crates/storage/src/hef/`; `cargo test -p
      storage --features write --lib hef::` green (701 tests); `cargo test -p query
      --lib` green (615 tests).
- [ ] The `conformance` test binary does not build on this branch — 71 pre-existing
      compile errors across unrelated capabilities (`status_page_and_service_health`,
      `subscription_billing`, `trust_and_compliance_program`, and others), plus a
      missing `BuildLifecycle` import in the shared
      `crates/conformance/tests/conformance/support.rs`. The `hef_column_design`
      module was run green (18 tests, including the new one) against a local build
      that trimmed the broken modules and added that import; neither local patch is
      part of this change. Two `hef_column_design` failures found that way —
      `payload_arena_stores_canonical_variant_values_with_statistics_driven_shredding`
      reads the empty `footer.marks` of a columnar-marks file, and
      `context_projection_columns_avoid_raw_payload_scans` expects `context_title`
      public-safe — are pre-existing and touch no free-text path.
