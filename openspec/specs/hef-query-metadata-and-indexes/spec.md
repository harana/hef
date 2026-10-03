## Purpose

Defines the metadata and indexes HEF uses to skip data it doesn't need to read:

- The layered metadata hierarchy: manifest summaries → header → footer → granule → marks → page.
- A uniform skip-index model (`SkipIndex<kind, granularity>`).
- Policy for probabilistic filters (Ribbon / split-block Bloom) and for exact bitmap indexes.

The concrete metadata hierarchy, SkipIndex, Bloom, and bitmap index formats are embedded in [index-formats.md](index-formats.md).
## Requirements
### Requirement: Layered metadata hierarchy to minimize reads
HEF SHALL provide layered metadata so a reader can avoid work in order: avoid file opens (manifest summaries), then footer work (header rejection), then stripe reads, then granule reads, then column/page reads, then payload reads. Manifest summaries SHALL carry only small per-file/per-projection summaries and pointers; large rollups SHALL live in HEF aggregate blocks or PreparedView outputs, not the manifest. Manifest-copied summaries SHALL NOT substitute for HEF footer validation.

#### Scenario: Prune without opening files
- **WHEN** a query's predicates fall outside a file's manifest summary coverage
- **THEN** the file is pruned without being opened

### Requirement: Manifest summaries mirror footer coverage flags
A manifest summary SHALL faithfully copy the footer's coverage flags — whether the file carries deletion vectors and whether it carries late events — so a planner reading only the summary sees the same coverage the footer declares. A file SHALL be recorded as carrying late events when it contains a row whose `occurred_at` is older than the `occurred_at` of a row ingested before it (an inversion against `(epoch, sequence)` order), and that fact SHALL travel as `has_late_events` in the manifest summary so a planner can tell, without opening the file, that its `occurred_at` coverage is not implied by ingest order.

#### Scenario: Late-event file is flagged in its manifest summary
- **WHEN** a file contains a row whose `occurred_at` predates a row ingested before it
- **THEN** its manifest summary has `has_late_events` set, and a file with no such row has it clear

### Requirement: Authoritative footer and granule metadata
The HEF footer SHALL be the authoritative file-local planning source, containing the directories (schema, column/stripe/granule/marks/projection/skip-index/bitmap/aggregate directories, deletion-vector directory, checksums, `file_blake3`). The granule directory SHALL be the authoritative pruning and parallel-scan unit between stripe and page. Page-level integrity SHALL be proven by BLAKE3, which is authoritative; CRC-64/NVME is not required at page level.

#### Scenario: Footer drives planning
- **WHEN** a surviving file is opened for a query
- **THEN** planning uses the footer directories as authoritative, not the manifest summaries

### Requirement: Uniform SkipIndex model
HEF SHALL generalize indexes into `SkipIndex<kind, granularity>` with explicit `exactness` (`exact` or `inexact_no_false_negative`). The required kinds SHALL be supported (`minmax` replacing zone maps, `sequence_range`, `time_range`, `ribbon_filter`, `split_block_bloom_filter`, `binary_fuse_filter`, `range_filter`, entity/account/opportunity hash filters, `context_locator`, `text_token`, `path_presence`). `range_filter` SkipIndexes SHALL answer range-emptiness with no false negatives (static HEF blocks use a Grafite-style succinct range filter; LiveOverlay MAY use a Memento-style dynamic range filter, replaced by the static form at rewrite). Bitmap indexes and embedding/vector blocks SHALL remain distinct structures (exact row-selection and approximate retrieval respectively), not SkipIndex kinds.

#### Scenario: Inexact skip index keeps a filter above
- **WHEN** a SkipIndex of exactness `inexact_no_false_negative` is used for pushdown
- **THEN** an exact predicate filter is retained above or inside the scan

#### Scenario: Point filter does not answer a range predicate
- **WHEN** a query needs range-emptiness pruning but only point-membership filters exist on the column
- **THEN** a `range_filter` is used (or min/max pruning), and the point-membership filter is not used to answer the range predicate

### Requirement: Workload-adaptive probabilistic filters
Ribbon, binary fuse, and split-block Bloom filters SHALL be workload-adaptive, not automatic for every column, and used for high-cardinality equality predicates and entity/account/actor/event/opportunity/customer lookups with bounded metadata cost. Binary fuse SHALL be preferred for immutable HEF point-membership blocks whose key set is fully known at build time; Ribbon SHALL be preferred when binary fuse construction is unavailable or it meets the target false-positive rate with fewer bytes; split-block Bloom SHALL be the required fallback. Classic unstructured Bloom bitsets SHALL NOT be a default. These filters MAY produce false positives but SHALL NOT produce false negatives, so pushdown using them is inexact unless combined with an exact check.

