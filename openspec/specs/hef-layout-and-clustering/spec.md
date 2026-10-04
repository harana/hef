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

### Requirement: Entity projection
Compaction MAY rewrite ingest files into a secondary entity projection whose rows sort by `entity_id_hash_low`, `entity_id_hash_high`, `epoch`, `sequence`, so one entity's history sits in one or a few contiguous granules. Ingest SHALL keep writing the primary projection unchanged, and a build SHALL accept entity order only when it is building the entity projection. The entity sort key SHALL NOT weaken sequence coverage: each entity-projection granule SHALL hold a single epoch, its recorded first and last `(epoch, sequence)` SHALL be the lowest and highest points it holds, and the file's sequence bounds SHALL be the lowest and highest points in the file. Each granule's per-block min/max of the entity hash columns SHALL serve as its entity min/max, so a reader skips every granule that holds none of an entity's rows from footer metadata alone. Each granule SHALL record a sortedness proof naming the entity order, never the primary order. The manifest SHALL publish the projection as an `EntityProjection` file over the same coverage as the files it was built from: a read alternative to them, never additional data.

#### Scenario: Short entity range reads at most two granules
- **WHEN** a 100-event range of one entity is read from the entity projection
- **THEN** at most two granules are read, and they hold all 100 events

#### Scenario: Entity min/max skips other granules
- **WHEN** a reader looks for one entity in the entity projection
- **THEN** the entity min/max skips every granule that holds none of its rows

#### Scenario: Primary build refuses entity order
- **WHEN** an ordinary build is handed rows in entity order
- **THEN** it refuses them, while the entity-projection build accepts the same rows

