## ADDED Requirements

### Requirement: Encoded blocks never grow past the plain form
After the sampled winner encodes the full block — and after each recursive cascade level encodes its stream — the encoder SHALL compare the encoded size against the plain form of the same values and SHALL keep the encoded form only when it is strictly smaller; otherwise it SHALL store the plain form. A sample that misrepresents the block can therefore cost only the wasted trial encode, never a size regression: no encoded block and no kept cascade level SHALL be larger than its plain equivalent. The verification SHALL be a pure function of the encoded output, so selection determinism is unchanged (`hef-encodings-and-compression` — "Lifecycle-selected cascade strategies": same content at the same lifecycle stage produces the same bytes on any node), and the recorded pipeline SHALL describe whichever form was actually stored, so a reader decodes from the recorded description alone exactly as before.

#### Scenario: Unlucky sample falls back to plain
- **WHEN** the transform the sampler chose, applied to the full block, produces more bytes than the plain form of the same values
- **THEN** the block is stored in the plain form, the recorded pipeline names the plain form, and the block decodes correctly

#### Scenario: A cascade level that fails to shrink is dropped
- **WHEN** a deeper cascade level encodes its stream to at least as many bytes as the shallower form it would replace
- **THEN** that level is not kept and the stream stays at the shallower form, complementing the sampling gate in "Recursive cascade selection" with a full-data verification

#### Scenario: Verification preserves byte determinism
- **WHEN** two nodes encode the same block content at the same lifecycle stage
- **THEN** both reach the same accept-or-fallback decision and produce byte-identical encoded blocks

### Requirement: Stratified sampling through one shared statistics pass
Transform selection SHALL draw its sample as evenly spaced contiguous runs spanning the whole block — never a block prefix — at fixed, content-independent positions, with the total sampled value count aligned to the bit-packing quantum (a multiple of 1024 values) so packing padding does not distort size estimates. For each sample the encoder SHALL compute one shared statistics bundle in a single pass — at minimum min/max, run count, a distinct-value estimate or proven lower bound, and the most frequent value — and every candidate estimator SHALL read that bundle rather than re-walking the sample. A cheap disproof MAY precede exact counting: a distinct-count lower bound computed from value lengths and short prefixes MAY disqualify constant or dictionary candidates without a full deduplication of the sample. Fixed sample positions keep selection deterministic, so the scenario "Strategy selection is deterministic" under "Lifecycle-selected cascade strategies" holds unchanged.

#### Scenario: An unrepresentative prefix no longer mispicks the transform
- **WHEN** a block's leading values are sorted or constant but its remainder is not
- **THEN** the sampled runs span the whole block and the chosen transform reflects the whole block's shape, not the prefix's

#### Scenario: Candidate estimators share one statistics pass
- **WHEN** several candidate estimators need statistics over the same sample
- **THEN** the statistics bundle is computed once and every candidate reads it, with no candidate re-scanning the sample for a statistic the bundle already carries

#### Scenario: Sample positions are deterministic
- **WHEN** two nodes encode the same block content at the same lifecycle stage
- **THEN** both draw sample runs from identical positions and choose identical pipelines

### Requirement: Encoder telemetry pairs estimated with achieved ratios
For every encoded block the encoder SHALL emit, through the storage observability layer, the compression ratio the sampler estimated for the chosen pipeline and the ratio achieved on the full block, labelled by transform family and lifecycle strategy, together with a counter of acceptance-gate fallbacks (blocks stored plain because the sampled winner failed the "Encoded blocks never grow past the plain form" verification). Telemetry SHALL be aggregated per family — never per value — and SHALL NOT feed back into selection, which stays a deterministic function of block content and lifecycle stage.

#### Scenario: Estimator drift is visible per family
- **WHEN** a transform family's estimates diverge from its achieved ratios on a workload
- **THEN** the paired estimated/achieved series for that family makes the drift observable without re-running the encoder under instrumentation

#### Scenario: Acceptance-gate fallbacks are counted
- **WHEN** a block is stored plain because the sampled winner grew on the full data
- **THEN** the fallback counter for that family increments, and the block itself is unaffected

