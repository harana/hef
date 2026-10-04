## Purpose

Defines the logical shape of an event as stored by the HEF/HEJ engine, independent of how the bytes are encoded:

- The fixed event envelope, how time and duration are recorded, optional promoted columns, and how the raw payload is handled.
- The analytical column families and the boundary that controls what may appear in public output.
- Logical field names are stable and unit-neutral; the physical encoding is an implementation detail.

The concrete field schemas, encodings, and column-family layouts are embedded in [event-model-schemas.md](event-model-schemas.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-logical-event-model/spec.md).
## Requirements
### Requirement: Fixed event envelope
Every event SHALL carry the fixed logical envelope fields (including `event_id`, `tenant_id`, `epoch`, `sequence`, `stream_id`, `stream_sequence`, `occurred_at`, `ingested_at`, `source_id`, `event_type_id`, `entity_type_id`, `entity_id_hash`, `payload_ref`, `flags`, and `schema_version`). Stable logical field names SHALL be unit-neutral; physical encodings (nanosecond timestamps, compact integers, dictionaries, hashes) are not part of the logical contract. The persisted widths of the membership-hash encodings — the 128-bit `entity_id_hash` and the 64-bit low-half actor, account, and trace hashes — SHALL be physical encodings governed by `hef-physical-artifacts` (see its Requirement: "Membership-hash width and collision semantics"), not part of the logical contract, so a logical query names the identifier and never a hash width.

#### Scenario: Event written with complete envelope
- **WHEN** an event is ingested
- **THEN** all required envelope fields are populated and the stable logical names are preserved regardless of the physical encoding chosen

#### Scenario: Internal sequence_key is a physical alias
- **WHEN** an implementation exposes a `sequence_key` for sorting and pruning
- **THEN** it SHALL be defined as an internal physical alias of `(epoch, sequence)` and SHALL NOT be exposed by public APIs

### Requirement: Unit-neutral time and duration
Logical timestamp and duration fields SHALL use the `TimestampValue` and `DurationValue` types. Physical storage MAY encode them as integer deltas with nanosecond precision and record precision/epoch base in block metadata, but stable schema names SHALL remain unit-neutral.

#### Scenario: Timestamp stored as nanosecond delta
- **WHEN** `occurred_at` is physically stored as a signed integer delta with nanosecond precision
- **THEN** the logical field remains `occurred_at: TimestampValue` and the precision/epoch base are recorded only in physical block metadata

### Requirement: Automatic workload-aware column promotion
The writer SHALL be able to promote common event attributes into typed columns automatically based on workload signals (filters, group-by, order-by, join keys, rules, prepared views, revenue metric definitions, dashboard cards, tool queries, frequent payload-field scans). The format SHALL NOT require users to manually configure every promoted field. Promotion SHALL preserve data-class labels and field-level authorization.

#### Scenario: Frequently filtered field is promoted
- **WHEN** a payload field is repeatedly used in filters or group-by expressions
- **THEN** the writer promotes it to a typed column while preserving its data-class labels and field-level authorization

### Requirement: Single canonical payload format with ingest transcoding
There SHALL be exactly one payload format, `harana_variant_v1` — a Parquet-Variant-based binary value encoding whose field ids resolve against an external shared dictionary and which supports offset-based single-path navigation that never touches sibling fields. Ingest routes SHALL transcode every accepted source body (JSON, Protobuf, Avro, MessagePack, raw bytes) into `harana_variant_v1` before HEJ append; source bytes SHALL NOT be stored unless the stream opts into the raw-payload column, and the canonical `harana_variant_v1` value SHALL be the stored payload. A stream that must return signed bodies byte-exact MAY opt into an optional `raw_payload` column holding each event's original UTF-8 body beside the canonical payload, compressed by the ordinary string-column encodings; shredding, promotion, and free-text extraction SHALL keep working off the canonical payload, the column SHALL be internal-only, and deletion vectors SHALL apply to it as to the payload while any field-level redaction of a row SHALL withhold that row's whole raw payload. QueryEngine SHALL NOT read payload values unless the query needs paths unavailable as authorized promoted or shredded payload columns, and payload reads SHALL be late-materialized: prune via metadata, read envelope/promoted/shredded columns, apply authorization/filters/deletion-vectors/corrections, then read residual variant values only for final matching rows, extract only the requested paths, then redact per the caller's authorization.

#### Scenario: Source body transcoded, source bytes discarded
- **WHEN** a Protobuf or JSON event body is ingested
- **THEN** it is transcoded into a canonical `harana_variant_v1` value before HEJ append, the original source bytes are not stored, and any source-format lineage is recorded only as internal `source_schema_ref`