A filter that a file declares as `ribbon` in its feature directory SHALL be a genuine ribbon (bounded-band solved linear-system) representation with the space footprint and false-positive behavior that representation implies. An implementation that does not construct a real ribbon filter SHALL NOT declare the `ribbon` feature; it SHALL instead select binary fuse or the split-block Bloom fallback and declare that type, so a declared filter type always matches the bytes on disk. A build SHALL NOT satisfy a declared `ribbon` filter by silently substituting another representation (for example split-block Bloom), because that changes the space and false-positive characteristics a reader predicts from the declared type. Because a filter type is a declared file capability, changing which representations a file may declare SHALL follow HEF's feature-directory discipline: a reader that does not recognize a declared filter type SHALL refuse rather than guess.

#### Scenario: Low-cardinality field avoids probabilistic filter
- **WHEN** a field is low-cardinality and better served by a bitmap or value set
- **THEN** a Ribbon/Bloom filter is not used for it

#### Scenario: Declared filter type matches the on-disk representation
- **WHEN** a build cannot construct a genuine ribbon filter for a column it would otherwise place one on
- **THEN** it selects binary fuse or the split-block Bloom fallback and declares that type, and it does not declare the `ribbon` feature while storing a non-ribbon representation

### Requirement: Exact directly-intersectable bitmap indexes
Bitmap indexes SHALL be exact for low-cardinality dimensions (e.g. `source_id`, `event_type_id`, `entity_type_id`, `status`, `stage`, `currency`, `country`, `region`, boolean flags) and SHALL be compressed and directly intersectable without materializing row ids. If an encoding cannot intersect compressed bitmaps directly, the planner SHALL NOT use that bitmap path. The planner MAY use bitmap acceleration only when the file declares the bitmap feature and the bitmap block checksum verifies.

#### Scenario: Non-intersectable bitmap rejected
- **WHEN** a bitmap encoding cannot be intersected directly in compressed form
- **THEN** the planner does not use the bitmap acceleration path for it

### Requirement: Bounded string min/max in skip indexes
For string (variable-length byte) columns, a `minmax` SkipIndex MAY store *truncated* lower and upper bound values in place of the full minimum and maximum, so that the zone-map size per granule, stripe, or file stays bounded even when the column holds long, high-cardinality strings. Each such entry SHALL expose three fields, listed alphabetically as `is_truncated`, `max`, `min`: the stored lower bound `min`, the stored upper bound `max`, and a boolean `is_truncated` that is set when either bound was shortened. When `is_truncated` is clear the entry SHALL hold the exact minimum and maximum, behaving identically to an untruncated `minmax` index.

Truncation SHALL be conservatively correct for pruning, so a truncated bound never excludes a row that could match. The stored lower bound SHALL be truncated *downward* — it SHALL be byte-wise less than or equal to the true minimum value of the column over the covered rows — and the stored upper bound SHALL be truncated *upward* — it SHALL be byte-wise greater than or equal to the true maximum. A reader MAY therefore prune a granule, stripe, or file whenever a predicate's range lies entirely outside `[min, max]`, exactly as for an untruncated zone map, with no risk of a false negative.

When `is_truncated` is set the entry SHALL be treated as `inexact_no_false_negative` (see Requirement: "Uniform SkipIndex model"), because a surviving range may not actually contain a matching value once the dropped suffix bytes are considered. A query that uses a truncated bound to prune SHALL retain an exact predicate filter above or inside the scan and SHALL report the pushdown to both the static and the LiveOverlay source as inexact, so correctness is preserved (this is the exactness-reporting contract of the query-execution capability, Requirement: "Pushdown applied to both sources with exactness reporting"). A truncated `minmax` entry SHALL NOT be used as if it were exact.

The truncation length SHALL be configurable — a writer-chosen maximum stored-bound byte length per column or per index — and the chosen length SHALL be recorded with the index so a reader interprets the bounds without external context. Truncation SHALL be a deterministic function of the column values and the configured length, so any node that builds the index over the same committed rows produces byte-identical bounds and the same `is_truncated` value; the truncated index SHALL remain rebuildable acceleration state derived from the committed snapshot and SHALL never be the authoritative source of any value.

