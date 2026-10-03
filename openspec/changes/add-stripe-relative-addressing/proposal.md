The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-stripe-relative-addressing/).

Status: Approved (2026-07-09)

## Why

Two format properties that HEF wants next — moving a stripe's bytes with a
server-side copy instead of a re-upload (the §3.5 splice rewrite), and keying the
local/peer cache tiers by `(file_id, block kind, block range)` — both need a
stripe to be a **relocatable byte range**. Today it is not. `ColumnMark` records
`compressed_offset`/`uncompressed_offset` (and the per-page directory and the
payload-arena geometry record their own offsets) as **file-absolute** positions,
and the offset domain is stated nowhere normatively — the `hef-file-layout` spec
lists `compressed_offset` in the mark with no rule about what it is measured
from. The consequence is concrete: relocating one stripe (a splice, a
compaction that moves an unchanged stripe, a repack) means rewriting **every
mark, page entry, and payload offset** that points into or past it, even though
the stripe's own bytes are byte-identical. The offsets are also large integers
that delta-encode poorly, which works against the §3.4 columnar-marks direction
that wants the offset arrays to be small, delta-friendly columns.

The stripe directory already carries each stripe's base offset —
`StripeEntry.file_offset` in the footer — so the base-offset table this change
needs is present; what is missing is the **rule** that intra-stripe structures
are measured from it, and a refusing feature flag so a reader can never
confuse the two domains.

Separately, the write path stages the whole HEF object and uploads it **after**
finalization. Multipart upload exists in the provider interface, but nothing
begins the upload while the file is still being built, and parts are not aligned
to stripe boundaries. That leaves the "durable-to-HEF-published lag" — a gated
real-time-query safety metric (`hef-benchmarks-and-acceptance-gates` — Real-time
query safety gates: "p99 durable-to-HEF-published lag below the HEF publication
target") — carrying the full serialized upload after the build is already done.
Because HEF encodes are deterministic and stripes are integrity-delimited
(per-stripe BLAKE3), a sealed stripe's bytes are final the moment it seals, so
its upload can start then. Stripe-aligned parts are also the enabler for §3.5:
an unchanged 64–512 MiB stripe is one to a few whole multipart parts (provider
limits 5 MiB–5 GiB), copyable by reference without read-modify-write.

This change promotes §3.6 of the HEF format review to specs. It is the
**prerequisite for §3.5** (splice rewrite) per the epic's sequencing, and is
feature-flagged and refuse exactly like the §3.4 columnar-marks work.

## What Changes

**Intra-stripe offsets become stripe-relative, governed by a refusing feature
flag.** A HEF file that declares the new `stripe_relative_addressing` required
feature measures every intra-stripe offset — a mark's `compressed_offset` and
`uncompressed_offset`, each per-page directory entry's offset, and each
payload-arena granule's dictionary/offsets/residual offsets — from the **base
offset of the owning stripe**, recorded once per stripe in the stripe
base-offset directory (the existing `StripeEntry.file_offset`). The file-absolute
position is that base plus the stripe-relative offset. Relocating a stripe then
rewrites **one** base-offset directory entry; no mark, page entry, or payload
offset changes, and the stripe's per-stripe BLAKE3 is unchanged because its bytes
did not change. The offset domain is stated **normatively** in the spec for the
first time. This is declared as a **required, refuse** feature — not an
ignorable optional acceleration — because it changes how every offset is
interpreted: a reader that does not understand the flag SHALL refuse rather
than silently read a stripe-relative offset as file-absolute. A file that does
not declare the flag keeps the legacy file-absolute domain, so the rollout is
staged like §3.4. No new physical file shape is introduced (Decision 16): a
stripe-relative file is the same base event file with a stated offset domain.

**Multipart upload becomes stripe-aligned and pipelined behind the build
(upload-as-sealed).** As each stripe seals during the HEF build, its bytes are
uploaded as one or a few whole multipart parts aligned to the stripe boundary,
pipelining the upload behind the ongoing build instead of waiting for
finalization. Visibility is unchanged: the multipart upload is **completed**, and
the object made visible through the manifest, only at the existing HEF publish
boundary after the footer is written and the authoritative BLAKE3 verifies. The
pipelined result is **byte-identical** to staging the whole file and uploading it
after finalization (deterministic encodes + integrity-delimited stripes), so it
is pure mechanism invisible to readers. An incomplete or abandoned multipart
upload leaves **no** visible object and is aborted — explicitly on a failed or
lost publish, and by provider lifecycle policy for any orphan — so a losing or
crashed attempt leaks nothing, exactly as the publish boundary already requires.

## Capabilities

### Modified Capabilities

- `hef-file-layout` — MODIFY "Granule directory and authoritative marks" so a
  mark resolves a `(column, projection, granule)` block by adding the mark's
  stripe base offset to its stripe-relative offset, and state the offset domain
  normatively. The stripe base-offset directory (`StripeEntry.file_offset`)
  becomes the single authority for a stripe's file placement.
- `object-store` — MODIFY "Full provider capabilities — byte ranges, multipart
  uploads, and conditional writes" to reflect stripe-aligned (caller-segment-
  aligned) multipart part sizing and to state that an incomplete multipart upload
  leaves no visible object and is abortable by lifecycle policy. The object-store
  contract — immutable create-only objects, conditional pointers, tenant-qualified
  paths, authoritative BLAKE3, no raw paths in public output — holds unchanged.

