## ADDED Requirements

### Requirement: Index artifacts ride the manifest generation
A manifest generation SHALL be able to reference index artifacts — the out-of-file skip-index objects of the hef-query-metadata-and-indexes requirement "Heavy skip indexes publish as index artifacts outside the file" — alongside the deletion-vector generation records and footer mirrors it already carries. Each reference SHALL record the artifact's object identity, BLAKE3, kind, and a compact coverage summary, and the artifacts of a generation SHALL be published atomically with that generation through the same conditional-write discipline as every other generation object. Index-artifact objects SHALL be immutable, create-only, and tenant-qualified.

Adding, rebuilding, or dropping an index artifact SHALL be a manifest-only operation: it SHALL change no data file's manifest-entry identity or integrity fields, require no data-file rewrite, and never alter query results — only which acceleration the planner may use. An artifact no longer referenced by any readable snapshot SHALL be retired through the same sweeper discipline as other generation objects, honouring the safety window and the in-flight-query horizon.

#### Scenario: Artifacts publish atomically with the generation
- **WHEN** a generation is published with new index artifacts
- **THEN** the artifact references land in the same atomic manifest publication, and a reader of the previous generation sees none of them

#### Scenario: Dropping an artifact is manifest-only
- **WHEN** an index artifact is dropped
- **THEN** the next generation simply omits its reference; no data file is rewritten, no manifest entry's identity or integrity fields change, and queries fall back to the always-on tier

#### Scenario: Unreferenced artifacts are swept safely
- **WHEN** no readable snapshot references an index artifact any longer
- **THEN** it is retired through the sweeper with the same safety window and in-flight-query horizon as any other generation object, and no in-flight query loses an artifact it selected