#### Scenario: Long-string zone map stays bounded
- **WHEN** the writer builds a `minmax` SkipIndex over a string column whose values are longer than the configured truncation length
- **THEN** the stored `min` and `max` are shortened to at most that length, `is_truncated` is set on the entry, and the entry's size is bounded by the configured length regardless of how long the underlying strings are

#### Scenario: Truncated bounds keep pruning sound
- **WHEN** a reader prunes a granule, stripe, or file using a truncated `minmax` entry because a predicate's range lies entirely outside `[min, max]`
- **THEN** no row that could satisfy the predicate is skipped, because the stored lower bound is byte-wise ≤ the true minimum and the stored upper bound is byte-wise ≥ the true maximum

#### Scenario: Truncated entry forces a residual filter
- **WHEN** a query uses a truncated `minmax` entry (`is_truncated` set) to prune and the predicate's range overlaps `[min, max]`
- **THEN** the entry is treated as `inexact_no_false_negative`, an exact predicate filter is kept above or inside the scan, and the pushdown is reported as inexact to both the static and the LiveOverlay source

#### Scenario: Untruncated entry stays exact
- **WHEN** every value of a string column fits within the configured truncation length, so neither bound is shortened
- **THEN** `is_truncated` is clear, the entry holds the exact minimum and maximum, and it behaves identically to an untruncated `minmax` zone map

#### Scenario: Same bounds on every node
- **WHEN** two nodes build the string `minmax` SkipIndex over the same committed rows with the same configured truncation length
- **THEN** they produce byte-identical `min`, `max`, and `is_truncated` values, preserving node independence

### Requirement: Pruning as a falsification expression
The planner SHALL express granule pruning as a single conservative pruning expression derived from the query filter, rather than as a separate code branch per statistic. From the query filter the planner SHALL build one expression — a "falsification" expression that asks "could any row in this granule match the filter?" — composed of terms contributed by whatever statistics the granule declares (for example min/max bounds, null counts, `ribbon_filter` / `split_block_bloom_filter` / `binary_fuse_filter` point membership, `range_filter` range-emptiness bounds, sketch bounds, `path_presence`, and `sequence_range` / `time_range` bounds). The planner SHALL evaluate this one expression uniformly over every granule to decide keep or drop.

The pruning expression SHALL be conservative: it MAY keep a granule that in fact contains no matching row, but it SHALL NEVER drop a granule that contains a matching row (no false negatives). When a granule declares no statistic able to falsify a given filter term, that term SHALL contribute the always-keep result for that granule, so absence of a statistic never drops data.

A new statistic SHALL plug into pruning by contributing a term to this expression, NOT by adding a new planner branch. Each contributing term SHALL carry the `exactness` of the `SkipIndex` it comes from (see Requirement: "Uniform SkipIndex model"); when any term used for a filter is `inexact_no_false_negative`, an exact residual filter SHALL be retained above or inside the scan for that filter, consistent with the query-execution capability, Requirement: "Pushdown applied to both sources with exactness reporting".

The keep/drop decision the falsification expression produces for a granule SHALL equal the decision the per-statistic pruning would produce for the same granule, statistics, and filter, so adopting it changes no query result, ordering, or visibility.

This expression SHALL compose with, and SHALL NOT replace or weaken, HEF's exact aggregate / cube / sketch shortcut tier: a query answerable from an aggregate block, a cube cell, or a sketch SHALL still take that more powerful path, and the pruning expression SHALL NOT reduce HEF pruning to min/max bounds alone.

#### Scenario: One expression spans every declared statistic
- **WHEN** a granule declares several statistics (for example min/max, a `binary_fuse_filter`, and a `range_filter`) and the query filter touches columns each statistic covers
- **THEN** the planner derives a single pruning expression whose terms come from those statistics and evaluates that one expression to decide keep or drop, with no per-statistic planner branch

#### Scenario: A granule with no matching row may be kept but one with a match is never dropped
- **WHEN** the pruning expression is evaluated against a granule whose statistics cannot prove the filter unsatisfiable
- **THEN** the granule is kept even if it holds no matching row, and a granule that does hold a matching row is never dropped, because each term is conservative and a missing statistic contributes the always-keep result

#### Scenario: A new statistic adds a term, not a branch
- **WHEN** a new statistic kind is introduced for a column
- **THEN** it participates in pruning by contributing a falsification term to the existing expression, and the planner needs no new per-statistic branch to use it

