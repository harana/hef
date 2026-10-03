## ADDED Requirements

### Requirement: Encoding selection may capture and replay a winning pipeline
After a column's block encodes, the encoder MAY capture the winning pipeline — transform, side-stream cascade, trailing compression, and their parameters — and replay it for subsequent blocks of the same column within the same build, skipping candidate sampling where the data distribution is stable. Replay SHALL be bounded by two re-arm rules: full selection SHALL re-run at a fixed granule cadence, and immediately when a replayed block's encoded-to-raw ratio drifts past a pinned bound (the size-regression trip-wire), so a distribution shift never rides a stale capture for long. Replay SHALL preserve determinism — whether a block is replayed or fully selected SHALL be a pure function of the preceding blocks of the same build in build order, so any node encoding the same rows produces byte-identical files — and it SHALL compose with the acceptance gate: every block, replayed or not, is still kept only when smaller than the plain form, so replay can never regress a block past plain. A capture MAY additionally be persisted per `(tenant, event_type, column)` in the catalog to seed the next build's first blocks, as a candidate-ordering prior only — the seeded winner is still verified like any sampled choice.

#### Scenario: A stable column stops re-sampling
- **WHEN** a column's granules share a stable distribution through a build
- **THEN** after the first fully selected block the encoder replays the captured pipeline, re-sampling only at the re-arm cadence, and two nodes encoding the same rows produce byte-identical blocks

#### Scenario: The trip-wire re-arms selection
- **WHEN** a replayed block's encoded-to-raw ratio drifts past the pinned bound
- **THEN** full candidate selection re-runs from that block onward and a fresh capture replaces the stale one

#### Scenario: Replay never beats the plain-form gate
- **WHEN** a replayed pipeline would produce a block no smaller than the plain form
- **THEN** the acceptance gate stores the plain form exactly as it would for a sampled winner

### Requirement: Dictionary alphabets may be shared at file scope
A file MAY carry, per dictionary-encoded column, one file-scope dictionary alphabet, so a low-cardinality column stops re-storing its sorted distinct values in every granule's block. A block whose distinct values are all present in the shared alphabet MAY store only its code stream plus a scope marker (`block` or `file`); codes against the shared alphabet SHALL keep the ascending sorted-order assignment, so compressed-form equality and range predicates on codes are unchanged and predicate-to-code translation runs once per file for shared-scope blocks instead of once per block. The shared alphabet SHALL be an optimization, never a constraint: a block containing values outside it SHALL keep a block-local dictionary, and sampling SHALL decide per block which scope wins. Because a reader that cannot resolve the shared alphabet cannot decode a shared-scope block at all, the section SHALL be governed by a **required** feature bit, following the `columnar_marks` precedent. An external (cross-file) alphabet scope SHALL be reserved but not defined — cross-file code translation remains the catalog global dictionary's job.

#### Scenario: A low-cardinality column stores its alphabet once
- **WHEN** a status-like string column repeats the same small value set across every granule of a file
- **THEN** the file stores one shared alphabet and each block stores only its code stream with the `file` scope marker, decoding to the same values as per-block dictionaries would

#### Scenario: Predicates translate once per file
- **WHEN** an equality or range predicate runs over a column whose blocks use the shared alphabet
- **THEN** the predicate resolves to codes against the shared alphabet once, and every shared-scope block is tested by code with no per-block dictionary work

#### Scenario: Novel values keep a local dictionary
- **WHEN** one granule's block contains a value absent from the shared alphabet
- **THEN** that block stores a block-local dictionary and decodes correctly, while other blocks keep the shared scope

#### Scenario: An old reader refuses shared-scope files
- **WHEN** a reader that does not understand the shared-dictionary feature opens a file declaring it as required
- **THEN** it refuses the file rather than misdecoding shared-scope code streams