### Requirement: Encoder correctness rides law suites, an oracle fuzzer, and decision snapshots
The encoding module SHALL be covered by three complementary suites. First, an algebraic-law suite that every transform runs: a range decode equals the same slice of a whole-block decode, a point read equals the indexed element of a whole-block decode, and a compressed-form predicate selects the same rows as decode-then-filter — each over empty, single-value, repeated-value, and null-bearing edge cases — so correctness coverage scales with the transform set without maintained expected outputs. Second, an encoder oracle fuzz target: arbitrary column data is generated together with its expected plain-path results, encoded under both lifecycle strategies, and the encoded form's decode, range, point, and predicate paths are diffed against the expectation, with inputs the encoder legitimately refuses excluded from the corpus rather than treated as failures. Third, golden decision snapshots: a seeded corpus whose entries exceed one packing quantum (so sampling runs) SHALL have its full recorded pipeline selections and exact encoded byte counts snapshotted per lifecycle strategy, with each entry encoded twice to assert determinism, so any change to the cost model or sampler lands as a reviewable snapshot diff instead of a silent size regression.

#### Scenario: A new transform inherits the law suite
- **WHEN** a transform is added to the adaptive encoder's candidate set
- **THEN** it runs the shared algebraic-law suite unmodified, and a violated law fails the suite naming the transform and the law

#### Scenario: Fuzzing diffs the encoded paths against the plain oracle
- **WHEN** the encoder fuzz target generates a column whose encoded decode, range, point, or predicate result differs from the plain-path expectation computed at generation time
- **THEN** the fuzz run fails carrying the input, the divergent path, and the recorded pipeline

#### Scenario: A cost-model change is a reviewable diff
- **WHEN** a change alters transform selection or encoded sizes for the seeded corpus
- **THEN** the golden snapshot diff shows exactly which entries changed pipeline or byte count under which lifecycle strategy, and an entry encoding differently twice in one run fails the determinism assertion

## MODIFIED Requirements

### Requirement: Compressed-data string predicates
Dictionary string blocks SHALL assign codes in ascending sorted order of their
distinct values, so a code's numeric order matches its value's byte order. The
reader SHALL provide a string-predicate evaluator that answers common filters
directly from a block's compressed form, without rebuilding every row's text:

- For a dictionary block, it SHALL answer equality (`=`), inequality (`!=`), set
  membership (`IN`), and range (`<`, `<=`, `>`, `>=`, open or closed on either
  side) by resolving the predicate against the sorted dictionary and testing the
  stored codes.
- For an FSST block, it SHALL answer the equality class (`=`, `!=`, `IN`) by
  compressing the comparison value(s) with that block's symbol table and
  byte-comparing against the stored compressed values, decompressing nothing.
- For an FSST block, it SHALL answer prefix (`LIKE 'p%'`) and substring
  (`LIKE '%s%'`, `contains`) predicates by running a deterministic automaton,
  derived once per predicate from the comparison bytes and the block's symbol
  table, over the stored compressed code stream — one table transition per
  compressed byte, escapes included — decompressing nothing.

Because FSST codes are not order-preserving, range predicates SHALL NOT be
answered from FSST compressed bytes; the evaluator SHALL decline them so the
caller falls back to a full decode. The evaluator SHALL be an optimisation,
never a correctness dependency: a reader SHALL always be able to decode the block
and filter the values, and SHALL do so whenever the block's encoding or the
predicate is unsupported. The evaluator's result SHALL be exact — every selected
row truly satisfies the predicate — and SHALL equal a full decode-then-filter row
for row, with null rows never selected.

#### Scenario: Dictionary codes preserve value order
- **WHEN** the writer encodes a dictionary string block
- **THEN** the codes are assigned in ascending sorted order of the distinct
  values, so comparing codes orders rows the same way as comparing their strings

#### Scenario: Dictionary equality and range answered from codes
- **WHEN** an `=`, `IN`, or range filter runs on a dictionary-encoded column
- **THEN** the evaluator resolves the predicate to a set or interval of codes and
  selects rows by testing the stored codes, without rebuilding any row's string

#### Scenario: FSST equality answered from compressed bytes
- **WHEN** an `=` or `IN` filter runs on an FSST-encoded column
- **THEN** the evaluator compresses the comparison value(s) with the block's
  symbol table and byte-compares against the stored compressed values, without
  decompressing any value

#### Scenario: FSST prefix and substring answered by a compressed-domain automaton
- **WHEN** a prefix or substring filter runs on an FSST-encoded column
- **THEN** the evaluator builds the automaton from the pattern and the block's
  symbol table once, runs it over the stored code bytes with one transition per
  byte, and selects exactly the rows a decode-then-match would select, without
  decompressing any value

#### Scenario: FSST range falls back to decode
- **WHEN** a range filter targets an FSST-encoded column
- **THEN** the evaluator declines (FSST is not order-preserving) and the reader
  decodes the block and filters the values

#### Scenario: Compressed-data filter equals full decode
- **WHEN** the evaluator answers any predicate from the compressed form
- **THEN** the selected rows equal those of a full decode-then-filter, and null
  rows are never selected
