## ADDED Requirements

### Requirement: Falsification pruning evaluates columnar across a stripe's granules
Where a stripe's granule statistics are stored columnar (`hef-file-layout` — "Granule directory and authoritative marks"), the pruning expression of "Pruning as a falsification expression" SHALL be evaluable in one vectorized pass per stripe: each term SHALL bind to the stripe's decoded statistics arrays and produce a per-granule verdict bitmap over all of the stripe's granules at once, and the term bitmaps SHALL combine into the stripe's keep set, instead of re-evaluating the expression once per granule. A granule that does not carry the statistic a term needs SHALL contribute the always-keep result for that granule, preserving the expression's conservatism unchanged. The columnar evaluation SHALL produce exactly the keep/drop set of the per-granule term loop — adopting it changes no pruning decision, result, or visibility outcome — and the per-granule loop SHALL remain the correctness reference the columnar path is tested against.

#### Scenario: Columnar and per-granule evaluation agree
- **WHEN** the same filter is pruned against the same stripe through the columnar pass and through the per-granule term loop
- **THEN** both produce the identical keep/drop set for every granule

#### Scenario: A granule missing a statistic stays kept
- **WHEN** a term's statistic is absent for some granules of the stripe
- **THEN** those granules contribute keep for that term while granules carrying the statistic are still falsified by it

#### Scenario: One pass per stripe, not one per granule
- **WHEN** a stripe with many granules is pruned
- **THEN** each term is evaluated as one vectorized comparison over the stripe's statistics arrays, producing all granule verdicts in that pass

### Requirement: A cold open retains the bytes its tail read already fetched
When opening a file locates the footer through a sized tail read (one request when the manifest entry's recorded footer and tree lengths and file size are in hand), any data-area bytes that fall inside that tail read SHALL be retained and served to the plan instead of being fetched again, so a file compact enough to fit in the tail read is fully read by its open. This is an IO-count discipline only: footer validation, checksum verification, and refuse-on-required semantics are unchanged, retained bytes are still verified exactly as if they had been read on demand, and it composes with — never replaces — the layered metadata hierarchy and the requests-per-cold-open economics gate (`hef-benchmarks-and-acceptance-gates` — "Metadata-economics gates for cold opens and pruning").

#### Scenario: A compact file is fully read by its open
- **WHEN** a file's data area fits inside the sized tail read that fetched its footer
- **THEN** a subsequent scan of that file issues no further storage reads, serving the data from the retained tail bytes after normal verification

#### Scenario: Retained bytes are verified like fetched bytes
- **WHEN** the plan consumes data bytes retained from the open's tail read
- **THEN** they pass the same checksum verification they would have passed if read on demand, and a verification failure is handled identically
