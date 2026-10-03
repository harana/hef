Status: Approved (2026-08-18)

## Why

The `hef-apis` requirement "Writer, reader, and aggregation traits" names a
batch append (`EventJournal::append_batch`) and a batch payload read
(`EventFileReader::read_payloads`), and `api-signatures.md` gives both a
signature — but neither has a contract, and neither exists in the engine.
`append_batch`, `read_payloads`, `PayloadRef`, and `PayloadBatch` have no hits
under `server/crates`. Every other API in the capability says what a caller
gets; these two say only that a method exists (issue #13782).

Both gaps are the same gap seen twice: HEF batches by accumulation — a flush
target, a granule, a stripe — and gives a caller who knows N things belong
together no way to say so.

- **Write** (#13783) — `WorkerCommitPipeline::submit` admits one event, so a
  caller runs N submits and the bounded queue can refuse at event k with
  events 1..k already queued and on their way to durable. Retrying the batch
  duplicates the committed prefix; not retrying loses the suffix; neither is
  discoverable from `QueueError::Full`. The all-or-nothing primitive already
  exists and is trusted: `CommitQueue::can_admit` is what makes work-stealing
  safe; the ingest path just does not call it.
- **Read** (#13784) — `HefFile::payload` / `payload_path` are the only payload
  access, so N reads repeat the granule directory walk, the payload directory
  walk, and the granule dictionary decode N times, and produce byte ranges the
  remote-read planner (`build_plan_for`, which plans over a *set* of ranges)
  is never handed as a set to coalesce.

## What Changes

Two ADDED requirements on `hef-apis`, and the code plus conformance tests that
implement them:

1. **Journal batch append admits a caller batch whole or not at all.**
   Admission is decided for the whole batch before any event of it is queued;
   a refused batch leaves nothing queued, durable, or visible and is
   retryable as one unit; an admitted batch keeps caller order in one
   contiguous `(epoch, sequence)` sub-range; the receipt resolves one position
   per submitted event on commit. Backpressure stays — the queue must still
   refuse work — the change is only *where* the refusal lands: before the
   batch instead of part-way through it.

2. **Batched payload read amortizes per-granule work.** One result per
   reference in caller order, identical to the per-row read; granule lookup,
   dictionary decode, and column-block decode at most once per granule per
   call; ascending row order within a granule so ranges coalesce; an absent
   payload reported in place without failing the batch. The per-row read
   stays and stays the oracle the batched path is checked against.

No format bytes, no optional feature, no index, no change to durability,
checksums, ordering, or any value a reader returns.

**Out of scope, and why** — `EventFileWriter::append_event_batch` (the file
writer already takes a batch: `build_hef_file(rows, config)`),
`EventFileReader::read_columns` (columnar reads are already set-shaped via
`read_column`, `bulk_read_family`, `HefExec::execute_arrow`), and a batch
ingest route on the public HTTP API (`POST /api/v1/event-stream-events` takes
one CloudEvent per request; that is the API capability's question, and nothing
here forecloses it).

## Capabilities

### Added Capabilities

- `hef-apis` — ADD "Journal batch append admits a caller batch whole or not at
  all": batch admission decided before any event is queued, refusal leaves
  nothing queued and is retryable as one unit, admission yields one contiguous
  `(epoch, sequence)` sub-range in submission order with one receipt position
  per event.
- `hef-apis` — ADD "Batched payload read amortizes per-granule work": one
  result per reference in caller order identical to the per-row read,
  per-granule work done at most once per call, ascending row order within a
  granule, absent payloads reported in place.

## Impact

- **A caller's batch is never half-admitted.** The queue-full refusal lands
  before the batch instead of at event k, so a refused batch is retryable as
  one unit with no duplicated prefix and no lost suffix.
- **Reading N payloads stops paying per-granule work N times.** Granule
  directory walk, payload directory walk, and dictionary decode happen once
  per granule per call; only the per-row slot resolution and residual read
  stay per row.
- **The remote-read planner gets a set to plan over.** Ascending row order
  within a granule is what lets `build_plan_for` coalesce the surviving byte
  ranges into fewer, larger requests on a remote file.
- **No correctness surface is added.** The per-row read is unchanged and is
  the oracle: the batched path returns the same values in the same order.
  Durability, checksums, ordering, and visibility are untouched.

## Open Questions

None. Both requirements are pinned to existing, trusted mechanisms
(`can_admit` for admission; the granule directories and dictionary the per-row
path already reads) and add no format bytes or features.
