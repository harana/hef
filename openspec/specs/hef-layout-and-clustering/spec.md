## Purpose

Defines how HEF sorts and clusters data on disk to match query patterns:

- The default primary ordering and the allowed projection types (alternate orderings of the same data).
- Rules that stop projections from double-counting.
- A preference for explicit projections over space-filling curves, and incremental (liquid-style) reclustering at rewrite.

The concrete sorting, clustering, projection, and workload-driven layout detail are embedded in [layout-detail.md](layout-detail.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-layout-and-clustering/spec.md).
## Requirements
### Requirement: Default primary projection
The default primary projection SHALL sort by internal `tenant_id`, `epoch`, `sequence`, and MAY cluster by `occurred_at` bucket only when it does not break sequence-range coverage metadata.

#### Scenario: Clustering preserves sequence coverage
- **WHEN** clustering the primary projection by `occurred_at` bucket would break sequence-range coverage metadata
- **THEN** that clustering is not applied and the sequence-ordered primary projection is kept

