The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-splice-rewrite/).

# Tasks — add-hef-splice-rewrite

> This change turns §3.5 of `docs/hef-format-review.md` into a stripe-reuse rewrite plan
> for SuperHEF compaction. The rewrite planner classifies each source stripe as unchanged
> or rebuilt (any doubt rebuilds), copies unchanged stripes **by reference** — a new
> `server_side_copy` provider capability on the cloud tier, `copy_file_range`/reflink on
> the local tier, buffered copy on the fallback — over stripe-aligned parts, reuses the
> outboard BLAKE3 tree's subtree hashes for byte-identical prefixes while re-hashing
> relocated stripes, and records nothing new in the manifest so splice stays invisible to
> readers. Stripe-relative addressing and stripe-aligned parts are the prerequisite owned
> by `add-hef-stripe-relative-addressing` (`hef-file-layout`); this change assumes them.
> INV-HARDWARE-ACCEL holds (probe / portable fallback / byte-identical / software oracle),
> BLAKE3 stays the sole authority, and deterministic simulation is unaffected. Spec deltas
> land first; the code lands in `writer/compaction.rs`, which folds sidecars only today.

## object-store — the server-side copy capability

- [x] MODIFY "Full provider capabilities — byte ranges, multipart uploads, and
      conditional writes" to add server-side copy (`UploadPartCopy` / compose / Put Block
      From URL) to the provider capability descriptor as `server_side_copy`, define a
      client ranged-read-plus-upload portable fallback that is byte-identical, keep BLAKE3
      as the sole integrity authority over the destination, and require the in-memory
      provider to model it for deterministic simulation. Implements `object-store` — "Full
      provider capabilities — byte ranges, multipart uploads, and conditional writes".

## hef-write-path — the stripe-reuse rewrite plan

- [x] MODIFY "Compaction folds sidecar files into the base" to add the stripe-reuse plan:
      conservative unchanged/rebuilt classification (any doubt rebuilds), copy-by-reference
      of unchanged stripes via `server_side_copy` / `copy_file_range`/reflink / buffered
      fallback over stripe-aligned parts with no read-modify-write, BLAKE3 subtree-hash
      reuse for byte-identical prefixes with relocated stripes re-hashed from cache or a
      ranged read, an unchanged staged-verification publish boundary, and a manifest entry
      that records nothing new so a spliced file is indistinguishable from a full rewrite.
      Implements `hef-write-path` — "Compaction folds sidecar files into the base".

## Code — object store server-side copy (req: object-store "Full provider capabilities — byte ranges, multipart uploads, and conditional writes")

- [x] Add a `server_side_copy` entry to the provider capability descriptor and a copy
      operation on the provider interface that copies an existing object's byte range into
      a new object or multipart part (`copy_object_range` / `upload_part_copy` on
      `RemoteObjectProvider`), with the client ranged-read-plus-upload portable fallback as
      the trait default where the capability is absent, byte-identical to the native path.
      > Native cloud transports are wired: `UploadPartCopy` on the `aws` S3 client (with an
      > inclusive `x-amz-copy-source-range`) and Put Block From URL on the `azure` client
      > (with `x-ms-source-range`), each overriding `upload_part_copy` on its provider so an
      > unchanged stripe is stitched into a part server-side with no bytes through the
      > client. GCS has no *ranged* server-side copy (compose combines only whole objects),
      > so `GcsObjectProvider` keeps the byte-identical portable fallback, as the spec's
      > "where a provider lacks it" clause allows.
- [x] Implement `server_side_copy` in the in-memory provider and route fault injection
      (failed or partial copy) through the existing provider interface so deterministic
      simulation exercises both the native and fallback paths, reproducible from the seed.

## Code — stripe-reuse rewrite planner (req: hef-write-path "Compaction folds sidecar files into the base")

