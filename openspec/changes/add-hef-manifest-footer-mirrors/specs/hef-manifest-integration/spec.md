## ADDED Requirements

### Requirement: Footer mirror for opening a generation in one request

A manifest generation MAY publish one optional **footer-mirror object** that lets a query planner open every file in that generation with a single object read instead of one tail read per file. The mirror SHALL be manifest-native auxiliary metadata, permitted by the `hef-core-invariants` requirement "Barriers, deletion vectors, projections, and layout classes" ("auxiliary metadata SHALL be HEF-native or manifest-native"); it SHALL NOT introduce a new data-file shape and SHALL NOT change the closed set of HEF file shapes.

The mirror SHALL concatenate, keyed by `file_id`, the footer sections of the files published in the generation (optionally scoped to a tenant-day), compressed as one object. Each per-file footer section in the mirror SHALL carry its own authoritative BLAKE3 checksum and the `file_id` and generation of the file it mirrors, so a section can be validated on its own. BLAKE3 SHALL remain the sole integrity authority for every mirrored section, exactly as for the file footers themselves.

The mirror SHALL be droppable acceleration state: it is rebuildable from the files published in the generation, and its absence or loss SHALL cost only extra requests, never correctness. The HEF footer of each file SHALL remain authoritative; the mirror SHALL NEVER be a source of truth for a footer. When the mirror is present, a planner MAY read it once and open the generation's files from the mirrored sections; on any mismatch — a section whose BLAKE3 does not verify, a section whose recorded generation or `file_id` disagrees with the manifest entry, a missing section, or a mirror the manifest reports absent — the planner SHALL fall back to reading that file's footer directly from its own tail, which is the authoritative path and yields identical results.

The footer mirror SHALL be a feature-directory-gated capability: a reader that does not understand the mirror feature SHALL ignore the mirror and open files from their own footers (refuse to the authoritative path), never misread a mirror it cannot fully validate.

#### Scenario: One read opens the whole generation

- **WHEN** a planner opens a tenant-day generation that published a valid footer mirror
- **THEN** it fetches the single mirror object, validates each per-file section against its BLAKE3 and the manifest entry's `file_id` and generation, and opens every file in the generation from the mirrored footer sections without a per-file tail read

#### Scenario: Mismatch falls back to the authoritative footer

- **WHEN** a mirrored section fails BLAKE3 verification, or its recorded `file_id` or generation disagrees with the manifest entry, or the section is missing, or the manifest reports no mirror for the generation
- **THEN** the planner reads that file's footer directly from its own tail and proceeds with an identical result, and the mirror mismatch changes no query result

#### Scenario: Losing the mirror costs requests, not correctness

- **WHEN** a generation's footer-mirror object is dropped or was never published
- **THEN** every file in the generation is still openable from its own authoritative HEF footer, the mirror is rebuildable from the published files, and only the number of object requests differs
