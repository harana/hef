# Design — add-hef-batch-write-read-api

## 0. The confirmed shape of both gaps

**Write.** `server/crates/storage/src/hef/writer/pipeline.rs` —
`WorkerCommitPipeline::submit` takes one `EventInput` and pushes one
`PendingRecord`; `CommitQueue::push` (`queue.rs`) returns `QueueError::Full`
independently on each call once the bounded ring has no room. A caller with N
events that belong together runs N submits, and a refusal at event k leaves
events 1..k queued: they flush, harden, and become visible on the ordinary
path while the caller holds `QueueError::Full` for what it considers one unit
of work. The all-or-nothing primitive already exists in the same file:
`CommitQueue::can_admit(&records)` walks a slice and reports whether *every*
record fits, mutating nothing — it is what makes `steal_from` safe ("a claimed
record is never dropped between the two queues"). The ingest path is the one
admission point that does not call it.

**Read.** `server/crates/storage/src/hef/layout/reader.rs` —
`HefFile::payload(row_ordinal)` / `payload_path(row_ordinal, path)` are the
reader's only payload access. Each call runs `granule_of_row` (a linear `find`
over `footer.granules`), `payload_granule` (a linear `find` over
`footer.payload_granules`), and `granule_dictionary` (a fresh
`decode_variant_dictionary` — nothing caches it); only the column blocks are
amortized, through `cached_column`. Reading a thousand payloads out of one
granule repeats the per-granule work a thousand times, and a stream of
independent single-row reads never forms the set of ranges
`build_plan_for` (`query/src/scan/remote_read.rs`) exists to coalesce.

## 1. Batch append: admission before queuing, contiguity for free

A batch entry point beside `submit` — `WorkerCommitPipeline::submit_batch` —
encodes the caller's events into pending records, checks `can_admit` for all
of them, and pushes none unless every one fits. A refused batch returns the
queue-full refusal for the batch as a whole and leaves the queue's cursors
untouched; the caller retries the whole batch when there is room, with no
duplicated prefix and no lost suffix. Tenant validation happens for the whole
batch before anything is queued, for the same reason.

Contiguity falls out rather than needing machinery: a worker's queue drains in
order and `flush` reserves one contiguous sequence range for the records it
claims, so a batch admitted as a block already occupies a contiguous
`(epoch, sequence)` sub-range in submission order. The receipt side already
resolves one position per event: `record_receipts` emits one `RetryReceipt`
per event at `first_sequence + row`. No new state, no batch identifier, no
change to `flush`, stealing, voids, or durability.

## 2. Batched payload read: group by granule, keep the caller's order

`HefFile::read_payloads(&[PayloadRef]) -> PayloadBatch` (the types from
`api-signatures.md`, now real): sort the requested references by row ordinal,
walk them granule by granule — resolving the granule entry and payload granule
once per group and decoding the granule dictionary lazily at most once per
group — and write each result back into the caller's original position.
Within a granule rows are visited in ascending order, which is what lets a
remote-read planner coalesce the resulting ranges. An absent payload reports
`PayloadRead::None` in its own position — the same outcome `payload` returns
for a row that stores none — rather than failing the batch; a reference
outside the file's rows fails the call exactly as the per-row read does.

The per-row `payload` keeps its exact behavior (including decoding the
dictionary on each call) and both paths share one reconstruction body, so the
batched path cannot drift from the oracle: the only difference is where the
granule state comes from. A decode counter on the reader
(`granule_dictionary_decodes`) makes the amortization observable to the
conformance test without touching any read result.

## 3. What deliberately does not change

- Backpressure: the ring stays bounded and still refuses work; only the
  refusal point moves.
- The per-row read: unchanged, and the oracle the batched path is checked
  against.
- No format bytes, no optional feature, no index, no change to durability,
  checksums, ordering, or any value a reader returns.