#### Scenario: An inexact term keeps the residual filter
- **WHEN** a term in the pruning expression comes from a `SkipIndex` whose exactness is `inexact_no_false_negative` (for example a Bloom or ribbon membership term)
- **THEN** the granule survives pruning only as a candidate and an exact filter for that predicate is retained above or inside the scan

#### Scenario: Aggregate and sketch shortcuts stay above pruning
- **WHEN** a query is answerable from an exact aggregate block, a cube cell, or a sketch
- **THEN** that shortcut tier still answers it and the falsification expression does not downgrade the query to min/max-only granule pruning

### Requirement: Page-granularity skip metadata for sub-granule pruning
The `minmax`, `sequence_range`, and `time_range` SkipIndex kinds SHALL be producible at `page` granularity — the granularity already admitted by the uniform `SkipIndex<kind, granularity>` model — and the writer SHALL emit them at `page` granularity for selected columns where page pruning has prune value above its metadata cost. The selection SHALL be gated by measured column heat rather than emitted for every column by default: the writer SHALL use the workload measurements the promotion machinery already collects to decide, per column, whether page-level min/max and range stats earn their footer bytes, and SHALL default to granule-level stats (leaving the per-page stats empty) for columns whose workload does not justify them. The pruning evaluator SHALL use page-granularity skip metadata to reject individual pages within a granule that the granule-level metadata could not reject, narrowing the read set to the surviving pages before any column bytes are fetched. Page-granularity skip metadata SHALL carry the same `exactness` marking as its coarser-grained counterparts and SHALL never produce a false negative. A column left at granule-level stats SHALL remain fully queryable, pruning at granule granularity with identical results.

#### Scenario: Reject pages inside a surviving granule
- **WHEN** a granule survives granule-level min/max pruning but only two of its pages can contain matching rows
- **THEN** page-granularity min/max skip metadata rejects the other pages and the scan fetches only the two surviving pages

#### Scenario: Cold column defaults to granule-level stats
- **WHEN** a column's workload measurements do not justify per-page stats
- **THEN** the writer emits no per-page min/max for that column, leaves its page-level stats empty, and the column still prunes correctly at granule granularity

#### Scenario: Hot column earns page-level stats
- **WHEN** the promotion machinery's measurements show a column is a hot page-pruning target whose per-page stats prune more than they cost
- **THEN** the writer emits page-granularity min/max, sequence_range, and time_range stats for that column and the evaluator uses them to reject non-matching pages

### Requirement: Constant and all-null flags enable metadata-only answers
Per-page and per-granule metadata SHALL carry an explicit `all_null` flag and an explicit `constant` marker (the single value when a block holds exactly one non-null value and no nulls), in addition to the existing `null_count`. When a predicate references a column whose covering page or granule is flagged `all_null` or `constant`, the pruning evaluator SHALL answer the predicate from the flag alone — without fetching or decoding the column block — and SHALL short-circuit to all-rows-pass or no-rows-pass where the flag determines the outcome. A constant or all-null projection output SHALL likewise be materializable from metadata without decoding the block. These flags SHALL be exact.

#### Scenario: Equality on a constant column answered from metadata
- **WHEN** a filter tests `currency = 'USD'` over a granule whose `currency` page is flagged `constant = 'USD'`
- **THEN** the evaluator returns all rows of that granule as matches without reading the `currency` block, and a projection of `currency` is filled from the constant without decode

#### Scenario: Predicate on an all-null column rejects without decode
- **WHEN** a filter requires `amount > 0` over a page flagged `all_null` for `amount`
- **THEN** the evaluator rejects the whole page from the flag without fetching the `amount` bytes

### Requirement: Text-token and path-presence indexes are produced and used
The `text_token` and `path_presence` SkipIndex kinds SHALL be concrete, not merely permitted. For schema-declared free-text and searchable string fields, the writer SHALL produce a `text_token` index — a token filter or compact inverted token list, plus an optional n-gram filter where substring matching is declared — at granule or page granularity; for shredded payload paths, the writer SHALL produce a `path_presence` index mapping a payload path to the granules/pages that contain it. The scan SHALL use these indexes to prune for token, contains, and path-presence predicates before reading the field's bytes. Both SHALL be inexact with no false negatives: a surviving granule/page SHALL still be confirmed by an exact predicate check over the materialized values, and a pruned granule/page SHALL be guaranteed not to match.

#### Scenario: Token query pruned then confirmed
- **WHEN** a query matches a token in a free-text field
- **THEN** the text-token index rejects granules that cannot contain the token, and each survivor is confirmed by an exact check over the materialized text