#### Scenario: Opted-in raw payload returns byte-exact
- **WHEN** a stream that opted into the raw-payload column writes canonical-JSON Matrix events, including integers
  near 2^53 and unicode escapes, and a reader asks for one event's raw payload from the sealed file
- **THEN** the bytes come back identical to the bytes that arrived, while the canonical payload still reads as the
  transcoded variant

#### Scenario: Query satisfiable from promoted columns
- **WHEN** a query references only authorized promoted columns and envelope fields
- **THEN** the `QueryEngine` answers without reading any payload values

#### Scenario: Payload read only for final matching rows
- **WHEN** a query requires an unshredded payload path
- **THEN** the `QueryEngine` prunes and filters first and reads residual variant values only for the final matching rows, extracting only the requested paths and redacting fields according to the caller's authorization

### Requirement: Analytical column families
HEF SHALL support the analytical column families beyond the minimal envelope (source/type/entity, safe-retry/ingest-mode, classification labels, cluster columns, internal embeddings, revenue-anomaly, driver/cause, revenue metric, prepared-view lineage, and chat/investigation context). Internal-only column families SHALL be blocked from public output unless an owning service defines a public-safe derived representation.

#### Scenario: Internal embedding column requested publicly
- **WHEN** a public caller requests an internal-only column family such as embeddings
- **THEN** the column is blocked from public output unless an owning service has defined a public-safe derived representation

### Requirement: Column families split by temporal computability
Each analytical column family SHALL be classified by temporal computability, not subject area. Tier A families (envelope, source/type/entity, safe-retry/ingest-mode, classification labels, payload variant, workload-promoted payload-field columns, per-event-deterministic derivations such as embeddings, revenue metric, and chat/investigation context) SHALL be stored HEF-native in the base file because they are computable at seal time from the single event. Tier B families (revenue-anomaly, driver/cause, and cluster columns — cross-event, asynchronous, revisable model outputs) SHALL NOT be stored in the sealed base file; they SHALL live in a sibling derived-columns file in HEF format, row-aligned ordinal-for-ordinal to the base, carrying per-column lineage (producer, model version, input coverage, readiness, settle horizon). Revision of a Tier B value SHALL republish a new sibling-file generation; the immutable base file SHALL never be rewritten for it.

#### Scenario: Anomaly score arrives days after seal
- **WHEN** an anomaly model produces a score for an event whose base HEF sealed days earlier
- **THEN** the score is written to a sibling derived-columns file row-aligned to the base, and the base file is not modified

#### Scenario: Per-event embedding computed at seal
- **WHEN** a deterministic per-event derivation such as an embedding is computable from the single event at publication
- **THEN** it is stored Tier A in the base file

### Requirement: Public-output authorization boundary
The QueryEngine provider and owner services SHALL enforce public-output safety below API serialization by authorizing requested columns, dropping blocked internal columns, rejecting raw internal identity fields for public callers, mapping internal event identity to an opaque cursor only at the API boundary, late-materializing raw payload only for authorized callers, and redacting payload fields before return. The fields blocked-by-default (raw `tenant_id`, `epoch`, `sequence`, `sequence_key`, object-store/local-cache paths, row offsets, `payload_ref`, raw payload for unauthorized callers, embedding/internal analytical columns, and raw data-class labels) SHALL NOT be returned on ordinary event/product APIs.

#### Scenario: Internal identity requested by public caller
- **WHEN** a public caller requests raw `tenant_id`, `epoch`, `sequence`, `payload_ref`, or a storage path
- **THEN** the provider blocks the field and, where identity is needed, returns only an opaque cursor mapped at the API boundary

