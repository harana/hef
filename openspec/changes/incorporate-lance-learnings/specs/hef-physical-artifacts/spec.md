## ADDED Requirements

### Requirement: External payload references carry a pinned descriptor
The external-payload escape (`PAYLOAD_FLAG_EXTERNAL_REF`) SHALL have a pinned reference shape before any writer uses it: a descriptor with the fields, alphabetically, `blake3` (the hash of exactly the referenced bytes), `kind`, `position` (byte offset within the target), `size`, and `uri` (present only for the external-target kind). The `kind` domain SHALL be closed and pinned: `dedicated_object` (the payload is its own object), `external_uri` (a range inside an existing container, addressed by `uri` + `position` + `size`, so an oversized payload can be referenced in place without copying), and `packed_sidecar` (a range inside a shared sidecar object). A reader SHALL verify the fetched bytes against the descriptor's `blake3` before use — an external payload never weakens the integrity chain — and SHALL refuse a descriptor whose `kind` it does not recognize rather than guess. Pinning the shape now is deliberate: no placement machinery ships with it, and a writer SHALL NOT emit external references until a placement kind's machinery lands, so the descriptor is stable before the first byte depends on it.

#### Scenario: An oversized payload is referenced in place
- **WHEN** a payload beyond the journal frame cap already lives in a connector's raw archive object
- **THEN** its row carries an `external_uri` descriptor with `uri`, `position`, `size`, and the `blake3` of that range, and no payload bytes are copied into the arena

#### Scenario: External bytes verify before use
- **WHEN** a reader resolves an external payload reference
- **THEN** it fetches the descriptor's range and admits the bytes only after they match the descriptor's `blake3`

#### Scenario: An unknown placement kind refuses
- **WHEN** a reader meets an external-payload descriptor whose `kind` it does not recognize
- **THEN** it refuses the reference rather than dereferencing it by guess, and surfaces the row through the defined error path
