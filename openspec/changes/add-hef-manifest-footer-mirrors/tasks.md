The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-manifest-footer-mirrors/).

# Tasks — add-hef-manifest-footer-mirrors

> This change folds §3.2 of `docs/hef-format-review.md` — manifest-native footer
> mirrors ("open N files in one request") — into the specs. A manifest generation MAY
> publish one optional, rebuildable footer-mirror object: the concatenated,
> individually-checksummed footer sections of the files published in that generation
> (optionally per tenant-day), keyed by `file_id`, compressed as one object. Planners
> fetch one object to open every file in the generation; any mismatch (BLAKE3,
> `file_id`, or generation) or a missing mirror falls back to the authoritative per-file
> footer tail read, with identical results. The mirror is manifest-native auxiliary
> metadata (allowed by `hef-core-invariants`), adds no new HEF file shape (Decision 16),
> and is droppable acceleration state (Decision 20) — losing one costs requests, never
> correctness. BLAKE3 stays the sole integrity authority; the HEF footer stays
> authoritative; old readers refuse to the footer path. The reference model and the
> planner's "fetch one, open all" step land in `storage`, following the shape §3.1
> (`add-hef-one-request-cold-opens`) set: pure, tested primitives a not-yet-existing
> remote-fetch orchestration layer will call, exactly like `tail_range`/`HefFooter`.

## hef-manifest-integration — the footer mirror

- [x] ADD "Footer mirror for opening a generation in one request": an optional
      per-generation (or per-tenant-day) footer-mirror object keyed by `file_id`,
      concatenating the generation's footer sections compressed as one object; each
      section carries its own authoritative BLAKE3 and records the `file_id` and
      generation it mirrors; a planner MAY open the whole generation from one mirror read;
      any mismatch (BLAKE3, `file_id`, or generation), a missing section, or a manifest
      that reports no mirror falls back to the authoritative per-file footer tail read
      with identical results; the mirror is manifest-native auxiliary metadata (per
      `hef-core-invariants`), adds no new file shape, is droppable/rebuildable acceleration
      state, and is feature-directory-gated so old readers refuse to the footer path.
      Implements `hef-manifest-integration` — "Footer mirror for opening a generation in
      one request".

## object-service — storing and serving the mirror

- [x] ADD "Footer-mirror objects are rebuildable acceleration state": the object service
      stores and serves the footer-mirror object as a normal immutable, tenant-qualified,
      create-only object, cacheable and promotable like any other object and never
      exposing raw local paths; the mirror does not count against tenant storage quota,
      never authorizes cross-node visibility, is rebuildable from the generation's
      published files, and never stands in for a file's authoritative footer; BLAKE3 stays
      the authoritative integrity check for every mirrored section served. Implements
      `object-service` — "Footer-mirror objects are rebuildable acceleration state".

## Code

- [x] `FooterMirror` (`crates/storage/src/hef/layout/mirror.rs`): builds the compressed,
      per-generation mirror object from each published file's `file_id` and exact tail
      bytes, stamping every section with its own BLAKE3 and the generation it mirrors; opens
      one, dropping any section whose BLAKE3 fails to verify instead of rejecting the whole
      object; and exposes `open_footer`, the "fetch one, open all" planner step that resolves
      each manifest entry's footer from the mirror or falls back to
      `HefFileEntry::tail_range()`/`HefFooter::open` on any mismatch, missing section, or
      missing mirror.
- [x] `ManifestGeneration` (`crates/storage/src/hef/lifecycle.rs`) gains an additive
      `footer_mirror: Option<StoredObject>`; a newly published generation starts without one
      (`HefPublisher::publish_range` in `writer/publish.rs`) since the file set just changed —
      rebuilding is the separate, out-of-band step the mirror's droppable-acceleration design
      already calls for.
- [x] `footer_mirror_path` (`crates/storage/src/object/path.rs`): the mirror's tenant-qualified
      object-store location, one per generation, alongside `manifest_generation_path`.

## Companion edits (applied at archive, not part of the requirement deltas)

- [x] `hef-manifest-integration/manifest-integration-detail.md`: note the footer-mirror
      object beside the deletion-vector generation records — its `file_id` keying,
      per-section BLAKE3 and generation stamps, compression, and the per-file tail
      fallback.

## Verification

- [x] `openspec validate add-hef-manifest-footer-mirrors --strict` green; every `###
      Requirement:` in the deltas carries SHALL or MUST and at least one `#### Scenario:`.
- [x] `cargo test -p storage --features write hef::layout::mirror object::path` green: one
      mirror read opens every file in a generation; a missing mirror, an unknown `file_id`, a
      generation mismatch, and a corrupted section (which costs only that file, not the rest
      of the mirror) all fall back to the per-file tail read with an identical result;
      truncated/unparsable mirror bytes refuse instead of panicking.