### Requirement: Event relationship references column family
The logical event model SHALL define an optional Tier A relationship-references
column family carrying the references an event declares about other events.
Each reference SHALL carry `relationship_kind` (a registry-controlled tag;
initially `parent`, `root`, `link`, and `related`, extended only by registry;
the registry now also holds `prev` and `auth` for a federated protocol's
`prev_events` and `auth_events`),
`target_ref` (the declared target identifier bytes), and `target_id_space` (a
registry-controlled tag naming the identifier space of `target_ref`; initially
`event_id` for envelope identity and `protocol_event_id` for a signed
protocol's own identifier space, plus `external_id` for a 1-to-255-byte
external protocol id matching the event external-id column). Only `parent` and
`root` are limited to one per event; every other kind repeats freely. References SHALL be declared by the
referencing event at ingest and SHALL be immutable with it; a sealed target
SHALL never be rewritten to record later referrers. The family SHALL be
absent — materializing no columns — for streams that declare no relationships;
its presence SHALL NOT alter the fixed envelope, and `sequence` assignment,
pruning, and ordering SHALL remain governed by the envelope alone. Relationship
columns SHALL participate in the ordinary column machinery — physical
encodings, workload-aware promotion, data-class labels, and field-level
authorization — and equality predicates on `(relationship_kind, target_ref)`
SHALL be pushdown-eligible and served by the standard metadata and filter
machinery, so reverse lookups such as "events whose `parent` reference names X"
prune files and granules without any graph engine, new index kind, or new file
shape. Every file that carries relationship columns SHALL carry, per
(relationship column, granule) holding any reference, a no-false-negative
membership filter over the stored references, so a reverse lookup reads only
the granules whose filter admits the target; a granule holding no reference of
a kind SHALL be skipped for that kind. A relationship reference SHALL assert structure only — reply, grouping,
or reference — and SHALL NOT assert that one event caused another
(INV-EVENTSTREAM-NON-CAUSAL); it is disjoint from the relational entity
relationship graph, which remains derived, rebuildable, entity-level state.

#### Scenario: Unrelated stream pays nothing
- **WHEN** a stream whose events declare no relationships is written
- **THEN** the relationship family is absent, no relationship columns are
  materialized, and the fixed envelope is unchanged

#### Scenario: Children of an event resolve by filter
- **WHEN** a caller asks for the events whose `parent` reference names event X
- **THEN** the query is answered as a pushdown equality lookup on
  `(relationship_kind = parent, target_ref = X)` pruned by the standard
  metadata and filter machinery, and X's own stored bytes are not modified or
  required

#### Scenario: Federated predecessors and authorisers round-trip
- **WHEN** a Matrix event declaring 20 `prev` and 10 `auth` references, including a room version 1 target
  `$abc:example.org` in the `external_id` space, is written and read back
- **THEN** every reference comes back in declaration order, and a reverse lookup on `auth` naming one target
  reads only the granules whose reference filter admits it

#### Scenario: Relationship never licenses causal claims
- **WHEN** two events are connected by a `link` or `related` reference
- **THEN** the connection is presented as structural association only, and
  causal phrasing remains licensed solely by the Revenue Intelligence
  corroboration paths

### Requirement: Relationship references accepted without referential integrity
The store SHALL accept relationship references without validating the target at
ingest: the append path SHALL NOT perform a lookup to confirm a target exists,
and a dangling reference — a target not yet ingested, retention-expired,
erased, or never existing — SHALL be stored as declared data, not an error.
Query-time resolution of a reference SHALL be an equality lookup in the
declared identifier space; an unresolvable reference SHALL yield an empty
resolution, never a query error.

#### Scenario: Reply arrives before its parent
- **WHEN** a reply declaring a `parent` reference is ingested before the
  parent event itself
- **THEN** the reply commits normally with the reference stored as declared,
  and the reference resolves once the parent is ingested

#### Scenario: Target erased after commit
- **WHEN** a reference's target is later erased or expires out of retention
- **THEN** the referencing events are unchanged and resolving the reference
  yields an empty result, not an error

### Requirement: Thread reconstruction without recursion
A threaded event SHALL carry both its `parent` reference and its denormalized
`root` reference, declared by the producer at ingest, so that an entire thread
is reconstructible with a single non-recursive equality lookup on
`(relationship_kind = root, target_ref = R)` ordered by the envelope. The
logical contract for fetching a conversation SHALL NOT require recursive
parent-chain traversal. A thread's root event SHALL carry no `root` reference;
absence of `parent` and `root` references marks an event as a root or as
unthreaded, and no self-reference is stored.

#### Scenario: Whole conversation in one filter
- **WHEN** a caller fetches the thread rooted at event R
- **THEN** a single equality lookup on `(relationship_kind = root,
  target_ref = R)`, ordered by the envelope, returns the thread's events
  without recursive traversal

#### Scenario: Root marked by absence
- **WHEN** an event carries no `parent` and no `root` reference
- **THEN** it is a thread root or an unthreaded event, and no self-reference
  is stored or required

