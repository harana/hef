The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-manifest-footer-mirrors/).

Status: Approved (2026-07-09)

## Why

A tenant-day query plans over many files at once — the daily SuperHEF, plus recent
committed HEFs, plus projections. Opening each file needs its footer, and even when a
remote open is already a single exact-range tail read per file, cold planning still
pays one object request for every file the plan touches. On S3-class stores those
requests are the dominant cost of a cold query: dependent round trips before any data
comes back, multiplied by the file count.

The manifest already copies small per-file summaries, and the format deliberately keeps
large metadata out of the manifest itself — but it explicitly allows *manifest-native
auxiliary metadata* (`hef-core-invariants` — Requirement: "Barriers, deletion vectors,
projections, and layout classes": "auxiliary metadata SHALL be HEF-native or
manifest-native"). A footer mirror lives exactly in that allowance: it is the same trick
Iceberg's manifest files and ClickHouse's packed marks caches use, applied at the
visibility boundary HEF already owns, and it sits right where the deletion-vector
generation records already live. It does not add a new file *shape* (Decision 16 covers
data files; this is manifest-native metadata), and it is pure acceleration state
(Decision 20): losing a mirror costs requests, never correctness.

## What Changes

**A manifest generation MAY publish one optional footer-mirror object.** The mirror
concatenates, keyed by `file_id`, the footer sections of the files published in that
generation (optionally scoped to a tenant-day), compressed as one object. A planner
fetches that single object and opens every file in the generation from the mirrored
sections, instead of one tail read per file. Each per-file section carries its own
authoritative BLAKE3 checksum and records the `file_id` and generation it mirrors, so a
section validates on its own against the manifest entry.

**Any mismatch falls back to the authoritative per-file footer.** A section whose BLAKE3
does not verify, whose recorded `file_id` or generation disagrees with the manifest
entry, or that is missing — or a generation the manifest reports has no mirror — sends
the planner to read that file's footer directly from its own tail. That fallback is the
status quo per-file open and yields identical results, so a stale or wrong mirror can
never change a query answer. The HEF footer of each file stays authoritative; the mirror
is never a source of truth for a footer.

**The mirror is a feature-directory-gated, rebuildable acceleration object.** A reader
that does not understand the mirror feature ignores it and opens files from their own
footers (refuse to the authoritative path). The object service stores and serves the
mirror as a normal immutable, tenant-qualified object — cacheable and promotable like any
other object, never exposing raw local paths — but treats it as droppable acceleration
state: it does not count against tenant storage quota, never authorizes cross-node
visibility, and is rebuildable from the files the generation published. BLAKE3 stays the
sole integrity authority on every mirrored section, exactly as for the file footers
themselves.

## Capabilities

### Added Capabilities

- `hef-manifest-integration` — ADD "Footer mirror for opening a generation in one
  request": the optional, per-generation (or per-tenant-day) footer-mirror object keyed
  by `file_id`, its per-section BLAKE3 and generation checks, the fall-back-to-tail rule
  on any mismatch, and the feature-directory gating that keeps old readers on the
  authoritative footer path.
- `object-service` — ADD "Footer-mirror objects are rebuildable acceleration state": the
  object service stores and serves the mirror as a normal immutable tenant-qualified
  object, cacheable and rebuildable, that does not count against quota, never authorizes
  visibility on its own, and never stands in for a file's authoritative footer.

## Impact

- **Cold tenant-day planning opens N files in one request.** Where a mirror is present,
  the plan pays a single object read for the generation's footers instead of one tail
  read per file, cutting the dominant cold-query cost on S3-class stores.
- **No new file shape and no correctness dependency.** The change is manifest-native
  auxiliary metadata within the existing `hef-core-invariants` allowance; the closed set
  of HEF file shapes is untouched (Decision 16), and the mirror is droppable acceleration
  state (Decision 20) whose loss costs only requests.
- **BLAKE3 stays the sole integrity authority.** Every mirrored footer section carries
  and is validated against its own BLAKE3; any mismatch, staleness, or generation
  disagreement routes to the authoritative per-file footer, so the mirror can never
  change a result.
- **Old readers refuse.** The mirror is feature-directory-gated: a reader without
  the feature ignores it and opens from the authoritative footers, per HEF's
  feature-directory discipline.
- **Quota and visibility are unchanged.** The mirror does not count against tenant
  storage quota and never makes data visible; UnifiedEvents reference plus authoritative
  BLAKE3 on the files' own footers continue to gate query visibility.

## Open Questions

1. **Generation vs. tenant-day scope.** Whether a mirror is published per manifest
   generation, per tenant-day, or both is left to the builder; the requirement fixes only
   that the mirror is keyed by `file_id`, per-section validated, and rebuildable, and that
   the fallback is per-file tail reads either way.
2. **Compression codec and section framing.** The concrete compression codec and the
   in-object section framing are an implementation choice; the requirement fixes only that
   the object is compressed, keyed by `file_id`, and that each section carries its own
   BLAKE3 and generation.
3. **When to (re)build a mirror.** Whether a mirror is built eagerly at publication or
   lazily on first cold open, and its eviction policy, is a builder/object-service tuning
   question; correctness holds for any choice because the fallback is the authoritative
   per-file footer.
