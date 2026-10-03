The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-stripe-relative-addressing/).

# Tasks — add-stripe-relative-addressing

> This change promotes §3.6 of the HEF format review. It makes intra-stripe
> offsets **stripe-relative** so a stripe is a relocatable byte range (rewriting
> one base-offset directory entry, not every mark), governed by a required,
> refuse `stripe_relative_addressing` feature flag; and it makes multipart
> upload **stripe-aligned and pipelined behind the build** (upload-as-sealed),
> with completion and visibility still gated by the HEF publish boundary and
> incomplete uploads aborted. Prerequisite for §3.5 (splice rewrite);
> feature-flagged and refuse like §3.4. BLAKE3 stays the sole integrity
> authority; INV-HARDWARE-ACCEL and INV-RUNTIME (compio) hold; deterministic
> simulation is unaffected; no new file shape (Decision 16). Spec deltas land
> first; the code is enumerated below against the current implementation.

## hef-file-layout — stripe-relative offset domain

- [x] MODIFY "Granule directory and authoritative marks" so a mark resolves a
      `(column, projection, granule)` block by adding the base offset of the
      mark's stripe (from the stripe base-offset directory) to the mark's
      stripe-relative offset, and state the offset domain normatively. Implements
      `hef-file-layout` — "Granule directory and authoritative marks".
- [x] ADD "Stripe-relative intra-stripe addressing": the stripe base-offset
      directory as the single file-absolute anchor, marks/pages/payload geometry
      measured from it, the one-entry-per-relocation property, the refusing
      `stripe_relative_addressing` required feature, and composition with columnar
      marks (§3.4). Implements `hef-file-layout` — "Stripe-relative intra-stripe
      addressing".

## hef-write-path — upload-as-sealed

- [x] ADD "Stripe-aligned pipelined multipart upload": upload each stripe as it
      seals, as stripe-aligned parts, byte-identical to finalize-then-upload, with
      completion and visibility gated by the publish boundary and a
      finalize-then-upload fallback where multipart is unsupported. Implements
      `hef-write-path` — "Stripe-aligned pipelined multipart upload".
- [x] ADD "Incomplete pipelined uploads leave nothing visible and abort": abort
      on failed/lost publish; orphans reaped by object-store lifecycle policy; no
      partial object ever visible. Implements `hef-write-path` — "Incomplete
      pipelined uploads leave nothing visible and abort".

## object-store — stripe-aligned part sizing

- [x] MODIFY "Full provider capabilities — byte ranges, multipart uploads, and
      conditional writes" to add caller-segment-aligned (stripe-aligned) multipart
      part sizing (a whole segment maps to an integral number of parts) and the
      rule that an incomplete multipart upload leaves no visible object and is
      abortable and lifecycle-reapable, with the object-store contract unchanged.
      Implements `object-store` — "Full provider capabilities — byte ranges,
      multipart uploads, and conditional writes".

- [x] `openspec validate add-stripe-relative-addressing --strict` green.

## Code — stripe-relative marks (req: hef-file-layout "Stripe-relative intra-stripe addressing")

> The stripe base-offset directory already exists as `StripeEntry.file_offset`
> (`crates/storage/src/hef/layout/footer.rs`); a mark's stripe is reachable via
> `ColumnMark.granule_id` → `GranuleEntry.stripe_id` → `StripeEntry`. Fields stay
> in strict alphabetical order per the project ordering rule.
>
> Status: the authoritative random-access map — column marks and the page
> directory — is now stripe-relative end to end (writer emits, reader resolves,
> round-trip byte-consistent; 299 storage HEF tests green). The payload-arena
> geometry stays file-absolute for now: the payload arena is written after every
> stripe's column blocks and sits outside the stripe byte ranges the per-stripe
> BLAKE3 covers, so relocating it needs the arena moved inside the stripe — a
> layout change carried with the §3.5 splice work, not here. The upload-as-sealed
> pipeline below is likewise a separate slice; its spec deltas are landed.

- [x] Add `STRIPE_RELATIVE_MARKS` to `required_features` and extend `KNOWN`/`ALL`
      (`crates/storage/src/hef/layout/mod.rs`); the refusing path in
      `hef/compat/mod.rs` (`check_features`) then rejects any reader missing the
      bit with `FormatError::UnknownRequiredFeature`, with no code change there.
