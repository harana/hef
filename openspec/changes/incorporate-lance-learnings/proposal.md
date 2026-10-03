The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-lance-learnings/).

Status: Approved (2026-08-18)

## Why

The 2026-08-18 Lance evaluation (`docs/HEF-vs-Lance-evaluation.md`, reading
`lancedb/lance` at `97d8413`) compared HEF against its closest open relative —
another immutable, footer-indexed columnar format built for object-store and
NVMe economics. Most axes confirmed work already landed or in flight (the
one-request cold open, footer mirrors, per-row offset slots for wide columns).
Two findings demand structural change: Lance keeps **every query accelerator
outside the data file** as separately versioned, partially covering, droppable
index artifacts — and HEF's own implementation history (nine index kinds built,
one persisted, because retrofitting an index into a sealed BLAKE3-hashed file
is a full rewrite) is the proof that in-footer skip-index blocks are the wrong
home for anything the writer cannot predict at publish time. And Lance's writer
runs in ~8 MiB-per-column memory while HEF's builder materializes the whole
~1 GiB file in RAM. A cluster of smaller operational rules — scheduler
admissions, deletion-vector density encoding, bounded reader caches,
late-materialization thresholds, fixture-corpus compatibility, object-key
entropy — round out the borrow list.

This change incorporates those learnings the same way `incorporate-vortex-learnings`
did: normative borrows become spec deltas, implementation shape becomes tracked
tasks naming the requirement it implements, ideas already covered by landed or
active work are recorded as such in `design.md`, and the evaluation's
explicitly-not-recommended list is affirmed so it is not re-litigated.

## What Changes

Spec deltas (all ADDED requirements):

1. `hef-query-metadata-and-indexes` — ADD "Heavy skip indexes publish as index
   artifacts outside the file" (P0-A): the always-on min/max tier stays in the
   footer; probabilistic filters, range filters, path presence, learned
   position, bitmaps, and vector structures become immutable, manifest-referenced,
   partially-covering, progressively-loaded, droppable artifacts with enumerated
   staleness rules — an index can be added to a sealed file without rewriting it.
2. `hef-manifest-integration` — ADD "Index artifacts ride the manifest
   generation" (P0-A): atomic publication with the generation, manifest-only
   add/drop/rebuild, sweeper-governed retirement.
3. `hef-write-path` — ADD "The builder streams at stripe scope with bounded
   memory" (P0-B): encode-upload-retire per stripe, incremental integrity
   digests, byte-identical to the materialized build, splice-reused stripes
   never resident.
4. `hef-benchmarks-and-acceptance-gates` — ADD "Writer peak-memory gate"
   (P0-B): stripe-scope memory ceiling gated at fresh publish and rewrite.
5. `hef-deletes-and-corrections` — ADD "Deletion vectors choose their wire
   encoding by density" (P1-C): sorted ordinal array below a pinned threshold,
   positional Roaring above, recorded in the ref's `encoding` field.
6. `hef-apis` — ADD "Reader decoded-block caches are bounded" (P1-D): byte
   budgets with eviction above the decoder, bulk-scan bypass, distinct from the
   page-granular object cache below it.
7. `query-execution` — ADD "Late materialization follows a pinned policy"
   (P2-A): deferral decided by column width × tier × estimated selectivity with
   pinned, testable defaults.
8. `hef-physical-artifacts` — ADD "External payload references carry a pinned
   descriptor" (P2-B): the `{blake3, kind, position, size, uri?}` tuple pinned
   now, placement machinery deferred.
9. `hef-reader-compatibility` — ADD "Released-version fixture corpus after
   format freeze" (P2-C): released-writer bytes committed and read by every
   future reader in CI.
10. `object-store` — ADD "Durable object keys carry a high-entropy prefix"
    (P2-D): content-derived maximum-entropy key prefixes inside the tenant
    prefix.

Folded into an active change rather than added here (P1-B): the five scheduler
rules from the evaluation's §3.6 — two-part (file, row) priority, the two
deadlock-avoidance admission exceptions, cancellation-safe budget refund, the
per-requested-range un-coalescing delivery contract, and explicit minimal-plan
degeneration on the local tier — are amendments to
`add-hef-remote-read-scheduler` (its delta, tasks, and design record them).

Deferred to its sibling change (P1-A): persisted constant/all-null flags are
owned by `incorporate-duckdb-learnings`, whose P0-B treatment (derive from
existing stats, materialize from metadata, elide the bytes) subsumes the Lance
proposal.

## Capabilities

### Modified Capabilities

- `hef-query-metadata-and-indexes`: one ADDED requirement (index artifacts).
- `hef-manifest-integration`: one ADDED requirement (artifact generation refs).
- `hef-write-path`: one ADDED requirement (streamed stripe-scope build).
- `hef-benchmarks-and-acceptance-gates`: one ADDED requirement (writer memory gate).
- `hef-deletes-and-corrections`: one ADDED requirement (density-adaptive encoding).
- `hef-apis`: one ADDED requirement (bounded reader caches).
- `query-execution`: one ADDED requirement (late-materialization policy).
- `hef-physical-artifacts`: one ADDED requirement (external payload descriptor).
- `hef-reader-compatibility`: one ADDED requirement (fixture corpus).
- `object-store`: one ADDED requirement (key entropy).

## Impact

- **The unimplemented half of the index tier becomes incremental work.** Every
  already-built index structure (`hef/indexes/`) gains a persistence path that
  needs no new footer section, no feature bit on the file, and no rewrite of
  any published object — and workload-driven index selection can finally run
  after the workload exists. Vector/ANN blocks get the same home when they land.
- **No format change to the sealed file.** The footer's always-on statistics
  tier is untouched; artifacts, streaming, cache budgets, and key naming all
  live outside the pinned byte layout. The deletion-vector encoding lands
  inside a feature (`HEF_NATIVE_DELETION_VECTORS`) that has no on-disk form yet,
  so nothing published changes meaning.
- **Determinism and integrity preserved.** The streamed build is byte-identical
  by requirement; artifacts are BLAKE3-verified, immutable, tenant-qualified
  objects; external payload references carry their own BLAKE3.
- **No overlap with active changes.** Scheduler rules landed in
  `add-hef-remote-read-scheduler`; cache residency below the decoder stays with
  `add-page-granular-object-cache`; wide-column point access stays with
  `add-hef-wide-column-point-access`; `design.md` records the full mapping.

## Open Questions

1. **Artifact reference schema detail.** Whether artifact coverage is stored as
   one record per `(file_id, granule range)` or as a compact per-artifact
   coverage bitmap is a manifest-schema question for the implementation
   proposal; the requirement pins only what coverage must declare.
2. **Deletion-vector density threshold.** The requirement pins the two-form
   shape and determinism; the numeric threshold awaits the measured
   correction/erasure workload distribution (Lance's 5,000 is the reference
   point, not the answer).
3. **Fixture corpus size budget.** How many fixtures per release and their byte
   budget in the repository is a CI-cost question settled when the freeze
   happens.
