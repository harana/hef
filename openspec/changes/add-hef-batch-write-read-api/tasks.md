# Tasks — add-hef-batch-write-read-api

> Issue #13782. The `hef-apis` requirement "Writer, reader, and aggregation
> traits" names `EventJournal::append_batch` and
> `EventFileReader::read_payloads` and `api-signatures.md` gives both a
> signature, but neither has a contract and neither exists under
> `server/crates`. Both gaps are one gap seen twice: HEF batches by
> accumulation and gives a caller who knows N things belong together no way to
> say so. Spec deltas land with the code; the engine entry points are
> `WorkerCommitPipeline::submit_batch` (write, #13783) and
> `HefFile::read_payloads` (read, #13784).

## hef-apis — the two ADDED requirements

- [x] ADD "Journal batch append admits a caller batch whole or not at all":
      admission decided for the whole batch before any event is queued; a
      refused batch leaves nothing queued, durable, or visible, leaves the
      queue's cursors untouched, and is retryable as one unit; an admitted
      batch keeps caller order in one contiguous `(epoch, sequence)` sub-range
      with one receipt position per event; backpressure stays.
- [x] ADD "Batched payload read amortizes per-granule work": one result per
      reference in caller order, identical to the per-row read; granule
      lookup, payload-directory lookup, and dictionary decode at most once per
      granule per call; ascending row order within a granule; an absent
      payload reported in place without failing the batch.

## Code — batch append (req: hef-apis "Journal batch append admits a caller batch whole or not at all")

- [x] `server/crates/storage/src/hef/writer/pipeline.rs`: add `submit_batch`
      beside `submit` — validate every event's tenant, encode the batch into
      pending records, check `CommitQueue::can_admit` for all of them, and
      push none unless every one fits; a refused batch returns the queue-full
      refusal for the batch as a whole with the queue's cursors untouched.
      Contiguity and per-event receipts fall out of the existing `flush`
      (one reserved range per claim; `record_receipts` one receipt per event).

## Code — batched payload read (req: hef-apis "Batched payload read amortizes per-granule work")

- [x] `server/crates/storage/src/hef/layout/reader.rs`: add `PayloadRef`,
      `PayloadBatch`, and `HefFile::read_payloads` — group references by
      granule, resolve the granule entry and payload granule once per group,
      decode the granule dictionary lazily at most once per group, visit each
      group's rows ascending, return results in the caller's original order,
      and report an absent payload in place. The per-row `payload` keeps its
      exact behavior and both paths share one reconstruction body so the
      batched path cannot drift from the oracle.

## Tests (conformance, under `crates/conformance/tests/conformance/hef_apis/`)

- [x] `hef-apis`: a batch that does not fit admits nothing and leaves the
      queue's cursors unchanged, then succeeds once there is room; an admitted
      batch's events occupy one contiguous sequence sub-range in submission
      order after the flush that carries them, one receipt per event (#13783).
- [x] `hef-apis`: batched and one-at-a-time payload reads agree on values and
      order; a batch confined to one granule decodes that granule's dictionary
      once for the call rather than once per reference; one absent payload
      reports absent in place without failing the batch (#13784).

## Companion edits (applied at archive, not part of the requirement deltas)

- [ ] `hef-apis/api-signatures.md`: no edit needed — `append_batch` and
      `read_payloads(&[PayloadRef]) -> PayloadBatch` are already declared
      there; this change gives them their contracts.

## Verification

- [x] `openspec validate add-hef-batch-write-read-api --strict` green; every
      `### Requirement:` in the delta carries SHALL and at least one
      `#### Scenario:`.
