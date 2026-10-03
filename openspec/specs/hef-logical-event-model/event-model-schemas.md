# HEF Logical Event Model — Field Schemas and Encodings

Companion artifact for the `hef-logical-event-model` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


Every event has a fixed envelope plus optional typed attributes. Stable logical names are unit-neutral. Physical encodings may use nanosecond timestamps, compact integers, dictionaries, or hashes internally.

### Required logical envelope fields

```text
event_id              u128 / UUID / ULID-compatible
tenant_id             internal tenant identifier; never public output
epoch                 u64; internal only
sequence              u64; internal only
stream_id             u64
stream_sequence       u64
occurred_at           TimestampValue
ingested_at           TimestampValue
source_id             dictionary id
event_type_id         dictionary id
entity_type_id        dictionary id
entity_id_hash        u64 or u128
entity_id_ref         optional payload/string reference
actor_id_hash         optional
account_id_hash       optional
trace_id              optional
payload_ref           internal offset/reference into payload arena
flags                 bitset
schema_version        u32
```

A physical implementation may expose a derived `sequence_key` internally for sorting and pruning, but it must be defined as an internal physical alias of `(epoch, sequence)`. Public APIs must not expose `epoch`, `sequence`, `sequence_key`, row offsets, or payload references.

### Time and duration encoding

Logical fields use `TimestampValue` and `DurationValue`.

```text
occurred_at: TimestampValue
ingested_at: TimestampValue
latency:     DurationValue
duration:    DurationValue
```

Physical storage may encode timestamp and duration values as signed or unsigned integer deltas with nanosecond precision. Physical block metadata may record the precision and epoch base, but stable schema names must remain unit-neutral.

### Optional promoted columns

The writer may promote common event attributes into typed columns. Promotion is automatic and workload-aware. The format must not require users to manually configure every promoted field.

Examples:

```text
amount_decimal
currency
stage
status
outcome
country
region
product_id
campaign_id
opportunity_id
customer_id
workspace_id
latency
duration
score
```

Promotion candidates are selected from:

```text
filters
group-by expressions
order-by expressions
join keys
rules
prepared views
revenue metric definitions
dashboard cards
chat/investigation tool queries
frequent payload-field fallback scans
```

Promotion must preserve data-class labels and field-level authorization.

### Payload

There is exactly one payload format:

```text
harana_variant_v1
```

`harana_variant_v1` is a structured binary value encoding based on the Parquet Variant value encoding, with one Harana deviation: the key/metadata dictionary is not embedded per value. Field ids resolve against an external shared dictionary (`VariantDictionaryV1` per HEJ frame; variant dictionary blocks per HEF granule). The encoding supports arbitrary JSON-like structures (objects, arrays, and typed scalars including integers, decimals, floats, booleans, strings, binary, timestamps, and null) and provides offset-based navigation: extracting one path touches only the field-id/offset arrays on the path and the target scalar bytes, never sibling fields.

Ingest transcoding rule:

```text
Ingest routes must transcode every accepted source body into harana_variant_v1 before HEJ append.

Accepted source formats at the ingest edge: JSON, Protobuf, Avro, MessagePack, raw bytes.

Source bytes are not stored. The canonical harana_variant_v1 value is the stored payload.

Raw or undecodable binary bodies are stored as a single Variant binary scalar.

Transcoding is deterministic: object keys are interned into the shared dictionary;
numbers map to the narrowest lossless Variant numeric type; decimals preserve scale;
timestamp-like values follow TimestampValue semantics; Protobuf and Avro transcoding
require the registered source schema at the ingest route.

Source-format lineage (format name and schema fingerprint) may be recorded through
source_schema_ref. Lineage is internal and is not public output.

A body whose transcoded value cannot fit inside a 1 MiB HEJ frame follows the
external_immutable_payload_ref rule in the HEJ payload arena (see the hef-physical-artifacts capability).
```

QueryEngine must avoid reading payload values unless the query asks for paths not available as authorized promoted columns or shredded payload columns. Payload reads must be late-materialized:

```text
1. use manifest/footer/index/aggregate metadata to prune files and ranges;
2. read only required envelope, promoted, and shredded payload columns;
3. apply authorization, filters, deletion vectors, and corrections;
4. read residual variant values only for final matching rows that require unshredded paths;
5. extract only the requested paths from residual values;
6. redact or suppress payload fields according to the caller's authorization.
```

References: <https://github.com/apache/parquet-format/blob/master/VariantEncoding.md>, <https://github.com/apache/parquet-format/blob/master/VariantShredding.md>

### Harana analytical column families

HEF must support column families beyond the minimal event envelope so the native event storage path covers Harana analytical workloads.

Families are tiered by temporal computability: Tier A families are computable at seal time from the single event and live in the base HEF file; Tier B families are cross-event, asynchronous, revisable model outputs and live in a sibling derived-columns file (HEF bytes, manifest `file_type = derived_columns`), row-aligned ordinal-for-ordinal to the base with per-column producer/model lineage.

```text
event_envelope (Tier A)
  required event identity, time, stream, source/type/entity, and payload reference fields

source_type_entity (Tier A)
  dictionaries and normalized source/type/entity fields used by filters and rollups

safe_retry_and_ingest_mode (Tier A)
  client request id hash, ingest route class, connector delivery identity, retry class, and dedupe hints

classification_labels (Tier A)
  pii/phi/ephi/confidential/sensitive/never_send_externally/never_train and derived strongest-source labels

freetext_columns (Tier A)
  schema-declared free-text fields shredded by declaration into their own columnar family
  (text-tuned encoding; serves bulk re-extraction and per-subject erasure, not the query hot path)

cluster_columns (Tier B)
  cluster ids, parent assignments, pattern ids, and clustering lineage

embedding_columns_internal (Tier A)
  per-event-deterministic embeddings or quantized projections used only by internal analytical/model tools

revenue_anomaly_columns (Tier B)
  revenue-anomaly scores, reference paths, residuals, and window lineage

driver_and_cause_columns (Tier B)
  cause ids, driver candidates, graph/driver analysis lineage, and confidence metrics

revenue_metric_columns (Tier A)
  promoted revenue measures, entity refs, period keys, metric lineage, currency, stage/status, and amount fields

prepared_view_lineage_columns (Tier A)
  view definition version, source schema version, input coverage, readiness, and rebuild lineage

chat_investigation_context_columns (Tier A)
  compact safe display text, title/summary fields, entity labels, evidence grouping keys,
  tool-visible source facets, and optional snippet references

relationship_reference_columns (Tier A)
  references the event declares about other events — parent, denormalized thread root, links,
  and loose associations — one column per kind holding the canonical space-tagged reference
  text an equality filter compares; absent for streams that declare no relationships
```

Internal-only column families must be blocked from public output unless an owning service defines a public-safe derived representation.

### Public-output and authorization boundary

The QueryEngine provider and owner services must enforce public-output safety below API serialization.

Required scan phase:

```text
authorize requested columns;
drop blocked internal columns;
reject raw internal identity fields for public callers;
map internal event identity to opaque cursor only at the API boundary;
late-materialize raw payload only for authorized callers;
redact payload fields before returning them;
return public-safe evidence refs for chat/investigation outputs.
```

Blocked by default:

```text
raw tenant_id
raw epoch
raw sequence
internal sequence_key
object-store paths
local cache paths
row offsets
payload_ref
raw payload bytes for unauthorized callers
embedding columns
internal analytical columns
raw data-class labels on ordinary event/product APIs
```

---

