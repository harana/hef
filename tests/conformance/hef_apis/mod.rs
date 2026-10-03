//! Conformance tests for the `hef-apis` capability: the public HEF API surface, including the bulk-egress reader path,
//! batched payload reads, and whole-batch journal appends.

mod batched_payload_read_amortizes_per_granule_work;
mod bulk_egress_reader_path;
mod context_evidence_and_introspection_apis_stay_public_safe;
mod journal_batch_append_admits_a_caller_batch_whole_or_not_at_all;
mod zero_copy_string_column_scans;
