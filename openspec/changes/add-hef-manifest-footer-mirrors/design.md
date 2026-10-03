The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-manifest-footer-mirrors/).

# Design — Manifest-native footer mirrors

Four decisions in the tension → decision → why → rejected → spec-edits style of
`docs/design-review-decisions.md`. The running theme: the footer mirror is pure
acceleration placed inside the allowance the format already grants for manifest-native
auxiliary metadata. Every decision is bent toward keeping the HEF footer authoritative,
the mirror droppable, and BLAKE3 the sole integrity authority — so a wrong or missing
mirror can only ever cost a request.

## 1. The mirror is manifest-native auxiliary metadata, not a new file shape

- **Tension.** "Open N files in one request" wants a single blob of everyone's footers.
  That blob could be modeled as a new HEF file *shape* (a footer-bundle file), or as
  manifest-native auxiliary metadata alongside the deletion-vector generation records the
  manifest already tracks. A new shape reopens the closed file-shape set (Decision 16); a
  manifest-native object stays inside an allowance the format already grants.
- **Decision.** The mirror is a manifest-native auxiliary object, permitted by
  `hef-core-invariants` — Requirement: "Barriers, deletion vectors, projections, and
  layout classes" ("auxiliary metadata SHALL be HEF-native or manifest-native"). It adds
  no new data-file shape and does not touch the closed set of HEF file shapes.
- **Why.** The manifest already copies per-file summaries and tracks per-generation
  auxiliary records (deletion-vector generations); the mirror is the same kind of thing —
  small, generation-scoped, rebuildable metadata — placed exactly where that metadata
  already lives. It is the trick Iceberg's manifest files and ClickHouse's packed marks
  caches use, applied at the visibility boundary HEF already owns.
- **Rejected.** *A new footer-bundle file shape.* — Reopens the closed file-shape set
  (Decision 16), forces every reader and the layout classes to learn a new shape, and
  buys nothing the manifest-native object does not.
- **Spec edits.** `hef-manifest-integration` ADD "Footer mirror for opening a generation
  in one request".

## 2. The HEF footer stays authoritative; the mirror is droppable acceleration state

- **Tension.** A mirror that planners trust for opens is tempting to treat as a small
  source of truth. But if the mirror is ever authoritative, losing or corrupting one
  becomes a correctness event, and the format's Decision 20 (every acceleration
  droppable) is violated.
- **Decision.** The mirror is droppable acceleration state (Decision 20): each file's own
  HEF footer stays authoritative, the mirror is never a source of truth for a footer, and
  its absence or loss costs only extra requests. The mirror is rebuildable from the files
  the generation published.
- **Why.** Keeping the footer authoritative means the mirror needs no durability or
  consistency guarantee of its own — it can be cached, evicted, or never built, and the
  worst case is the status-quo per-file open. That is the same posture the rebuildable
  local cache and every other acceleration state already take.
- **Rejected.** *Mirror as an authoritative footer store* (drop the per-file footer once
  mirrored). — Makes mirror loss a data-loss event, contradicts Decision 20, and removes
  the fallback that makes the feature safe.
- **Spec edits.** `hef-manifest-integration` ADD "Footer mirror…"; `object-service` ADD
  "Footer-mirror objects are rebuildable acceleration state".

## 3. Every mismatch falls back to the per-file authoritative footer

- **Tension.** A mirror can go stale or wrong: a file republished at a new generation, a
  section corrupted, a section missing, or a manifest that reports no mirror at all. If a
  planner opened a file from a stale or wrong section, it could read against the wrong
  footer and change a result. The mirror needs a validation that can never be fooled and
  a well-defined escape.
- **Decision.** Each per-file section carries its own authoritative BLAKE3 and records the
  `file_id` and generation it mirrors, so it validates on its own against the manifest
  entry. On any mismatch — BLAKE3 fails, `file_id` or generation disagrees, section
  missing, or no mirror published — the planner reads that file's footer directly from its
  own tail (the authoritative per-file path), yielding an identical result. BLAKE3 stays
  the sole integrity authority for every mirrored section.
- **Why.** Per-section BLAKE3 plus the generation/`file_id` cross-check against the
  manifest means a wrong section is always detected before use, and the fallback is the
  exact per-file open the planner would otherwise have done — so the mirror can only ever
  save a request, never change an answer. This mirrors the range-verification discipline
  the object cache already uses (BLAKE3 authoritative, non-authoritative prechecks only).
- **Rejected.** *Trust the mirror without per-section verification* (a single object-level
  checksum). — A file republished at a new generation would still checksum-match the stale
  object; only a per-section `file_id`/generation check catches that. *Fail the whole
  query on a mirror mismatch.* — Turns a droppable accelerator into a fragility; the
  fallback must be silent and result-identical.
- **Spec edits.** `hef-manifest-integration` ADD "Footer mirror…" (the mismatch/fallback
  and per-section BLAKE3 clauses).

## 4. Feature-directory gating: old readers refuse to the authoritative footer

- **Tension.** Introducing the mirror must not let a reader that predates the feature
  misread the mirror or, worse, treat an object it half-understands as authoritative.
- **Decision.** The footer mirror is a feature-directory-gated capability. A reader that
  does not understand the mirror feature ignores the mirror entirely and opens files from
  their own footers — refuse to the authoritative path — and never partially reads a
  mirror it cannot fully validate.
- **Why.** This is HEF's standing feature-directory discipline: new format features are
  feature-flagged and old readers refuse. Because the authoritative per-file footer
  path is always available, an old reader loses only the acceleration, never correctness.
- **Rejected.** *Unconditional mirror reads* (no feature gate). — An older reader could
  misinterpret the object; gating plus the authoritative fallback removes that risk with
  no downside.
- **Spec edits.** `hef-manifest-integration` ADD "Footer mirror…" (the feature-gating
  clause).
