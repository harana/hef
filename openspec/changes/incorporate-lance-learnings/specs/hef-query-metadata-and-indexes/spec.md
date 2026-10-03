## ADDED Requirements

### Requirement: Heavy skip indexes publish as index artifacts outside the file
The always-on statistics tier — granule and page min/max, null counts, `sequence_range` / `time_range` bounds, exact counts, and the constant/all-null flags — SHALL stay in the file footer, because exact-scan fallback pruning depends on it when no other acceleration is present. Every heavier index — `ribbon_filter`, `split_block_bloom_filter`, `binary_fuse_filter`, `range_filter`, `path_presence`, `learned_position`, bitmap indexes, and embedding/vector retrieval structures — SHALL be publishable as an **index artifact**: an immutable, BLAKE3-verified object outside the data file, referenced from the manifest generation, so an index can be created, rebuilt, or dropped for an already-sealed file without rewriting a byte of it or changing its identity. The artifact form SHALL be the only way to add one of these indexes to a file after it is sealed; a writer MAY still emit an index inline at build time where this capability already places it (for example text-token filter bytes in the data area), and an inline index and an artifact of the same kind SHALL feed pruning identically.

An index artifact SHALL declare its kind, the column or payload path it indexes, the projection it was built over, its `exactness`, its false-positive rate where applicable, its granularity, and its **coverage**: the set of `(file_id, granule range)` extents it indexes together with the deletion-vector generation and schema fingerprint it was built against. Partial coverage SHALL be legal: the planner SHALL split a scan's granules into the covered set, pruned with the artifact's terms, and the uncovered set, pruned by the always-on footer tier alone, with results identical to a fully covered scan. An artifact SHALL contribute terms to the existing falsification expression exactly as an in-footer index of the same kind would — no new planner branch.

Artifacts SHALL be loaded on demand and progressively — opening a file or a generation SHALL NOT require fetching any artifact bytes, and a loaded artifact SHALL be readable a small directory first, then only the parts a query needs. Artifact construction SHALL be asynchronous and workload-driven, gated by the same measured column heat that gates page-level stats, never emitted for every column by default. Every artifact SHALL remain droppable, reconstructable acceleration with exact-scan fallback, per the droppable-acceleration invariant.

Staleness SHALL be enumerated, not assumed away. An artifact whose recorded deletion-vector generation has been superseded SHALL remain usable when its `exactness` is `inexact_no_false_negative`, because deleted rows are filtered downstream by the visible deletion vectors; an `exact` artifact (for example a bitmap index) whose recorded generation is superseded SHALL NOT be used until rebuilt. An artifact whose recorded schema fingerprint does not match the file's SHALL NOT be used. In every stale case the scan SHALL fall back to the always-on tier and remain correct.

#### Scenario: An index is added to a sealed file without touching it
- **WHEN** workload heat justifies a point-membership filter over a column of an already-published file
- **THEN** an index artifact is built asynchronously and referenced from the next manifest generation, and the data file's bytes, `file_id`, and `file_blake3` are unchanged

#### Scenario: Partial coverage splits the plan, not the result
- **WHEN** an artifact covers only some granules of the files a scan touches
- **THEN** covered granules are pruned with the artifact's falsification terms, uncovered granules are pruned by the footer's always-on tier, and the rows returned are identical to a scan with full coverage

#### Scenario: Dropping an artifact only costs performance
- **WHEN** an index artifact is dropped or its object is lost
- **THEN** every query still answers correctly from the exact-scan fallback and the always-on footer tier, and the artifact can be rebuilt from the committed snapshot

#### Scenario: Stale coverage is handled by exactness
- **WHEN** the deletion-vector generation advances past the generation an artifact's coverage records
- **THEN** an `inexact_no_false_negative` artifact keeps pruning (deleted rows are removed downstream), and an `exact` artifact is not used until it is rebuilt against the current generation

#### Scenario: Cold open never pays for artifacts
- **WHEN** a reader opens a file or a generation
- **THEN** no index-artifact bytes are fetched; an artifact loads on first demand, directory first, then only the parts the query needs
