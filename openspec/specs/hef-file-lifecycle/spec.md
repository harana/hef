## Purpose

Defines the lifecycle states an HEF file moves through:

- Each file's part-state as it is created, used, and retired.
- The rule that only files in the Active state may be picked up by new query snapshots.

The concrete file lifecycle detail are embedded in [lifecycle-detail.md](lifecycle-detail.md).
## Requirements
### Requirement: Defined part-state progression
HEF files SHALL progress through the part-states: `OpenTmp` (being written, not queryable), `Sealed` (valid footer/checksum, not yet visible), `Active` (manifest-referenced, queryable), `Outdated` (replaced by a newer generation but retained for prior snapshots), `DeleteOnDestroy` (unused by any readable snapshot, awaiting sweeper), and `Deleted` (removed after retention and safety window).

#### Scenario: Sealed file not yet visible
- **WHEN** a file is `Sealed` with a valid footer and checksum but not yet manifest-referenced
- **THEN** it is not visible to queries

### Requirement: Only Active files in new snapshots
Only `Active` files SHALL be selected for new public query snapshots. `Outdated` files SHALL remain readable only for in-flight snapshots that already selected them.

#### Scenario: Outdated file after replacement
- **WHEN** a file becomes `Outdated` after a rewrite
- **THEN** new query snapshots do not select it, while snapshots that already selected it may continue reading it

### Requirement: Retention, sweeper, and in-flight-query horizons are fixed before general availability
The sweeper cadence, the recovery/safety window before a `DeleteOnDestroy` file becomes `Deleted`, and the in-flight-query horizon SHALL be assigned concrete configured values before general availability; leaving any of them unset SHALL NOT be a valid GA configuration. Until they are finally tuned, conservative floors SHALL hold so no in-flight query loses the files it selected: a file SHALL NOT be swept from `DeleteOnDestroy` to `Deleted` until it has been unreferenced by every readable snapshot for at least the safety window; the safety window SHALL be at least the in-flight-query horizon; and the in-flight-query horizon SHALL be at least the configured maximum query duration. The sweeper SHALL run periodically rather than continuously and SHALL delete only files past their safety window, retention floor, and any legal hold. These values SHALL be recorded in the operator configuration before GA.

#### Scenario: Sweeper honors the safety window before final tuning
- **WHEN** a file enters `DeleteOnDestroy` before the final timing values are tuned
- **THEN** the conservative floor keeps it until it has been unreferenced by every readable snapshot for at least the in-flight-query horizon, and no in-flight query loses a file it selected

#### Scenario: GA requires concrete values
- **WHEN** a release is prepared for general availability
- **THEN** the sweeper cadence, safety window, and in-flight-query horizon carry concrete configured values, and an unset value blocks the GA configuration

### Requirement: Compaction retires its inputs and the sweeper deletes them
A compaction SHALL publish its output and retire its inputs in one manifest generation: the output enters `Active`, every input moves `Active → Outdated`, and the generation records for each input the generation and time it was retired. The inputs SHALL be `Active` files of one tenant whose coverage joins into one contiguous range, which the output covers exactly. The sweeper SHALL be driven by the application, periodically, with the oldest generation any running query still reads: it SHALL move an `Outdated` file to `DeleteOnDestroy` only when every live snapshot was opened at or after the generation that outdated it and the in-flight-query horizon has passed since, and SHALL delete a `DeleteOnDestroy` file's object through the application's object store, then drop its entry (`Deleted`), only after the safety window (never shorter than the in-flight-query horizon). Objects SHALL be deleted before the catalogue forgets them, and an object still named by an `Active` or `Outdated` entry SHALL NOT be deleted. The application drives `plan_compaction_cycle` and `should_roll`; HEF runs none of these on its own.

#### Scenario: Compaction publishes one generation
- **WHEN** a compaction of two adjacent Active files is published
- **THEN** one new generation holds the output as Active and both inputs as Outdated, and new snapshots select only the output

#### Scenario: Sweeper spares files a live snapshot may read
- **WHEN** the sweeper runs while a query still reads a generation in which the replaced files were Active
- **THEN** it moves nothing and deletes nothing, however long that query has run

