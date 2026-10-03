## ADDED Requirements

### Requirement: Reader decoded-block caches are bounded
Every cache a long-lived reader keeps above the storage layer — decoded column blocks, inflated residual arenas, and similar per-file decode products — SHALL carry a byte-capacity budget with eviction, so a reader that lives across many queries over a large file holds a bounded working set rather than every block it ever decoded. Eviction SHALL be performance-only: an evicted block is re-fetched and re-decoded on next use through the normal verified path, with identical results. The bulk-scan read path SHALL NOT populate these caches — a full scan streams through without evicting the point-read working set. This budget is distinct from, and additive to, the page-granular object cache below the decoder: that tier bounds verified bytes, this tier bounds decoded blocks, and both SHALL be budgeted.

#### Scenario: A long-lived reader stays bounded
- **WHEN** a reader serves point lookups over a day-scale file across many queries, touching more granules than the budget holds decoded
- **THEN** its decoded-block memory stays within the byte budget, evicting least-recently-used blocks, and every lookup still answers correctly

#### Scenario: Eviction changes no result
- **WHEN** a block is evicted and a later query touches it again
- **THEN** the block is re-read and re-decoded through the same verification as its first use, and the query's rows are identical

#### Scenario: Bulk scans bypass the cache
- **WHEN** a bulk-egress or full-family scan streams a file
- **THEN** it does not populate the decoded-block cache, and a concurrent point-read working set is not evicted by the scan
