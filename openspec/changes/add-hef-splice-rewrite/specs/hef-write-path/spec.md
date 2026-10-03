## MODIFIED Requirements

### Requirement: Compaction folds sidecar files into the base
SuperHEF compaction SHALL fold sidecar files into the rewritten base: promotion-backfill vertical projections SHALL be folded in and dropped (they are transient bridges), and derived-columns sibling files SHALL be folded in once their columns are settled per their declared settle horizon, with a new sibling re-emitted for the unsettled tail. Folding SHALL preserve row alignment, deletion-vector and correction accounting, and BLAKE3 verification, and SHALL publish atomically in one manifest generation.

The rewrite that performs a fold SHALL plan stripe reuse before moving bytes. A rewrite planner SHALL classify each source stripe as *unchanged* — same rows, same lifecycle-selected encoding strategy, and no intersecting deletion vector or sidecar fold — or *rebuilt*; the classification SHALL be conservative, so any stripe whose reuse cannot be proven byte-identical SHALL be rebuilt. An unchanged stripe SHALL be copied into the replacement file **by reference** rather than re-read and re-uploaded: through the provider's native `server_side_copy` on a cloud durable tier (an S3 `UploadPartCopy`, a GCS compose, or an Azure Put Block From URL, with parts at or above the provider's minimum copy-part size), through `copy_file_range`/reflink on the local tier where the capability probe confirms it, and through a portable buffered copy otherwise; the three paths SHALL produce byte-identical replacement bytes (INV-HARDWARE-ACCEL, the buffered path is the oracle). Because upload parts are stripe-aligned (see `hef-file-layout` — the stripe offset domain and stripe-aligned upload parts), an unchanged stripe SHALL copy as whole parts without any read-modify-write.

Integrity SHALL be preserved without re-reading reused bytes. `file_blake3` of the replacement SHALL still be computed and SHALL remain the sole integrity authority. Where unchanged stripes form a byte-identical *prefix* of the replacement (stripes and their footer positions did not move), the rewriter SHALL reuse the outboard BLAKE3 tree's subtree hashes for that prefix as-is — the chunk counters are position-dependent and the positions are unchanged — and SHALL hash only the new bytes. Where a reused stripe is relocated to a new offset, the rewriter SHALL re-hash it from the local cache tier or a ranged read rather than trust the moved-position subtree hashes, which still avoids re-uploading it. The staged-verification step of the HEF publish boundary SHALL be unchanged: the replacement is verified and published exactly as any freshly written file.

Splice SHALL be pure mechanism, invisible to readers: the manifest entry for a spliced replacement SHALL record nothing new — it is an ordinary immutable object with an ordinary `file_blake3`, in the same one HEF format, and a reader SHALL NOT be able to tell a spliced file from a fully rewritten one.

#### Scenario: Settled derived columns folded in
- **WHEN** compaction rewrites a range whose derived-columns sibling has columns past their settle horizon
- **THEN** the settled columns are embedded in the new base file and a sibling is re-emitted only for the unsettled tail

#### Scenario: Unchanged stripe copied by reference, not re-uploaded
- **WHEN** a fold changes only a few stripes and the planner classifies the remaining stripes as unchanged
- **THEN** each unchanged stripe is copied into the replacement by the durable tier's `server_side_copy` (or `copy_file_range`/reflink locally, or a buffered copy on the fallback) as whole stripe-aligned parts, its bytes never routed through the client, and the buffered-copy result is byte-identical

#### Scenario: Prefix subtree hashes reused, only new bytes hashed
- **WHEN** the unchanged stripes form a byte-identical prefix of the replacement file and new stripes and the footer follow
- **THEN** `file_blake3` is computed by reusing the outboard tree's subtree hashes for the unchanged prefix and hashing only the new bytes, while a stripe that was relocated to a new offset is re-hashed from the cache tier or a ranged read

#### Scenario: Any doubt rebuilds the stripe
- **WHEN** the planner cannot prove a source stripe would reproduce byte-identically (its encoding strategy, row set, or an intersecting deletion vector is uncertain)
- **THEN** the stripe is rebuilt rather than copied by reference, and correctness never depends on an unproven reuse

#### Scenario: Spliced file is indistinguishable to readers
- **WHEN** a reader opens a replacement file produced by splice
- **THEN** it sees an ordinary immutable HEF object with an ordinary `file_blake3` in the one HEF format, with no manifest field, feature bit, or on-read behavior revealing that stripes were copied by reference
