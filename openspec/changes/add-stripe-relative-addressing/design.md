The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-stripe-relative-addressing/).

# Design — Stripe-relative addressing and stripe-aligned pipelined multipart

Two decisions, in the tension → decision → why → rejected → spec-edits style of
`docs/design-review-decisions.md`. The running theme: both features make a
**stripe the unit of relocation and transfer** without changing a single byte a
reader sees or the BLAKE3 authority that admits those bytes. The offset change is
a reinterpretation of numbers already in the footer against a base already in the
footer; the upload change moves *when* known-final bytes leave for the provider,
not *which* bytes or *when they become visible*.

## 1. Stripe-relative offsets are a refusing feature over the base offset the footer already stores

- **Tension.** A stripe cannot be a relocatable byte range while `ColumnMark`
  offsets are file-absolute. Today `ColumnMark.compressed_offset` is computed from
  a whole-file base (`writer/build.rs` — `base = HEADER_BLOCK_LEN`; the mark stores
  `base + data.len()`) and the reader slices the whole-file buffer with it
  (`layout/reader.rs` — `slice(&self.bytes, mark.compressed_offset, …)`); the
  per-page directory and payload-arena geometry do the same. Moving one stripe
  therefore rewrites every mark, page entry, and payload offset that points into
  or past it — even though the stripe's own bytes, and its per-stripe BLAKE3, do
  not change. And the offset domain is stated nowhere, so nothing stops a reader
  from assuming file-absolute. The reflex fix — store both a stripe id and a
  relative offset on every mark, or version the whole footer — is heavier than
  the problem.
- **Decision.** Measure every *intra-stripe* offset from the base offset of its
  stripe, which the footer **already stores once** as `StripeEntry.file_offset`
  (the stripe base-offset directory). A mark's stripe is resolved through the data
  already present (`ColumnMark.granule_id` → `GranuleEntry.stripe_id` →
  `StripeEntry.file_offset`), so no new per-mark field is required to make the
  scheme work, and the file-absolute position is `stripe_base + relative`.
  Relocating a stripe rewrites that **one** directory entry and nothing else.
  Govern the domain with a new **required, refuse** feature flag,
  `stripe_relative_addressing` (a new bit in `required_features` alongside
  `MARKS_PER_COLUMN`): a reader that lacks the bit in its `KNOWN` set refuses
  on the existing `compat::check_features` path (`hef/compat/mod.rs`), never
  serving the file; a file without the bit keeps the file-absolute domain. State
  the offset domain normatively in `hef-file-layout` for the first time.
- **Why.** The base-offset table already exists, verified and checksummed, so the
  format cost is a rule and one feature bit, not a new directory. Refuse is
  mandatory here and is the reason this is a *required* feature, not an optional
  one like `PER_PAGE_MARKS`: an optional feature is *ignored* when unknown
  (`check_features` masks unknown optional bits off), and a reader that ignored a
  stripe-relative file would read every offset against the wrong base and return
  wrong bytes. This is exactly the "(refusing feature)" constraint from the
  issue, and it keeps Decision 16 intact — the file is still a base event file,
  no new physical shape. The stripe-relative offsets are also bounded by the
  stripe size (≤ 512 MiB), so the mark offset arrays become small, delta-friendly
  integers — the property §3.4's columnar marks want — and the BLAKE3 authority is
  untouched: a relocated stripe keeps its checksum because its bytes are
  unchanged.
- **Rejected.** *Keep file-absolute offsets and rewrite all marks on relocate.* —
  Defeats the whole point (§3.5 splice, cache/peer block-range keys); an O(marks)
  rewrite of bytes that did not move, and it keeps the poorly-delta-encoding large
  integers. *Make it an ignorable optional feature.* — Wrong by construction: an
  ignoring reader misreads the offset domain. It must refuse. *Store both a
  file-absolute and a stripe-relative offset per mark.* — Redundant, doubles the
  offset arrays, and invites drift; the base already lives in one place. *Bump the
  footer major version instead of a feature flag.* — A version bump is a blunt
  all-or-nothing gate; the feature-directory mechanism already gives per-file,
  refuse, staged rollout and is how every other format-affecting HEF feature
  is introduced.
