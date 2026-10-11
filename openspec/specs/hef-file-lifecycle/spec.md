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

### Requirement: Inbox files retire once every recipient acknowledges them
An `Active` inbox file SHALL become retireable once every recipient in its recipient directory has an acknowledgement watermark at or after its newest point in the file; a file without a directory SHALL NOT be retired this way. Retirement SHALL reuse the compaction lifecycle: the application checks the files, and one manifest generation moves each fully acknowledged file `Active -> Outdated` with a retirement record (generation and time read through the clock), after which the sweeper moves and deletes it exactly as it does a compaction input. Retiring a file that is no longer `Active` SHALL be a no-op, and naming a file that is not an inbox file SHALL be refused.

#### Scenario: Retired after all recipients acknowledge
- **WHEN** some but not all recipients of an inbox file have acknowledged their newest row in it
- **THEN** the file is not fully acknowledged, and once the last recipient acknowledges it is, retirement moves it to Outdated, and the sweeper later deletes its object

#### Scenario: Only inbox files retire by acknowledgement
- **WHEN** retirement by acknowledgement names a file whose profile is not `inbox`
- **THEN** the publication is refused and no generation is written

### Requirement: A snapshot file replaces the previous epoch's file in one generation
A `snapshot` profile file SHALL be published into a catalogue that holds only that snapshot's files, never event files, because its epoch and sequences number its rows rather than journal positions. Publishing SHALL use the same conditional manifest advance as compaction: one new generation in which the new file is `Active` and every previously `Active` file of the catalogue moves to `Outdated` with its retirement recorded, so the sweeper deletes it later. The new file SHALL hold a later epoch than every `Active` file; a lost race SHALL rebase and re-check, so a rival's equal or later epoch refuses the publication. Republishing the file already `Active` SHALL change nothing.

#### Scenario: Next epoch replaces the last
- **WHEN** a snapshot of epoch 2 is published over the Active snapshot of epoch 1
- **THEN** one new generation holds epoch 2 as the only Active file and epoch 1 as Outdated

#### Scenario: Stale epoch refused
- **WHEN** a snapshot of an epoch not later than the Active one is published
- **THEN** the publication fails and the head generation is unchanged

### Requirement: Graph files compact per subject in size tiers
Graph files SHALL be compacted per subject, never by the event-file planner. A run is a stretch of one subject's `Active` graph files below 64 MiB whose events follow on with no gap; a file of 64 MiB or more, a gap, or another subject ends it. A run of two or more files SHALL merge when it holds more than 8 files or adds up to less than 8 MiB, cut front to back into merges that each stop before their output passes 256 MiB. The merged file's graph SHALL be rebuilt from its inputs: every parent kept in order, reverse edges derived again, an edge from a later input into an earlier one becoming an in-file edge, an edge from a later file outside the merge becoming an inbound edge of the merged file, unresolved parents, chain-cover labels and links carried over, and the event-id index rebuilt from the rows. The merge publishes through the ordinary compaction publication.

#### Scenario: Compaction preserves every edge and id
- **WHEN** three sealed files of a subject are merged while a fourth, later file names parents in them
- **THEN** every event's parents and children, and every event id lookup, read back from the merged file as they did across the four files

#### Scenario: Size-tiered trigger
- **WHEN** a subject has three files adding up to under 8 MiB, three adding up to 8 MiB or more, or nine of any size below 64 MiB
- **THEN** the first and last merge and the second waits
