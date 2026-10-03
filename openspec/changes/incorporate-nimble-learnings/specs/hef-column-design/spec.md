## ADDED Requirements

### Requirement: Sparse shredded columns below the promotion threshold
Between dense promotion and the residual arena, HEF SHALL support a third storage tier for payload paths: a **sparse shredded column** — per granule, a presence bitmap in the format's compressed bitmap form plus a dense value block holding values for the present rows only, gathered through the rank/select machinery. A granule in which the path is absent SHALL cost zero value bytes and SHALL be prunable from its presence entry alone, so reading a sparse-but-hot field costs work proportional to the rows that *have* the value, not the rows the query selects — where the residual arena costs a probe per selected row regardless.

Tier selection SHALL be writer policy driven by observed per-path presence and query demand — the same statistics that drive dense promotion — never an operator knob: paths hot enough and dense enough promote to dense columns as today, paths hot but sparse MAY store sparse, and everything else stays residual. Governance SHALL stay closed: the sparse key set SHALL be declared per file in a footer section keyed by the schema fingerprint, exactly as the presence map declares promoted columns, and the writer SHALL bound the number of sparse keys per file by a pinned budget so footer metadata cannot grow without limit — a path past the budget simply stays residual.

Shredding semantics SHALL be preserved: for each row, a sparse path's value SHALL live in exactly one of the sparse column or the residual value, and a row's full payload SHALL remain the deterministic merge of typed columns and residual. Because sparse-stored values are removed from the residual, the sparse tier SHALL be governed by a **required** feature bit: a reader that does not understand it SHALL refuse the file rather than fall through to a residual read that silently misses values. Sparse columns SHALL carry the same data-class/authorization labels, adaptive encoding, and random-access obligations as dense shredded columns.

#### Scenario: A sparse-but-hot field escapes residual decode
- **WHEN** a payload path present on a small fraction of rows is filtered on daily and the writer's statistics see it
- **THEN** it is stored as a sparse shredded column, a query over it decodes the dense value block and presence bitmap for granules that contain it, and no residual bytes are decoded for the rows that lack it

#### Scenario: An absent granule costs zero bytes
- **WHEN** a query touches a granule whose presence entry records the sparse path absent
- **THEN** the granule is pruned for that path from the presence entry alone, with no value bytes stored or fetched for it

#### Scenario: The key budget bounds the footer
- **WHEN** more paths qualify for sparse storage than the pinned per-file key budget allows
- **THEN** the writer stores sparse columns only up to the budget and leaves the remaining paths in the residual arena, and the file remains fully correct

#### Scenario: An old reader refuses rather than missing values
- **WHEN** a reader that predates the sparse tier opens a file declaring its required feature bit
- **THEN** it refuses the file, because reading the residual as if the path lived there would silently return missing values

#### Scenario: The payload merge is unchanged
- **WHEN** a row's full payload is reconstructed from a file with sparse columns
- **THEN** each sparse path's value comes from exactly one place — the sparse column where present, the residual otherwise — and the merged payload is identical to the same rows stored without the sparse tier
