The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-splice-rewrite/).

Status: Approved (2026-07-09)

## Why

A day-scale SuperHEF compaction that changes only a small fraction of stripes — a few
sidecar folds, a deletion vector cleared in a handful of granules, an appended late
range — today re-uploads 100% of the file's bytes. At object-store bandwidth and
request pricing, that rewrite write-amplification is the single largest avoidable cost
of the lifecycle, and on the local NVMe cache tier the same full rewrite doubles device
write amplification.

Three format properties already make byte-reuse rewrites *provable*, so the cost is
avoidable without weakening any guarantee:

- **Encodes are deterministic.** The same part content at the same lifecycle stage
  produces byte-identical encoded blocks (`hef-encodings-and-compression` — "Lifecycle-
  selected cascade strategies"), so an unchanged stripe's bytes are known to reproduce.
- **Stripes are integrity-delimited.** Per-stripe BLAKE3 and the outboard tree let a
  reused stripe be verified — or its subtree hashes reused — without re-reading the
  whole file.
- **Files are immutable.** A replacement is a new object, so copying bytes into it races
  nothing.

Nothing in the specs exploits **server-side copy** — `UploadPartCopy` (S3), compose
(GCS), Put Block From URL (Azure) appear nowhere; the only "server-side copy" reference
is the local `copy_file_range`/reflink already in the object-store probe list. And the
rewrite engine is not yet written (`writer/compaction.rs` folds sidecars only), so
stripe reuse can be a design input rather than a retrofit — this is the moment to specify
it.

## What Changes

**The compaction rewrite plans stripe reuse before it moves bytes.** A rewrite planner
classifies each source stripe as *unchanged* (same rows, same lifecycle-selected encoding
strategy, no intersecting deletion vector or fold) or *rebuilt*, conservatively — any
stripe whose byte-identical reuse cannot be proven is rebuilt. Unchanged stripes are
copied **by reference**, not re-read and re-uploaded: through the provider's native
`server_side_copy` on a cloud tier, through `copy_file_range`/reflink on the local tier
where probed, and through a portable buffered copy otherwise, all three byte-identical.
Because upload parts are stripe-aligned (owned by the sibling `hef-file-layout` stripe-
offset change), an unchanged stripe copies as whole parts with no read-modify-write.

**Integrity is preserved without re-reading the reused bytes.** `file_blake3` is still
computed and stays the sole authority. Where unchanged stripes form a byte-identical
prefix of the replacement, the outboard BLAKE3 tree's subtree hashes for that prefix are
reused as-is (chunk counters are position-dependent and the positions did not move) and
only new bytes are hashed. A relocated stripe is re-hashed from the local cache tier or a
ranged read — still saving the upload, which is the expensive direction. The staged-
verification publish boundary is unchanged.

**Splice is invisible to readers.** The manifest entry for a spliced replacement records
nothing new — an ordinary immutable object with an ordinary `file_blake3`, in the same
one HEF format. There is no new on-disk feature, so there is nothing for an old reader to
refuse on; a reader cannot tell a spliced file from a fully rewritten one.

**The object store gains a `server_side_copy` provider capability.** The provider
capability descriptor adds one entry: a byte range of an existing object can be copied
into a new object or a multipart part server-side, without routing the bytes through the
client, using each provider's native operation. Where a provider lacks it, the portable
fallback is a client ranged read plus a normal upload, byte-identical. BLAKE3 authority is
unchanged, and the in-memory provider models the capability so deterministic simulation
exercises both the native and fallback paths.

Stripe-relative addressing and stripe-aligned upload parts themselves are **not** defined
here — they are the prerequisite owned by `add-hef-stripe-relative-addressing`
(`hef-file-layout`); this change references them as an assumption.

## Capabilities

### Modified Capabilities

- `hef-write-path` — MODIFY "Compaction folds sidecar files into the base" to add the
  stripe-reuse rewrite plan: conservative unchanged/rebuilt classification (any doubt
  rebuilds), copy-by-reference of unchanged stripes via `server_side_copy` /
  `copy_file_range`/reflink / buffered fallback over stripe-aligned parts, BLAKE3 subtree-
  hash reuse for byte-identical prefixes with relocated stripes re-hashed, and a manifest
  entry that records nothing new so splice stays invisible to readers.
- `object-store` — MODIFY "Full provider capabilities — byte ranges, multipart uploads,
  and conditional writes" to add server-side copy (`UploadPartCopy` / compose / Put Block
  From URL) to the provider capability descriptor as `server_side_copy`, with a client
  ranged-read-plus-upload portable fallback that is byte-identical, BLAKE3 authority
  unchanged, and the in-memory provider modeling it for deterministic simulation.

Two existing requirements modified; no requirements added or removed.

## Impact

- **Rewrite write-amplification collapses for small-delta compactions.** A compaction
  that changes 5% of stripes moves roughly 5% of the bytes — server-side on the cloud
  tier (a request, not a transfer) and reflink/`copy_file_range` on the local tier — and
  hashes only the changed region plus any relocated stripes.
- **INV-HARDWARE-ACCEL holds.** `server_side_copy` and `copy_file_range`/reflink are
  capability-probed with a portable buffered/client-copy fallback whose bytes are
  byte-identical; the software path is the oracle; no operator key is introduced.
- **BLAKE3 stays the sole integrity authority.** Copying bytes by reference grants no
  integrity authority; the replacement is verified and published through the unchanged
  staged-verification boundary, and subtree-hash reuse is a computation shortcut over the
  same tree, not a new trust root.
- **Readers and the format are untouched.** Splice adds no manifest field and no feature
  bit; there is nothing to feature-flag and nothing for an old reader to reject.
- **Deterministic simulation is unaffected.** The in-memory provider implements
  `server_side_copy` (and its fallback), fault injection rides the existing provider
  interface, and every run stays reproducible from its seed.

## Open Questions

1. **Prefix vs. general relocation split.** How much of the first cut targets the append-
   oriented byte-identical-prefix case (maximal subtree-hash reuse) versus general stripe
   relocation is an implementation-sequencing question; the requirement admits both and
   fixes only that reuse be proven and relocated stripes re-hashed.
2. **Minimum copy-part sizing across providers.** Each provider's minimum copy-part size
   differs; whether a rare sub-minimum trailing stripe is coalesced with a neighbor or
   simply rebuilt is left to the writer, both satisfying the conservative-classification
   rule.