> Lands in `writer/compaction.rs` (`plan_stripe_reuse` → `RewritePlan`), which folded
> sidecars only before. The planner reads each source stripe's `StripeEntry` byte range and
> classifies it; `StripeEntry.file_offset`/`byte_len` are the stripe offset domain this
> assumes from `add-hef-stripe-relative-addressing`.
>
> The live executor lands in `writer/rewrite.rs`: `rewrite_segments` turns a `RewritePlan`
> plus the replacement stripe layout into ordered spans (a reused stripe below
> `min_part_bytes` is downgraded to a rebuild — the conservative sub-minimum reconciliation),
> and `execute_rewrite` drives one multipart upload, `upload_part_copy` for each reused span
> (server-side on S3/Azure, portable fallback elsewhere) and `upload_part` for the rest, then
> completes it create-only. A provider failure aborts the upload, leaving nothing published.
>
> Now that stripe-relative addressing has landed, the two reuse cases it unblocks are wired
> end-to-end in `writer/compaction.rs`:
>  - **Relocated-stripe reuse.** `plan_rewrite(source, replacement)` matches stripes by their
>    per-stripe BLAKE3 (confirmed byte-for-byte) rather than by offset, so an unchanged stripe
>    that an earlier change shifted to a new offset is still copied **by reference** from where
>    it lives in the source into its new place in the replacement — a stripe's bytes are
>    position-independent under stripe-relative addressing. Any stripe with no byte-identical
>    match is rebuilt (conservative).
>  - **Prefix-hash reuse from the source.** `RewritePlan::file_blake3_reusing_source` takes the
>    byte-identical prefix's subtree hash from the source's cached tree (`prefix_hash` on the
>    source), so the copied prefix is never re-hashed on the rewriting machine — only changed
>    bytes are hashed. Bit-for-bit equal to a full re-hash.

- [x] Build the rewrite planner: classify each source stripe as unchanged (same rows, same
      lifecycle-selected encoding strategy, no intersecting deletion vector or fold) or
      rebuilt, rebuilding on any doubt. (`plan_stripe_reuse` → `StripeDisposition::{Reuse,
      Rebuild}`; any `changed` or `uncertain` stripe rebuilds.)
- [x] Copy unchanged stripes by reference as whole stripe-aligned parts — the plan emits a
      `Reuse { source_offset, byte_len }` per unchanged stripe, which `execute_rewrite` copies
      via `upload_part_copy` (native `server_side_copy` on S3/Azure, portable fallback
      elsewhere) over one multipart upload; rebuilt stripes and the header/footer are uploaded
      fresh.
- [x] Compute `file_blake3` by reusing the outboard tree's subtree hashes for a byte-
      identical unchanged prefix and hashing only new bytes (`RewritePlan::file_blake3` via
      `hash_reusing_prefix` in `file/integrity.rs`); a relocated stripe (past the prefix) is
      re-hashed at its new offset. The result is bit-for-bit equal to a full re-hash, and the
      manifest entry records an ordinary immutable object with an ordinary `file_blake3` — no
      new field.

## Tests

- [x] `object-store`: server-side-copy parity with the capability present and absent —
      identical destination bytes, identical BLAKE3 outcome, identical visibility and error
      taxonomy; a partial-copy fault leaks nothing.
- [x] `hef-write-path`: a spliced replacement is byte-identical to a full rewrite of the same
      inputs — `execute_rewrite` reassembles a real `build_hef_file` output over the in-memory
      provider (mixing server-side copies with a fresh upload) and the result equals the
      original bytes (`splice_executor_reassembles_a_real_hef_byte_identically`). `file_blake3`
      matches whether computed by subtree-hash reuse or a full re-hash, a relocated stripe is
      re-hashed correctly, and a stripe with any classification doubt is rebuilt.
- [x] Reader indistinguishability: the spliced object reopens as an ordinary HEF file verified
      against the file's own authoritative BLAKE3 and reads back its full row count — and
      because it is byte-identical to a full rewrite, and the plan records nothing new (no
      per-file "spliced" field; `HefFileEntry` / `SuperHefEntry` carry only whole-file
      `file_blake3`), a reader cannot tell the two apart.
- [x] Simulation: both the copy paths (`object::provider`) and the full rewrite path
      (`hef::writer::rewrite`) run unchanged under the in-memory provider with
      `server_side_copy` reported present (native copy) and absent (portable fallback), each
      producing identical destination bytes, and every run is reproducible from its seed.

## Verification

- [x] `openspec validate add-hef-splice-rewrite --strict` green; every `### Requirement:`
      in the deltas carries SHALL or MUST and at least one `#### Scenario:`.
