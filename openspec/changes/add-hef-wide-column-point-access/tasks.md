# Tasks — add-hef-wide-column-point-access

> Issue #1639 is a "confirm the cost, then decide" item. The cost is confirmed
> (see `design.md` §0): in `crates/storage/src/hef/layout/reader.rs` the residual
> arena is point-accessible via `residual_slot`'s per-row `(offset, len)` slot,
> while free-text and shredded typed columns reach one row only through
> `shredded_value_for_row` → `cached_column` → `read_column`, which decodes the
> whole granule block; embedding/vector blocks have no per-row reader at all. This
> change extends the residual arena's per-row offset-index pattern to those wide
> typed columns as an additive, droppable `typed_column_row_offsets` optional
> feature, so a single-row point lookup costs a bounded number of IOPs instead of
> a full-granule decode. Spec deltas land first; the code files the reader path
> names are enumerated below.

## hef-column-design — per-row offset index for wide typed columns

- [x] ADD "Per-row byte-offset index makes wide typed columns point-accessible":
      a per-row `(offset, len)` index for free-text and embedding/vector blocks in
      the same shape as the residual arena's per-row slot, giving bounded-IOP
      point access; additive and governed by a `typed_column_row_offsets` optional
      feature (reader without it falls back to whole-granule decode, byte-identical
      values); droppable acceleration state that leaves the bulk-egress and
      approximate-retrieval paths unchanged. Implements `hef-column-design` —
      "Per-row byte-offset index makes wide typed columns point-accessible".
- [x] MODIFY "Free-text shredded by schema declaration" to add a single-row
      point-lookup path over the per-row offset index (bulk-egress path unchanged;
      reader without the index falls back identically). Implements
      `hef-column-design` — "Free-text shredded by schema declaration".
- [ ] MODIFY "Internal embedding/vector columns isolated from public output" to
      add exact per-row fetch of one stored vector by row ordinal over the same
      index (isolation, crypto-shredding, rebuild, and no-dependence-on-approximate-
      retrieval rules unchanged; reader without the index falls back identically).
      Implements `hef-column-design` — "Internal embedding/vector columns isolated
      from public output".

## Code — reader point-access path (req: hef-column-design "Per-row byte-offset index makes wide typed columns point-accessible")

> The reader path is `crates/storage/src/hef/layout/reader.rs`. `residual_slot`
> is the pattern to mirror; `shredded_value_for_row` / `read_column` is the
> whole-granule-decode path that the index bypasses for free-text and vector
> point reads.

- [ ] In the footer/reader (`hef/layout/footer.rs`, `hef/layout/reader.rs`): carry
      the per-row `(offset, len)` index for free-text and embedding/vector blocks,
      in the same shape as the `PayloadGranule` offsets block, and declare the
      `typed_column_row_offsets` optional feature in the feature directory.
- [ ] Add a point-access read for free-text: when the file carries the index,
      resolve the row's `(offset, len)` and read only that row's bytes rather than
      routing the free-text field through `shredded_value_for_row` → `read_column`
      (whole-granule decode); when the feature is absent, fall back to the current
      whole-granule path and return identical values.
- [x] Add an exact per-row vector fetch keyed by row ordinal over the same index,
      distinct from the approximate ANN retrieval path, with the same fallback.

## Tests (conformance)

- [ ] `hef-column-design`: a free-text point lookup on a file carrying the index
      reads only the target row's bytes (no whole-granule decode) and returns the
      same value the whole-block-decode path yields — sibling of
      `crates/conformance/tests/conformance/hef_column_design/free_text_shredded_by_schema_declaration.rs`.
- [ ] `hef-column-design`: a reader without `typed_column_row_offsets` falls back
      to the whole-granule decode on the same file and returns byte-identical
      free-text and vector values.
- [x] `hef-column-design`: an exact single-vector fetch by row ordinal reads that
      row's vector via the index without decoding the whole vector block, and the
      vector is identical to the whole-block-decode result; the approximate ANN
      path is unaffected.

## Companion edits (applied at archive, not part of the requirement deltas)

- [x] `hef-column-design/column-design.md`: record the per-row byte-offset index
      for free-text and embedding/vector blocks (same shape as the residual arena's
      per-row slot), the `typed_column_row_offsets` optional feature and its
      fallback, and the point-access-beside-bulk / point-access-beside-ANN split.

## Verification

- [x] `openspec validate add-hef-wide-column-point-access --strict` green; every
      `### Requirement:` in the deltas carries SHALL or MUST and at least one
      `#### Scenario:`.
