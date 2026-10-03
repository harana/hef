# HEF Write Path — Ingest to Durable Acknowledgement, Safe Retry, Publish, and Rewrite

Companion artifact for the `hef-write-path` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


### Ingest to durable acknowledgement

```text
1. Ingest worker validates event.
2. Ingest worker serializes the event into a pending `harana_hej_compact_batch_v1` record descriptor.
3. The pending descriptor is appended to the worker's lock-free serialized commit queue.
4. The event is in READY state. It is not durable, not queryable, and not public.
5. The worker checks whether its unclaimed pending bytes have reached the selected flush target.
6. The default flush target is 16 KiB; latency-critical and low-load force-commit flushes use 4 KiB.
7. The worker may topology-locally steal same-tenant/same-epoch pending records from peer queues using the clean-cursor CAS rule.
8. The claiming worker reserves one contiguous `(epoch, sequence)` range for the final HEJ frame.
9. The worker writes `first_sequence`, `last_sequence`, and per-row sequence order into `HEJFrameHeaderV1` and the compact batch.
10. The worker submits the aligned HEJ frame through io_uring: io_uring_cmd NVMe passthrough on the journal char device, or filesystem io_uring in the non-NVMe dev fallback.
11. Completion is polled by the submitting worker.
12. Header CRC-64/NVME and frame BLAKE3 are verified.
13. Event becomes HARDENED in HEJ after the selected durability policy completes.
14. Minimal safe-retry metadata is committed or made reconstructable.
15. For ordinary append-only event ingest, HARDENED becomes COMMITTED immediately.
16. For routes with dependent mutable state, autonomous acknowledgement checks the route dependency condition before COMMITTED.
17. The durable HEJ frame is decoded into the fixed LiveOverlay Arrow RecordBatch schema.
18. Event is published into LiveOverlay as part of an immutable Arrow RecordBatch segment.
19. Client is acknowledged according to the API durability/freshness contract.
20. Event becomes eligible for HEF publication.
```

The latency paper’s core recommendation is exactly this direction: workers independently flush logs when their local threshold is reached, avoid large SSD writes, avoid centralized group commit, and decouple log flush from commit acknowledgement. HEJ applies that to event ingest by treating append-only event dependencies as trivially satisfied after journal durability, while retaining the autonomous acknowledgement machinery only for routes that truly have dependent mutable state.

LiveOverlay publication must not redefine durability. Durability is HEJ completion. LiveOverlay publication defines query visibility for HEJ-backed rows not yet HEF-covered.

Reference: <https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf>

### HEJ and safe retry

HEJ is the protected event payload record. The system must not require both a full KV buffer payload and a HEJ payload before acknowledgement.

Safe retry uses:

```text
client_request_id hash or connector delivery identity;
tenant/account scope;
commit receipt;
HEJ frame cursor;
response status class;
expiry;
minimal dedupe metadata.
```

After restart/takeover, safe-retry state is reconstructed from retained replay-guard rows plus HEJ coverage. Replaying a retained HEJ frame must not enqueue duplicate events when the original acknowledgement is still within the replay-guard window.

### Publishing HEF from HEJ

```text
1. Vended durable HEJ ranges are read by the HEF publisher.
2. HEJ frames are validated with header CRC-64/NVME, frame BLAKE3, and segment BLAKE3 chain.
3. harana_hej_compact_batch_v1 payloads are decoded using the public deterministic mapping.
4. Events are decoded into HEF column builders.
5. Granules are formed using index_granularity and index_granularity_bytes.
6. Dictionaries are built.
7. Promoted attributes and shredded payload paths are selected from per-path statistics.
8. Adaptive encoding samples are collected and encoding pipelines are chosen per block.
9. HEF stripe, granule, marks, and page data are written.
10. Required SkipIndex, exact aggregate, and checksum blocks are written.
11. Optional feature blocks are written only when enabled by the HEF publication policy and benchmark gates.
12. Optional projections are written when benchmark-justified.
13. File-level directory and footer are written.
14. BLAKE3 checks are finalized.
15. Manifest entry, projection metadata, deletion-vector generation, and coverage watermarks are atomically published.
16. HEF journal coverage is advanced in the manifest.
17. LiveOverlay segments fully covered by the new published HEF range become evictable.
18. HEJ retention cursor and segment recycling eligibility advance only after publication and the configured recovery safety window.
```

Where the durable object store's provider supports multipart uploads, the publisher uploads each stripe's bytes as it seals (step 9) as stripe-aligned multipart part(s), pipelining the upload behind the ongoing build rather than staging the whole file first. The multipart upload is **completed** — the step that first makes the object readable — only at the publish boundary (steps 13–15), after the footer is written and the authoritative file BLAKE3 verifies, so visibility stays gated exactly as before. A publish attempt that fails a check or loses the manifest commit race after uploading parts aborts the multipart upload, so a losing attempt completes no object; an incomplete upload orphaned by a crashed publisher (no live handle to abort) is reaped by provider lifecycle policy rather than an in-process sweep. Where the provider has no multipart support (the local-directory default), the publisher falls back to finalize-then-upload with a byte-identical result.

The HEF publisher must be idempotent. Re-publishing the same journal range must produce either the same file identity or a safely replaceable file.

### HEF rewrite and repacking

```text
1. Select manifest-published Active HEF files and visible HEF-native deletion-vector/correction metadata.
2. Mark selected source files Outdated only after the replacement generation is published.
3. Choose target layout_class and projection set based on workload and policy; select rewrite scope incrementally using per-granule clustering_quality (see the hef-layout-and-clustering capability) rather than whole-file resorts by default.
4. Merge and sort selected rows into granules.
5. Apply safe deletion-vector removal and correction folding when allowed.
6. Rebuild dictionaries, promoted columns, variant_shredded_field_blocks, variant dictionary blocks, and adaptive encodings.
7. Build required marks, granule directories, SkipIndexes, and aggregates.
8. Build optional indexes, rollups, sparse cubes, sketches, context blocks, and optional internal vector blocks when benchmark-justified.
9. Write replacement HEF file(s) using the same HEF format.
10. Verify BLAKE3 checksums, aggregate coverage, marks coverage, and deletion-vector accounting.
11. Publish new generation manifest atomically.
12. Move superseded files to DeleteOnDestroy only after the safety window and in-flight query horizon expire.
```

HEF rewrite is an optimization and cleanup mechanism. It does not create a new file format and does not create separate freshness, historical, cold, or archive HEF classes.