### Requirement: Signed-event provenance column family
The logical event model SHALL define an optional Tier A provenance column
family for events originating from a signed protocol, carrying at minimum:
`author_pubkey` (the signing public key), `signature` (the protocol signature
bytes), `signature_scheme` (a registry-controlled tag; BIP-340 Schnorr over
secp256k1 for Nostr), `protocol_event_id` (the protocol's own content-derived
event identifier), `protocol_kind` (the protocol's event-kind integer), and
`claimed_at` (the author-claimed timestamp as a `TimestampValue`). The
signature-scheme registry SHALL also hold Ed25519 (signing the canonical bytes
themselves). Events of a protocol that several parties sign (Matrix federation)
SHALL instead carry `matrix_room_version` and `signer_signatures`: one
`(scheme, signer, key_id, public key, signature)` entry per signature, so each
re-verifies from the stored key alone. The family
SHALL be absent — materializing no columns — for streams whose events carry no
signatures; its presence SHALL NOT alter the fixed envelope, and `sequence`
assignment, pruning, and ordering SHALL remain governed by the envelope alone,
with `claimed_at` treated as author-influenced data, never as an ordering or
retention authority. Provenance columns SHALL participate in the ordinary
column machinery — physical encodings, workload-aware promotion, data-class
labels, and field-level authorization — and SHALL be exactly as visible as the
event they attest, never more.

#### Scenario: Unsigned stream pays nothing
- **WHEN** a stream whose events carry no protocol signatures is written
- **THEN** the provenance family is absent, no provenance columns are
  materialized, and the fixed envelope is unchanged

#### Scenario: Claimed timestamp never orders the log
- **WHEN** a signed event arrives whose `claimed_at` is far behind or ahead of
  ingest time
- **THEN** `(epoch, sequence)` assignment and pruning metadata derive from the
  envelope's ingest-side fields, and `claimed_at` is stored as a queryable
  column only

#### Scenario: Provenance visibility follows the event
- **WHEN** a caller lacking authorization for an event's stream or fields
  queries provenance columns
- **THEN** the provenance columns are filtered by the same field-level
  authorization and public-output rules as the event's other columns

### Requirement: Offline re-verifiability of signed events
For every event carrying the provenance family, the store SHALL preserve the
signature, the author public key, and the fields entering the protocol's
canonical serialization byte-exactly, such that a reader holding only the
event's stored form — from LiveRows, a published HEF, or a compacted
SuperHEF — can reconstruct the canonical serialization bit-for-bit, recompute
`protocol_event_id`, and verify `signature` against `author_pubkey` with no
access to the original wire bytes and no trust in the store. Ingest
transcoding, workload-aware promotion, encoding changes, splice rewrites, and
SuperHEF compaction SHALL NOT break this property. The store SHALL NOT persist
a verification verdict column; verification SHALL always be recomputable from
the stored bytes.

#### Scenario: Archived event re-verifies from a SuperHEF
- **WHEN** a signed event written years earlier is read back from a compacted
  SuperHEF
- **THEN** its canonical serialization reconstructs bit-for-bit, the
  recomputed `protocol_event_id` matches the stored column, and the signature
  verifies against `author_pubkey` offline

#### Scenario: Federated event re-verifies with several server signatures
- **WHEN** a Matrix event signed by two servers is read back from a sealed file together with its raw payload,
  external id, room version, and stored signatures
- **THEN** the content hash recomputes from the canonical JSON, both signatures verify over the redacted canonical
  form, and in room version 3 and later the recomputed reference hash, as `$` and unpadded base64 (URL-safe from
  version 4), equals the stored external id

#### Scenario: No stored verdict to drift
- **WHEN** an implementation proposes persisting a `verified` flag alongside
  the provenance family
- **THEN** the flag is rejected by the logical model; presence in the durable
  journal implies ingest-time verification, and any later check recomputes
  from the stored bytes


### Requirement: Events findable by external protocol id
A stream whose tenant declares external ids SHALL store each event's external
protocol id (1 to 255 bytes, a Matrix `$...` id say) in an optional
`external_id` column, absent for streams that carry none. Each file carrying
the column SHALL index it in its footer, as `(id hash, row ordinal)` pairs
sorted for binary search, so a reader finds an event's row without scanning a
granule and confirms every hit against the stored id. Across files, a
cross-file index from `(tenant, external id)` to `(file, generation, row
ordinal)` SHALL be persisted through an interface the embedding application
backs with durable storage, and when an id is recorded from several files the
newest generation SHALL win.

#### Scenario: Lookup hits and misses without a scan
- **WHEN** a reader asks a file for the row of an external id, including one longer than 32 bytes
- **THEN** a present id resolves to its row reading at most the one granule block that confirms it, and an absent
  id resolves to nothing without reading any block

#### Scenario: Duplicate id across files
- **WHEN** the same external id is recorded from files of two generations, in either order
- **THEN** the cross-file index points at the row in the newer generation's file