- **Spec edits.** `hef-file-layout` MODIFY "Granule directory and authoritative
  marks" (marks resolve through the stripe base; normative offset domain); ADD
  "Stripe-relative intra-stripe addressing" (the base-offset directory, the
  one-entry-per-relocation property, the refusing feature flag, composition
  with columnar marks).

## 2. Upload as stripes seal, but complete only at the publish boundary

- **Tension.** The write path builds the whole HEF in memory and uploads it only
  after finalization (`build_hef_file` → `stage_and_commit` →
  `RemoteObjectStore::commit_staged`, which chunks the finished buffer by a fixed
  byte size at arbitrary boundaries and runs strictly after the build). That puts
  the entire serialized upload on the durable-to-HEF-published critical path, a
  gated real-time-safety metric. A stripe's bytes are final the moment it seals
  (deterministic encodes, per-stripe BLAKE3), so the upload *could* start then —
  but naïvely "publishing as you go" would break the publish boundary, which
  requires a file to become visible atomically only after full verification.
- **Decision.** Upload each stripe's bytes as it seals, as multipart parts
  aligned to the stripe boundary (a 64–512 MiB stripe is one part where the
  provider's max part size covers it, else the fewest whole parts that do, within
  the 5 MiB–5 GiB provider limits). But **complete** the multipart upload — the
  step that first creates a visible object — only at the existing HEF publish
  boundary, after the footer is written and the authoritative BLAKE3 verifies, and
  make the object visible only through the atomically published manifest entry.
  The pipelined result is byte-identical to finalize-then-upload. On a failed or
  lost publish the upload is aborted so no object is completed; orphaned
  incomplete uploads (from a crashed publisher) are reaped by object-store
  lifecycle policy, since no orphan-abort sweep exists today. Where the provider
  has no multipart support (the local-directory default), fall back to
  finalize-then-upload with an identical result.
- **Why.** Completion is the only step that makes a multipart object visible, so
  gating *completion* (not *part upload*) at the publish boundary keeps the "failed
  publish leaks nothing" and idempotency rules exactly as they are while removing
  the serialized upload from the critical path. Stripe-aligned parts are also the
  precondition for §3.5: an unchanged stripe that is a whole number of parts is
  copyable by reference (`UploadPartCopy`/compose) without read-modify-write. This
  is why §3.6 is sequenced before §3.5. BLAKE3 stays the sole authority — parts
  are just where bytes sit before completion — and deterministic simulation is
  unaffected because the in-memory provider already models
  create/upload_part/complete/abort and fault injection behind the injectable
  interface (INV-RUNTIME: the work stays on the compio blocking-worker pool, no
  new runtime).
- **Rejected.** *Complete parts and expose the object incrementally as stripes
  seal.* — Breaks the atomic publish boundary and idempotency: a half-published
  file would be visible, and a lost race could not cleanly leave nothing. *Fixed-
  size parts ignoring stripe boundaries* (today's `commit_staged` chunking). — A
  stripe would straddle part boundaries, so §3.5 could not copy a whole stripe as
  whole parts; keeps the write-amplification §3.5 exists to remove. *A new
  orphan-abort sweep in the object store.* — Heavier than needed and provider-
  specific; provider lifecycle policy already reaps incomplete multipart uploads,
  so the spec points at that rather than building a sweep.
- **Spec edits.** `hef-write-path` ADD "Stripe-aligned pipelined multipart
  upload" and "Incomplete pipelined uploads leave nothing visible and abort";
  `object-store` MODIFY "Full provider capabilities — byte ranges, multipart
  uploads, and conditional writes" (segment-aligned part sizing; incomplete upload
  leaves nothing visible and is lifecycle-reapable).