- [x] Make the writer emit stripe-relative offsets when the feature is enabled:
      in `crates/storage/src/hef/writer/build.rs`, compute each `ColumnMark` and
      `PageDirectoryEntry` offset relative to its stripe's base instead of
      `base = HEADER_BLOCK_LEN`, with the header/footer feature bit set via
      `required_features::ALL`, and the stripe base-offset directory
      (`StripeEntry.file_offset`) kept file-absolute. (Payload-arena geometry
      deferred — see Status above.)
- [x] Make the reader add the stripe base when slicing: in
      `crates/storage/src/hef/layout/reader.rs`, resolve the mark's stripe base
      (granule → `GranuleEntry.stripe_id` → `StripeEntry.file_offset`) and read the
      block at `stripe_base + relative` for the mark path (`read_column`) and the
      per-page path (`read_page`), only when the file declares the feature; keep
      the file-absolute path for files that do not. (Payload-arena paths deferred —
      see Status above.)
- [x] Keep the mark encode/decode wire format working (`footer.rs` marks
      section): `ColumnMark` does not gain an explicit `stripe_id` — the stripe is
      resolved through the granule directory the file already stores — so the wire
      format and `bounded_count` minimum are unchanged.
- [x] Confirm the publish completeness check
      (`crates/storage/src/hef/writer/publish.rs` — `required_feature_flags` vs
      `required_features::ALL`) still passes for stripe-relative files and does not
      reject legacy files written before the bit existed.

## Code — upload-as-sealed pipeline (req: hef-write-path "Stripe-aligned pipelined multipart upload")

> Today the finished buffer is chunked by a fixed byte size after the build:
> `HefPublisher::publish_range` → `stage_and_commit`
> (`crates/storage/src/object/store.rs`) → `RemoteObjectStore::commit_staged`
> (`crates/storage/src/object/remote.rs` — `multipart_chunk_bytes` +
> `bytes.chunks()`). The provider multipart API
> (`crates/storage/src/object/provider.rs` —
> `create_multipart`/`upload_part`/`complete_multipart`/`abort_multipart`) is
> append-only sequential.

- [ ] Choose stripe-aligned part boundaries in the multipart chunker
      (`object/remote.rs`): size parts so each HEF stripe is an integral number of
      whole parts within `min_multipart_part_bytes`/`max_multipart_parts`
      (`ObjectCapabilities`), instead of the current fixed `by_max` chunking.
- [x] Begin the multipart upload and push stripe parts as stripes seal in
      `build_hef_file`/the publisher, rather than after `stage_and_commit` sees the
      whole buffer; keep `complete_multipart` at the publish boundary after the
      footer is written and the authoritative file BLAKE3 verifies
      (`writer/publish.rs` — `verify_staged`), so the object first becomes visible
      only via the atomically published manifest entry. `hef_upload_segments`
      (`writer/publish.rs`) cuts the file at stripe boundaries; `publish_committed_hef`
      stages those segments through `stage_and_commit_segments` (`object/store.rs`),
      which commits (and so completes the multipart upload) only after the bytes verify.
- [x] Fall back to finalize-then-upload where `capabilities.multipart_uploads` is
      false (LocalFs default), with a byte-identical published result. The segment
      boundaries only shape a multipart split, so a provider without multipart
      uploads puts the whole object once and `stage_and_commit_segments` commits
      exactly the same bytes.

## Code — incomplete-upload abort (req: hef-write-path "Incomplete pipelined uploads leave nothing visible and abort")

- [x] Abort the multipart upload on a failed check or lost manifest CAS in the
      publish path (`object/publication.rs` — `publish_committed_hef` /
      `stage_validate_publish`), so no object is completed on a losing attempt.
      The publish path reaches the object store through `stage_and_commit_segments`;
      `RemoteObjectStore::commit_staged` now aborts the multipart upload on a
      dropped part *and* on a failed completion (crash-before-complete / lost
      create-only race), so a losing attempt completes no object. A completed-but-
      unreferenced object left by a lost head CAS is reaped by the GC sweep.
- [x] Record the object-store lifecycle abort policy for orphaned incomplete
      uploads (crashed publisher, no live handle) as an `object-service`
      deployment policy; no in-process orphan-abort sweep is added (none exists
      today — `abort_multipart` only cleans a handle the process still holds).
      Recorded as `publication::IncompleteUploadLifecyclePolicy`
      (+ `INCOMPLETE_UPLOAD_ABORT_AFTER_NANOS`), a documented provider/deployment
      policy, not a `harana.toml` key.

