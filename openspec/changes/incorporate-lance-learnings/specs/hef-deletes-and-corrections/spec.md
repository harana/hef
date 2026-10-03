## ADDED Requirements

### Requirement: Deletion vectors choose their wire encoding by density
When HEF-native deletion-vector blocks are written, the wire encoding SHALL be selected by cardinality between exactly two pinned forms: a sorted `u32` ordinal array when the vector's deleted count is below a pinned threshold, and the positional Roaring form at or above it. The chosen form SHALL be recorded in the deletion-vector reference's `encoding` field so a reader decodes without guessing, and both forms SHALL decode to the identical set of deleted row positions in the primary-rowset ordinal domain — the encoding changes bytes, never semantics, and intersection with visibility bitmaps SHALL produce identical results under either form. Selection SHALL be deterministic: the same deleted set SHALL always produce the same encoding and the same bytes on any node. The threshold SHALL be pinned from the measured correction/erasure workload distribution before the native-block feature ships, and SHALL NOT be an operator key. This requirement composes with — and does not alter — the deletion-vector semantics, identity fields, and `DeletionAggregateDelta` obligations of "Deletes via immutable deletion vectors".

#### Scenario: A sparse correction vector stores as ordinals
- **WHEN** a correction deletes a handful of rows in a file
- **THEN** the published deletion vector is a sorted `u32` ordinal array, its reference's `encoding` field says so, and it is smaller than any bitmap form

#### Scenario: A dense vector stores as Roaring
- **WHEN** a deletion vector's deleted count is at or above the pinned threshold
- **THEN** it is stored in the positional Roaring form and its reference's `encoding` field says so

#### Scenario: Both encodings apply identically
- **WHEN** the same deleted set is decoded from the ordinal-array form and from the Roaring form
- **THEN** the visible rows, aggregate-delta application, and anti-join results are identical
