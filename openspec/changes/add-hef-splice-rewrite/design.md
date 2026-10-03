The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-splice-rewrite/).

# Design — Splice-based rewrite: move bytes with requests, not bandwidth

Four decisions, in the tension → decision → why → rejected → spec-edits style of
`docs/design-review-decisions.md`. The running theme: stripe reuse is a pure mechanism
riding guarantees the format already gives — deterministic encodes, per-stripe BLAKE3,
immutable files — so it changes cost, never correctness, and never surfaces to a reader.

## 1. Conservative classification: any doubt rebuilds

- **Tension.** The planner could try to reuse a stripe whenever it *probably* reproduces
  byte-identically, maximizing reuse, or reuse only where reuse is *provable*. The first
  risks a spliced file that disagrees with a fresh rewrite by a byte; the second leaves
  some reusable stripes on the table.
- **Decision.** Classify a source stripe as *unchanged* only when its rows, its lifecycle-
  selected encoding strategy, and the absence of an intersecting deletion vector or fold
  are all known to reproduce byte-identically; every other stripe is *rebuilt*. Any doubt
  rebuilds.
- **Why.** Deterministic encode already guarantees byte-identical output for identical
  part content at the same lifecycle stage, so the provable set is large in practice; and
  a rebuild is never wrong, only slower. Correctness must never rest on an unproven reuse.
- **Rejected.** *Optimistic reuse with a post-hoc byte compare.* — Re-reads the bytes to
  compare them, which is exactly the upload/read cost splice exists to avoid, and turns a
  mismatch into a late abort instead of a cheap up-front rebuild.
- **Spec edits.** `hef-write-path` MODIFY "Compaction folds sidecar files into the base".

## 2. Copy by reference, with a probed fast path per tier and a portable oracle

- **Tension.** An unchanged stripe could always be copied the portable way (read its
  bytes, write them into the new object), or use each tier's native zero-transfer copy.
  The portable way still pays the bandwidth splice is meant to save; the native way is
  provider- and kernel-specific.
- **Decision.** Copy unchanged stripes by reference: `server_side_copy` on a cloud tier
  (`UploadPartCopy` / compose / Put Block From URL), `copy_file_range`/reflink on the
  local tier where the capability probe confirms it, and a portable buffered copy
  otherwise. All three produce byte-identical replacement bytes; the buffered path is the
  oracle. Stripe-aligned parts (owned by `hef-file-layout`) let an unchanged stripe copy
  as whole parts with no read-modify-write.
- **Why.** This is the standard INV-HARDWARE-ACCEL shape — probe, use, portable fallback,
  observable equivalence — applied to a copy. The cloud path turns a byte transfer into a
  request; the local path turns it into a reflink; the fallback keeps every host correct.
- **Rejected.** *A single portable copy path.* — Leaves the entire bandwidth and request
  win unrealized, which is the whole point. *A cloud-only feature.* — Strands the local
  NVMe cache tier, where full rewrites double device write amplification.
- **Spec edits.** `object-store` MODIFY "Full provider capabilities — byte ranges,
  multipart uploads, and conditional writes" (adds `server_side_copy`); `hef-write-path`
  MODIFY "Compaction folds sidecar files into the base" (uses it).

## 3. Reuse BLAKE3 subtree hashes for the unchanged prefix; re-hash relocations

- **Tension.** `file_blake3` of the replacement must still be computed. Recomputing it
  over the whole file re-reads every reused byte — the cost splice avoids on upload, paid
  again on hashing. But BLAKE3 chunk counters are position-dependent, so a subtree hash is
  only valid if the bytes did not move.
- **Decision.** Where unchanged stripes form a byte-identical *prefix* of the replacement
  (their positions and the footer positions did not move), reuse the outboard BLAKE3
  tree's subtree hashes for that prefix as-is and hash only the new bytes. Where a reused
  stripe is relocated to a new offset, re-hash it from the local cache tier or a ranged
  read rather than trust the moved-position subtree hashes. BLAKE3 stays the sole
  authority and the staged-verification publish boundary is unchanged.
- **Why.** The position-dependence of chunk counters is exactly the condition the
  prefix case satisfies and the relocation case violates, so the split is principled, not
  a heuristic. Re-hashing a relocated stripe from the cache still avoids the upload, which
  is the expensive direction. Subtree-hash reuse is a computation shortcut over the same
  tree, not a new trust root.
- **Rejected.** *Trust moved-position subtree hashes.* — Produces a wrong `file_blake3`;
  the counters differ. *Skip hashing reused bytes entirely.* — Would make `file_blake3`
  no longer a hash of the actual file bytes, breaking the sole-authority invariant.
- **Spec edits.** `hef-write-path` MODIFY "Compaction folds sidecar files into the base".

## 4. Splice records nothing new: invisible to readers, nothing to feature-flag

- **Tension.** Splice could be surfaced — a manifest flag, a feature bit — so tooling can
  see which files were spliced, or it could be pure mechanism with no on-disk trace.
- **Decision.** The manifest entry for a spliced replacement records nothing new: an
  ordinary immutable object with an ordinary `file_blake3`, in the same one HEF format. A
  reader cannot distinguish a spliced file from a fully rewritten one.
- **Why.** The output is byte-identical to a full rewrite by construction (decisions 1–3),
  so a distinguishing field would carry no reader-relevant information and would be a new
  thing to keep consistent. Because nothing new lands on disk, there is nothing for an old
  reader to refuse on — the feature-directory discipline is not even engaged.
- **Rejected.** *A "spliced" manifest flag.* — Pure noise to readers and a new invariant
  to maintain, for a property that is already guaranteed byte-for-byte.
- **Spec edits.** `hef-write-path` MODIFY "Compaction folds sidecar files into the base".