#### Scenario: Rare payload path restricts the read
- **WHEN** a query tests presence of a rare payload path
- **THEN** the path-presence index restricts the read to the granules/pages that contain the path

### Requirement: Text-token filter bytes live in the data area
The writer SHALL store the encoded bytes of `text_token` filters in the data area, inside the owning granule's stripe (so the stripe checksum covers them), and record only each filter's byte range in the footer, governed by a declared optional feature. A cold open, which fetches the footer alone, therefore never pays for filter bytes; a reader that declares the feature SHALL resolve a filter lazily by its recorded byte range on first demand, and the resolved filter SHALL prune exactly as the same filter stored inline would. The relocation SHALL be backward- and forward-compatible: a reader SHALL still resolve inline filters from files written before the relocation with identical pruning behaviour, and a reader that does not declare the feature SHALL simply not prune by token filter — which is always safe, because token pruning is inexact-no-false-negative acceleration, never a correctness dependency.

#### Scenario: Cold open fetches no filter bytes
- **WHEN** the writer builds a file with text-token filters
- **THEN** the filter bytes sit in the data area inside their granule's stripe, the footer records only their byte ranges under the declared optional feature, and no filter bytes ride the eagerly parsed footer

#### Scenario: Lazily resolved filter prunes identically
- **WHEN** a reader that declares the feature resolves a filter by its recorded byte range
- **THEN** the decoded filter answers token membership exactly as the same filter stored inline in the footer would, so pruning decisions are unchanged

#### Scenario: Inline filters from older files still resolve
- **WHEN** a reader opens a file whose filters are stored inline in the footer (written before the relocation)
- **THEN** it resolves them from the footer with identical pruning behaviour

### Requirement: Learned position index over sorted keys
For a column stored in sorted order under a projection (for example `sequence` or `occurred_at` in a sequence- or time-ordered projection), the writer MAY emit a `learned_position` SkipIndex: a compact piecewise model (for example a PGM- or RMI-style index) that maps a key to its approximate row position with a recorded maximum error bound. The reader SHALL use it to seek directly to the start row of a range scan and then confirm the boundary with a bounded local search within the error window, so a range lookup on the sorted key costs a model evaluation plus a small local scan rather than a bucketed probe or a full granule scan. `learned_position` SHALL be a registered SkipIndex kind under the uniform SkipIndex model, and every companion kind list SHALL include it so the enumerations do not disagree. The index SHALL be optional and feature-gated, SHALL record its error bound, and SHALL be used only where a sortedness proof establishes the key order it assumes; the positions it yields SHALL be exact after the local confirmation step.

#### Scenario: Range seek via the learned model
- **WHEN** a range scan selects `sequence BETWEEN a AND b` on a sequence-sorted projection carrying a `learned_position` index
- **THEN** the reader evaluates the model to land near row `a`, confirms the exact boundary by a local search within the error bound, and scans only the matching run

#### Scenario: No proof, no learned seek
- **WHEN** no sortedness proof covers the key the model assumes
- **THEN** the learned index is not used and the reader falls back to the bucketed range filter or scan

#### Scenario: Registered as a SkipIndex kind
- **WHEN** the SkipIndex kinds are enumerated
- **THEN** `learned_position` appears among them, consistent with this requirement

### Requirement: Rank/select over visibility and null bitmaps
Deletion (visibility) bitmaps and column null bitmaps SHALL support constant-time rank and select — for example via a succinct rank/select layer over the compressed bitmap — so a consumer can compute how many live (non-deleted) or non-null rows precede a given position and find the physical position of the k-th live or non-null row, without materializing the bitmap into row ids. The scan SHALL use rank/select to translate filtered logical positions into physical row offsets for late-materialization gather, and to apply the deletion-vector anti-join as bitmap operations over the row-id space rather than a row-by-row or hash anti-join. The rank/select structure SHALL be exact and SHALL be derivable from, and consistent with, the authoritative bitmap it indexes.

#### Scenario: Gather survivors without expanding the bitmap
- **WHEN** the scan must gather the surviving rows of a wide column after filtering and deletion
- **THEN** it uses rank/select to map each surviving logical position to a physical offset without expanding the bitmap into row ids

#### Scenario: Anti-join as bitmap operations
- **WHEN** deletion vectors are applied to a granule
- **THEN** the anti-join is computed as rank/select bitmap operations over the row-id space, identical in result to a row-by-row anti-join

