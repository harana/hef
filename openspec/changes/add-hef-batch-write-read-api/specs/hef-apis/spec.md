## ADDED Requirements

### Requirement: Journal batch append admits a caller batch whole or not at all
The journal write path SHALL expose a batch append beside the per-event submit, and admission of a caller's batch SHALL be decided for the whole batch before any event of it is queued. A batch the queue cannot hold SHALL be refused whole: the refusal SHALL leave nothing from the batch queued, durable, or visible, SHALL leave the queue's cursors untouched, and the batch SHALL be retryable as one unit once there is room — never a duplicated prefix, never a lost suffix. An admitted batch SHALL keep the caller's submission order and SHALL occupy one contiguous `(epoch, sequence)` sub-range when the flush that carries it commits, and the commit SHALL resolve one receipt position per submitted event, in submission order. Backpressure itself SHALL remain: the queue stays bounded and refuses work it cannot hold — only the refusal point moves, from part-way through a batch to before it.

#### Scenario: A batch that does not fit admits nothing
- **WHEN** a caller submits a batch the bounded queue cannot admit whole
- **THEN** the batch is refused as one unit, nothing from it is queued, durable, or visible, the queue's cursors are unchanged, and the same batch succeeds once there is room

#### Scenario: An admitted batch commits one contiguous sub-range in submission order
- **WHEN** an admitted batch's events are flushed to durability
- **THEN** they occupy one contiguous `(epoch, sequence)` sub-range in the caller's submission order, and the commit resolves one receipt position per submitted event

### Requirement: Batched payload read amortizes per-granule work
The reader SHALL expose a batched payload read — `read_payloads(&[PayloadRef]) -> PayloadBatch` — that returns exactly one result per requested reference, in the caller's original order, and each result SHALL be identical to what the per-row payload read returns for the same row; the per-row read remains available and is the oracle the batched path is checked against. Within one call, granule lookup, payload-directory lookup, and granule-dictionary decode SHALL happen at most once per granule touched, not once per reference, and column-block decode SHALL happen at most once per `(column, granule)`. Rows within a granule SHALL be visited in ascending row order so the byte ranges a remote-read planner sees can coalesce. A reference whose row stores no payload SHALL be reported as absent in its own position without failing the batch — the same outcome the per-row read returns for that row.

#### Scenario: Batched and per-row reads agree on values and order
- **WHEN** the same references are read once through `read_payloads` and once through per-row `payload` calls
- **THEN** the batched result has one entry per reference in the caller's original order, and every entry equals the per-row result for the same row

#### Scenario: One granule's dictionary is decoded once per call
- **WHEN** a batch of references confined to one granule is read through `read_payloads`
- **THEN** that granule's dictionary is decoded once for the call, not once per reference

#### Scenario: An absent payload reports absent in place
- **WHEN** a batch includes a reference to a row that stores no payload
- **THEN** that reference's position reports the payload as absent and every other reference still returns its value — the batch does not fail