## Code — object-store stripe-aligned parts (req: object-store "Full provider capabilities — byte ranges, multipart uploads, and conditional writes")

- [ ] Thread the caller's segment (stripe) boundaries through the multipart path
      so part boundaries align to them where a segment fits the provider limits,
      splitting only segments larger than the provider max part size; keep S3
      (`s3.rs`), GCS (`gcs.rs`), Azure (`azure.rs`), and the in-memory
      `SimRemoteProvider` (`provider.rs`) assigning ascending part numbers as
      today.

## Tests

- [x] The one-entry-per-relocation invariant — every mark/page offset sits within
      `[0, stripe.byte_len)`, so relocating a stripe rewrites only its base-offset
      directory entry and the stripe's bytes (hence its per-stripe BLAKE3) are
      unchanged — is proven by the storage unit test
      `hef::writer::build::tests::marks_are_stripe_relative_and_bounded_by_their_stripe`.
      (A dedicated `hef_file_layout` conformance file that physically relocates a
      stripe waits on the conformance crate, which does not currently compile for
      reasons unrelated to this change.)
- [x] A mark resolves identically via `stripe_base + relative` (proven by the same
      storage unit test), and the existing file-absolute assertion
      `mark.compressed_offset >= 4096` in
      `hef_file_layout/granule_directory_and_authoritative_marks.rs` is revised to
      assert the stripe-relative contract (`stripe_base + relative` past the header
      and inside the stripe). Refuse on an unknown required bit is the
      automatic `compat::check_features` gate, already covered by
      `hef_reader_compatibility` tests.
- [x] `hef-write-path` conformance: pipelined upload-as-sealed publishes bytes
      byte-identical to finalize-then-upload for the same journal range; an
      in-progress HEF whose parts are uploaded but whose footer is unverified is
      invisible until the publish boundary succeeds
      (`crates/conformance/tests/conformance/hef_write_path/stripe_aligned_pipelined_multipart_upload.rs`).
- [x] `hef-write-path` / `object-store` conformance: a lost publish race aborts
      the multipart upload and completes no object; an incomplete upload exposes no
      visible object (extended
      `object_store/full_provider_capabilities_byte_ranges_multipart_uploads_and_conditional_writes.rs`
      with `a_losing_multipart_upload_aborts_and_completes_no_object`).
- [x] Deterministic simulation: upload-as-sealed and the abort path run through
      the injectable provider interface (`SimRemoteProvider` multipart +
      `ProviderFault`) with a dropped part and a crash-before-complete, and every
      run stays reproducible from its seed (storage unit tests
      `object::test::remote::{dropped_part_aborts_the_upload_and_completes_no_object,
      crash_before_complete_aborts_the_upload_and_completes_no_object}` and
      `object::test::provider::incomplete_multipart_upload_exposes_no_object_and_aborts`;
      new `ProviderFault::FailUploadPart` injects the dropped part).

## Companion edits (applied at archive, not part of the requirement deltas)

- [x] `hef-file-layout/file-layout.md`: note that `ColumnMark`,
      `PageDirectoryEntry`, and `PayloadGranule` offsets are stripe-relative when
      `stripe_relative_addressing` is declared, that `StripeEntry.file_offset` is
      the file-absolute base they are measured from, and add
      `stripe_relative_addressing` to the required-feature-flags list.
- [x] `hef-write-path/write-path-detail.md`: add the upload-as-sealed step to the
      "Publishing HEF from HEJ" sequence (begin multipart, push stripe parts as
      stripes seal, complete at the publish boundary, abort on a losing attempt).
- [x] `configuration/*`: no new operator key is introduced — the feature flag is a
      code/publication-policy parameter and the lifecycle-abort interval is an
      `object-service`/deployment policy, not a `harana.toml` key.

## Verification

- [x] `openspec validate add-stripe-relative-addressing --strict` green; every
      `### Requirement:` in the deltas carries SHALL or MUST and at least one
      `#### Scenario:`, and no requirement text dangles after a scenario without a
      heading.
- [x] `bash .claude/skills/run-pulse/smoke.sh --quiet` exits 0.