### Added Capabilities

- `hef-file-layout` — ADD "Stripe-relative intra-stripe addressing" governing the
  stripe base-offset directory, the normative stripe-relative offset domain for
  marks/pages/payload geometry, the one-entry-per-relocation property, the
  refuse `stripe_relative_addressing` feature flag, and how it composes with
  columnar marks (§3.4).
- `hef-write-path` — ADD "Stripe-aligned pipelined multipart upload" (upload-as-
  sealed, stripe-aligned parts, byte-identical to finalize-then-upload, completion
  and visibility still gated by the publish boundary) and "Incomplete pipelined
  uploads leave nothing visible and abort" (abort on failed/lost publish; provider
  lifecycle reaps orphans; no partial object ever visible).

One requirement modified in each of `hef-file-layout` and `object-store`; one
requirement added to `hef-file-layout`; two requirements added to
`hef-write-path`.

## Impact

- **A stripe becomes a relocatable byte range.** Splice rewrites (§3.5) and the
  cache/peer tiers can move or key a stripe by its byte range and rewrite exactly
  one base-offset entry to place it, instead of rewriting every mark that points
  into it. This is the format precondition §3.5 is sequenced to build on.
- **Smaller, delta-friendlier offset arrays.** Stripe-relative offsets are small
  integers bounded by the stripe size (≤ 512 MiB), so the mark offset columns
  delta-encode far better than file-absolute offsets — the property §3.4's
  columnar marks want.
- **Publish lag shrinks by pipelining the upload behind the build.** Upload-as-
  sealed removes the full serialized post-finalization upload from the
  durable-to-HEF-published critical path, directly attacking the gated real-time
  safety metric, with the published bytes byte-identical to today's.
- **BLAKE3 stays the sole integrity authority (INV unchanged).** Per-stripe and
  file BLAKE3 verification is unchanged; a relocated stripe keeps its checksum
  because its bytes are unchanged; the publish boundary's staged verification is
  untouched. Stripe-relative addressing changes only how an offset is
  interpreted, never which bytes are admitted.
- **Deterministic simulation is unaffected (INV-RUNTIME).** The in-memory
  provider already models multipart upload and fault injection; upload-as-sealed
  and the abort path run through the same injectable interface on the compio
  blocking-worker pool, and every simulated run stays reproducible from its seed.
- **Refuse, no new file shape (Decision 16).** The feature flag refuses
  for readers that do not understand it, and a stripe-relative file is still a
  base event file — no new physical shape, no catch-all container.

## Open Questions

1. **Whether to also relocate-encode granule and stripe directory offsets.** The
   stripe base-offset directory itself stays file-absolute (it is the anchor);
   this change makes only the *intra-stripe* structures relative. Whether a future
   splice also wants the granule directory's derived offsets recomputed lazily
   from stripe bases (rather than stored) is a §3.5 question, not settled here.
2. **Part-size policy within a stripe.** A 64–512 MiB stripe maps to one part on
   most providers and a few parts on providers with a smaller max part size; the
   exact split (one part per stripe where it fits, else N equal parts) is a code
   and benchmark choice bounded only by the requirement that a stripe is an
   integral number of parts.
3. **How aggressively lifecycle reaps orphaned uploads.** The abort-on-failure
   path is normative here; the provider-lifecycle reap interval for orphaned
   incomplete uploads is a deployment/`object-service` policy detail, not fixed by
   this spec.
