## ADDED Requirements

### Requirement: Presence and null bitmaps are encoded side streams
A block's presence or null bitmap SHALL be an encoded side stream chosen by the same sampling discipline as any other side stream, not a fixed raw one-bit-per-row prefix outside the cascade. The candidate forms SHALL be: `all_present` — zero stored bytes when nothing is null or absent (the overwhelmingly common case, which today pays a kilobyte of zero bytes per 8192-row nullable block); `all_absent` — zero stored bytes with the block body elided, satisfying the all-null marker; `roaring` — run containers for clustered nulls and array containers for sparse ones, the same form the reader already builds in memory; and `raw` — the current bitmap, kept as the fallback and the deterministic reference, winning whenever nulls are dense and incompressible. The chosen form SHALL be recorded with the block, selection SHALL be deterministic, and the rank/select obligations over null bitmaps SHALL be served from the compressed form without materializing the raw bitmap. Because the block frame changes, this SHALL be governed by a required feature bit (`compressed_presence`), with the writer permitted to dual-emit during migration per the `columnar_marks` precedent.

#### Scenario: An all-valid column stops paying bitmap bytes
- **WHEN** a nullable or presence-gated block has no null and no absent row
- **THEN** its presence side stream is `all_present` at zero stored bytes, and the block decodes identically to one carrying a raw all-set bitmap

#### Scenario: Sparse nulls compress
- **WHEN** a block's nulls are few or clustered
- **THEN** the sampled winner is a roaring form smaller than the raw bitmap, and rank/select answers over it match the raw form exactly

#### Scenario: Dense noise keeps the raw form
- **WHEN** a block's null pattern is dense and incompressible
- **THEN** sampling keeps the `raw` form, so the compressed forms never cost more than today's bitmap

#### Scenario: An old reader refuses rather than misframing blocks
- **WHEN** a reader that does not understand `compressed_presence` opens a file declaring it as required
- **THEN** it refuses the file, because the block frame it would parse no longer starts with a raw bitmap

### Requirement: Constant and all-null blocks store no data bytes
When a block's statistics prove it constant (min equals max with zero nulls) or all-null (null count equals row count), the writer SHALL elide the block body entirely: the block's mark SHALL record a zero-length extent, the metadata SHALL carry the constant value for every statistics-bearing column type — not integers only — and the reader SHALL materialize projections and answer predicates from metadata alone, per the existing constant/all-null flags requirement, fetching and decoding nothing. The flags SHALL be derived from the per-page and per-granule statistics the footer already carries — deriving, not adding, is the rule: no new statistic is required to know a block is constant. Because a zero-length block body is not ignorable by a reader that expects bytes, elision SHALL be governed by the feature directory as a required capability, and a file without the feature SHALL keep writing full blocks with identical query results.

#### Scenario: A constant column costs only footer bytes
- **WHEN** a granule's `currency` block holds one non-null value for every row
- **THEN** the block body is elided, its mark records a zero-length extent, and both `currency = 'USD'` filters and `currency` projections are answered from the recorded constant without any data-area read

#### Scenario: Constant strings are representable
- **WHEN** the constant block's column is a string, decimal, or timestamp
- **THEN** the recorded constant carries that type's value exactly, and materialization from metadata is byte-identical to decoding an un-elided block

#### Scenario: The flags cost no new statistics
- **WHEN** the writer decides whether a block is constant or all-null
- **THEN** the decision reads only the min/max and null-count statistics the footer already stores for that block

### Requirement: Decode-cost preference is a deterministic score
Where encoding selection weighs "the fastest valid pipeline", the decode-speed preference SHALL be expressed as fixed per-family penalty multipliers applied to each candidate's estimated encoded size, with a distinct pinned multiplier table per lifecycle strategy — `DecodeOptimized` penalizing decode-heavy families hard enough that they must win by a clear margin, `SizeOptimized` running near-neutral — so the cheap-to-decode family wins ties by construction. Wall-clock or hardware timing SHALL NOT be an input to selection: measured decode speed varies by node and would break the byte-identical-encode determinism, so the preference lives in the pinned score, never in a benchmark taken at encode time. The multiplier tables SHALL be pinned constants of the specification's companion material, identical on every node.

#### Scenario: A heavy family must win by a margin when fresh
- **WHEN** a decode-heavy candidate is only marginally smaller than a lighter one under `DecodeOptimized`
- **THEN** the penalty multiplier makes the lighter family's score win, and the same comparison under `SizeOptimized` may choose the heavier family

#### Scenario: No timing ever enters selection
- **WHEN** two nodes with different hardware encode the same block at the same lifecycle stage
- **THEN** both compute identical candidate scores from estimated sizes and the pinned multipliers, and produce byte-identical blocks

### Requirement: FSST over dictionary values is a sampled cascade level
For dictionary string blocks, the encoder SHALL sample FSST compression of the dictionary's value stream as an inner cascade level, so per block the choice is between a plain dictionary, a dictionary whose distinct values are FSST-compressed, and FSST alone — covering the middle ground where codes win on the code stream but the alphabet itself is bulky. Codes SHALL keep their ascending sorted-order assignment and FastLanes packing, so compressed-form equality and range predicates on codes are untouched by the inner level; only dictionary-value materialization pays the FSST decode. Size budgeting for the inner level SHALL assume the FSST worst case of two output bytes per input byte, and front coding, where implemented for sorted alphabets, SHALL compete as a candidate at the same slot. The choice SHALL follow the recursive-cascade sampling discipline — kept only when sampling proves it wins — and be recorded like any cascade level.

#### Scenario: The middle ground stops losing code pushdown
- **WHEN** a column has moderate cardinality with compressible values, where today FSST-only wins and code-order predicates are lost
- **THEN** sampling can choose dictionary-with-FSST-values, keeping sorted-code equality and range pushdown while the alphabet shrinks

#### Scenario: Code predicates never decode the alphabet
- **WHEN** an equality or range predicate runs over a dictionary block whose values are FSST-compressed
- **THEN** the predicate resolves and tests codes exactly as on a plain dictionary block, decoding no FSST bytes

#### Scenario: Worst-case expansion is budgeted
- **WHEN** the encoder sizes the FSST inner level for a dictionary's value stream
- **THEN** it budgets two output bytes per input byte before accepting the level, so a pathological alphabet cannot overflow its buffers or size limits
