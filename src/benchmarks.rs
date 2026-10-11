//! Gate-enforcement logic for HEF/HEJ benchmark acceptance.
//!
//! Each gate is a pure function or a small registry that the conformance harness drives. No hardware benchmarks run
//! here; the structures enforce the POLICY that capabilities cannot be marked ready until a named benchmark
//! demonstrates the threshold on the declared profile. Requirement: "Acceptance gates are mechanically enforced by the
//! committed

use hashbrown::{HashMap, HashSet};

/// Flush sizes at or above this byte count are negative-control only and must not be selected as the normal HEJ frame
/// size.
pub const NEGATIVE_CONTROL_FLUSH_BYTES: u64 = 128 * 1024;

/// Returns `true` when `flush_bytes` is small enough to be the default HEJ flush target. A 128 KiB or larger unit is a
/// negative control only.
pub fn flush_unit_valid_as_default(flush_bytes: u64) -> bool {
    flush_bytes < NEGATIVE_CONTROL_FLUSH_BYTES
}

/// Whether both flush-size targets required by the spec have been separately benchmarked before a default is selected.
#[derive(Debug, Clone)]
pub struct FlushTargetMeasurements {
    pub four_kib_benchmarked: bool,
    pub sixteen_kib_benchmarked: bool,
}

/// Returns `true` when both the 4 KiB and 16 KiB autonomous-flush targets have been benchmarked, so either may be
/// selected as the default.
pub fn flush_default_selectable(m: &FlushTargetMeasurements) -> bool {
    m.four_kib_benchmarked && m.sixteen_kib_benchmarked
}

/// Measurements for one query class, needed to decide readiness.
#[derive(Debug, Clone)]
pub struct QueryClassMeasurement {
    pub cold_cache_passes: bool,
    pub explicitly_excluded: bool,
    pub warm_cache_passes: bool,
}

/// Returns `true` when the query class may be added to the ready set.
///
/// A class is ready only when both warm-cache and cold-cache profiles pass, or the class is explicitly excluded from
/// the gate.
pub fn query_class_is_ready(m: &QueryClassMeasurement) -> bool {
    m.explicitly_excluded || (m.warm_cache_passes && m.cold_cache_passes)
}

/// All conditions the gate evaluates before allowing an accelerator backend.
#[derive(Debug, Clone)]
pub struct AcceleratorConditions {
    pub batch_exceeds_threshold: bool,
    pub benefit_is_positive: bool,
    pub encoding_compatible: bool,
    pub fallback_available: bool,
    pub parity_passed: bool,
    pub provider_detected: bool,
    pub self_test_passed: bool,
}

/// Which compute path the gate selects for one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceleratorPath {
    Accelerated,
    Software,
}

/// Returns the path the gate selects. All seven conditions must be true for the accelerated path; any failure routes to
/// the software path.
pub fn accelerator_path(conditions: &AcceleratorConditions) -> AcceleratorPath {
    if conditions.batch_exceeds_threshold
        && conditions.benefit_is_positive
        && conditions.encoding_compatible
        && conditions.fallback_available
        && conditions.parity_passed
        && conditions.provider_detected
        && conditions.self_test_passed
    {
        AcceleratorPath::Accelerated
    } else {
        AcceleratorPath::Software
    }
}

/// Detects whether a query snapshot returns the same event from both HEF and the live-overlay layer. Duplicates
/// indicate a snapshot-boundary bug.
#[derive(Debug, Default)]
pub struct SnapshotSafetyChecker {
    hef_ids: HashSet<u128>,
}

impl SnapshotSafetyChecker {
    /// Records an event id seen in HEF results for this snapshot.
    pub fn record_hef(&mut self, event_id: u128) {
        self.hef_ids.insert(event_id);
    }

    /// Returns `true` when `event_id` from the live-overlay also appears in HEF results — a duplicate that must not
    /// happen.
    pub fn is_duplicate(&self, event_id: u128) -> bool {
        self.hef_ids.contains(&event_id)
    }
}

/// Returns the minimum blackout window in whole seconds required to re-extract `corpus_bytes` at the gated
/// `throughput_bytes_per_sec`. A release must not declare a shorter window.
pub fn min_blackout_seconds(corpus_bytes: u64, throughput_bytes_per_sec: u64) -> u64 {
    if throughput_bytes_per_sec == 0 {
        return u64::MAX;
    }
    corpus_bytes.div_ceil(throughput_bytes_per_sec)
}

/// Returns `true` when the observed live-query p99 latency stays at or below its declared bound while maintenance is
/// active; `false` when p99 exceeds the bound, which causes the gate to fail.
pub fn maintenance_gate_passes(live_p99_micros: u64, live_p99_bound_micros: u64) -> bool {
    live_p99_micros <= live_p99_bound_micros
}

/// A performance result recorded as the committed baseline for one benchmark.
#[derive(Debug, Clone)]
pub struct CommittedBaseline {
    pub benchmark_id: String,
    /// Whether the accelerated path was verified byte-for-byte equivalent to the software path in the same benchmark
    /// run.
    pub equivalence_verified: bool,
    /// The baseline measurement value (units depend on the metric).
    pub measured_value: f64,
    /// Fraction above the baseline that still passes (e.g. 0.10 = 10 %).
    pub tolerance_fraction: f64,
}

/// Error when an accelerated baseline cannot be committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceleratedBaselineError {
    EquivalenceNotVerified,
}

/// Errors from the gate registry when a capability cannot be marked ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessError {
    NoBenchmark { capability: String },
}

/// Why a capability was dropped from the ready set.
#[derive(Debug, Clone, PartialEq)]
pub struct RegressionReason {
    pub baseline_value: f64,
    pub benchmark_id: String,
    pub measured_value: f64,
}

/// Tracks which capabilities have backing benchmarks, committed baselines, and membership in the ready set.
///
/// Enforces the rule that a capability cannot be marked ready without a named benchmark, and that a regressed baseline
/// removes a capability from the ready set until the regression is resolved. Requirement: "Acceptance gates are
/// mechanically enforced by the committed
#[derive(Debug, Default)]
pub struct BenchGateRegistry {
    baselines: HashMap<String, CommittedBaseline>,
    benchmarks_for: HashMap<String, Vec<String>>,
    ready: HashSet<String>,
}

impl BenchGateRegistry {
    /// Associates `benchmark_id` with `capability`, establishing that the capability has a backing benchmark in the
    /// harness.
    pub fn register_benchmark(&mut self, capability: impl Into<String>, benchmark_id: impl Into<String>) {
        self.benchmarks_for
            .entry(capability.into())
            .or_default()
            .push(benchmark_id.into());
    }

    /// Records a committed baseline for a benchmark run, making it available for future regression checks.
    pub fn commit_baseline(&mut self, baseline: CommittedBaseline) {
        self.baselines.insert(baseline.benchmark_id.clone(), baseline);
    }

    /// Records a committed baseline for an accelerated path. Fails with `EquivalenceNotVerified` when the run did not
    /// verify byte-for-byte equivalence against the software path in the same workload.
    pub fn commit_accelerated_baseline(&mut self, baseline: CommittedBaseline) -> Result<(), AcceleratedBaselineError> {
        if !baseline.equivalence_verified {
            return Err(AcceleratedBaselineError::EquivalenceNotVerified);
        }
        self.baselines.insert(baseline.benchmark_id.clone(), baseline);
        Ok(())
    }

    /// Adds `capability` to the ready set. Fails with `NoBenchmark` when no benchmark has been registered for the
    /// capability.
    pub fn mark_ready(&mut self, capability: impl Into<String>) -> Result<(), ReadinessError> {
        let capability = capability.into();
        if !self.benchmarks_for.contains_key(&capability) {
            return Err(ReadinessError::NoBenchmark { capability });
        }
        self.ready.insert(capability);
        Ok(())
    }

    /// Checks `measured_value` against the committed baseline for `benchmark_id`. When the measurement exceeds the
    /// tolerance, removes `capability` from the ready set and returns the regression details.
    pub fn detect_regression(
        &mut self,
        capability: &str,
        benchmark_id: &str,
        measured_value: f64,
    ) -> Option<RegressionReason> {
        let baseline = self.baselines.get(benchmark_id)?;
        let threshold = baseline.measured_value * (1.0 + baseline.tolerance_fraction);
        if measured_value > threshold {
            self.ready.remove(capability);
            Some(RegressionReason {
                baseline_value: baseline.measured_value,
                benchmark_id: benchmark_id.to_owned(),
                measured_value,
            })
        } else {
            None
        }
    }

    /// Returns `true` when `capability` is currently in the ready set.
    pub fn is_ready(&self, capability: &str) -> bool {
        self.ready.contains(capability)
    }
}

/// The gated requests-per-cold-open target on the `cold-cache/S3-durable` profile.
pub const REQUESTS_PER_COLD_OPEN_TARGET: u32 = 1;

/// A cold open's measured request count, needed to evaluate the requests-per-cold-open metadata-economics gate.
#[derive(Debug, Clone)]
pub struct ColdOpenMeasurement {
    pub justification_recorded: bool,
    pub request_count: u32,
}

/// Returns `true` when a cold open on `cold-cache/S3-durable` meets the metadata-economics gate: at or below the
/// requests-per-cold-open target, or over target with a recorded justification.
pub fn cold_open_gate_passes(m: &ColdOpenMeasurement) -> bool {
    m.request_count <= REQUESTS_PER_COLD_OPEN_TARGET || m.justification_recorded
}

/// Planner metadata bytes a query reads for one file eliminated by pruning, needed to evaluate the lazy-marks pruning
/// gate.
#[derive(Debug, Clone)]
pub struct PrunedFileMetadataMeasurement {
    /// Bytes the planner would have read for this file's granule-level marks without lazy per-stripe marks — its full
    /// share.
    pub full_granule_marks_share_bytes: u64,
    pub planner_metadata_bytes_read: u64,
}

/// Returns `true` when a file pruned out during planning costs near-zero planner metadata bytes rather than its full
/// granule-level marks share — the economics a build claiming lazy per-stripe marks must deliver.
pub fn pruned_file_metadata_gate_passes(m: &PrunedFileMetadataMeasurement) -> bool {
    m.planner_metadata_bytes_read < m.full_granule_marks_share_bytes
}

/// The gated dependent-round-trip ceiling for an `entity_id` point lookup on the `cold-cache/S3-durable` profile.
pub const S3_DURABLE_LOOKUP_ROUND_TRIP_TARGET: u32 = 2;

/// An `entity_id` point lookup's measured cost on the `cold-cache/S3-durable` profile, needed to evaluate the
/// object-store budget gate. This profile is judged by round trips and its own per-request millisecond allowance —
/// never against the sub-5 ms `warm`/`cold-cache/NVMe-durable` figure.
#[derive(Debug, Clone)]
pub struct S3DurableLookupMeasurement {
    pub dependent_round_trips: u32,
    pub elapsed_millis: f64,
    pub per_request_millis_budget: f64,
}

/// Returns `true` when an `entity_id` lookup on `cold-cache/S3-durable` stays within the object-store budget: at most
/// `S3_DURABLE_LOOKUP_ROUND_TRIP_TARGET` dependent round trips, and elapsed time within that many round trips' worth of
/// the budgeted per-request millisecond allowance.
pub fn s3_durable_lookup_gate_passes(m: &S3DurableLookupMeasurement) -> bool {
    m.dependent_round_trips <= S3_DURABLE_LOOKUP_ROUND_TRIP_TARGET
        && m.elapsed_millis <= m.dependent_round_trips as f64 * m.per_request_millis_budget
}

/// One lifecycle stage's write-amplification measurement (durable bytes written ÷ logical bytes produced), needed to
/// evaluate the per-stage WAF gate.
#[derive(Debug, Clone)]
pub struct WafMeasurement {
    pub ceiling: f64,
    pub measured_waf: f64,
}

/// Returns `true` when a lifecycle stage's measured WAF is at or below its gated ceiling. An FDP/ZNS placement path or
/// a splice-based rewrite path is not marked ready unless this passes at the relevant stage.
pub fn waf_gate_passes(m: &WafMeasurement) -> bool {
    m.measured_waf <= m.ceiling
}

/// The gated ceiling on what a write-ahead log object's header and trailer add on top of its records, in bytes. Log
/// objects are a few KiB, so their fixed framing must stay a few dozen bytes rather than HEF's 4 KiB header block.
pub const LOG_OBJECT_FRAMING_CEILING_BYTES: u64 = 32;

/// The gated ceiling on the bytes a verified forward read of a log object hashes before returning its first record:
/// one 1 MiB chunk group, however large the object.
pub const LOG_STREAM_FIRST_RECORD_CEILING_BYTES: u64 = 1 << 20;

/// One log object's framing cost and verified forward-read cost, needed to evaluate the log-object gate.
#[derive(Debug, Clone)]
pub struct LogObjectMeasurement {
    /// Bytes checked against the pinned root before the first record was returned.
    pub first_record_verified_bytes: u64,
    /// Header plus trailer, excluding every record's own length and checksum.
    pub framing_bytes: u64,
    pub object_bytes: u64,
    /// Bytes checked against the pinned root by the time every record had been read.
    pub verified_bytes: u64,
}

/// Returns `true` when a log object's framing is within its ceiling and a verified forward read streams it: the first
/// record arrives within one chunk group of hashing, and a full read hashes no more bytes than the object holds.
pub fn log_object_gate_passes(m: &LogObjectMeasurement) -> bool {
    m.framing_bytes <= LOG_OBJECT_FRAMING_CEILING_BYTES
        && m.first_record_verified_bytes <= LOG_STREAM_FIRST_RECORD_CEILING_BYTES
        && m.verified_bytes <= m.object_bytes
}

/// The gated ceiling on stored graph bytes per edge, forward and reverse copies together.
pub const GRAPH_EDGE_BYTES_TARGET: f64 = 6.0;

/// The edge blocks of a graph measured against the edges they describe, needed to evaluate the graph edge size gate.
#[derive(Debug, Clone)]
pub struct GraphEdgeBytesMeasurement {
    /// Each parent link counted once, though it is stored forward and reverse.
    pub edges: u64,
    pub stored_bytes: u64,
}

/// Returns `true` when a graph's edge blocks stay within `GRAPH_EDGE_BYTES_TARGET` bytes per edge. A graph with no
/// edges passes only when it stores nothing.
pub fn graph_edge_bytes_gate_passes(m: &GraphEdgeBytesMeasurement) -> bool {
    m.stored_bytes as f64 <= m.edges as f64 * GRAPH_EDGE_BYTES_TARGET
}

/// The gated ceiling on the bytes an audit file adds over the same rows built as an event file: its profile and audit
/// footer sections and their two section directory entries. The hash chain stores nothing per row, so the cost stays
/// the same however many rows the file holds.
pub const AUDIT_FILE_OVERHEAD_CEILING_BYTES: u64 = 192;

/// What an audit file adds over an event file of the same rows, measured at two row counts, needed to evaluate the
/// audit file overhead gate.
#[derive(Debug, Clone)]
pub struct AuditFileOverheadMeasurement {
    pub added_bytes_at_few_rows: u64,
    pub added_bytes_at_many_rows: u64,
}

/// Returns `true` when an audit file adds at most `AUDIT_FILE_OVERHEAD_CEILING_BYTES` over an event file of the same
/// rows and adds exactly as much at many rows as at few, so the hash chain costs no stored bytes per row.
pub fn audit_file_overhead_gate_passes(m: &AuditFileOverheadMeasurement) -> bool {
    m.added_bytes_at_few_rows <= AUDIT_FILE_OVERHEAD_CEILING_BYTES
        && m.added_bytes_at_many_rows == m.added_bytes_at_few_rows
}

/// The gated ceiling on object-store requests for one cold snapshot lookup: one read of the file's tail to open it,
/// then one read of the granule holding the key.
pub const SNAPSHOT_LOOKUP_REQUEST_TARGET: u32 = 2;

/// One cold point lookup in a snapshot file opened remotely, needed to evaluate the snapshot lookup gate.
#[derive(Debug, Clone)]
pub struct SnapshotLookupMeasurement {
    /// Tail and granule bytes together.
    pub bytes_fetched: u64,
    /// Requests to open the file and look the key up, the open's tail read included.
    pub requests: u32,
    pub stored_bytes: u64,
}

/// Returns `true` when a cold snapshot lookup issues at most `SNAPSHOT_LOOKUP_REQUEST_TARGET` requests and fetches
/// less than the whole file, which is what a snapshot file offers over reading a whole per-partition archive.
pub fn snapshot_lookup_gate_passes(m: &SnapshotLookupMeasurement) -> bool {
    m.requests <= SNAPSHOT_LOOKUP_REQUEST_TARGET && m.bytes_fetched < m.stored_bytes
}

/// The gated cost of one recipient's entry in an inbox file's recipient directory, in bytes: its two identity hashes,
/// its row range, and its newest queue point, eight bytes each.
pub const INBOX_DIRECTORY_BYTES_PER_RECIPIENT: u64 = 48;

/// The gated ceiling on what the recipient directory adds to a footer beyond its entries, in bytes: its entry count
/// and its line in the footer's section directory (56 bytes today).
pub const INBOX_DIRECTORY_FRAMING_CEILING_BYTES: u64 = 64;

/// One recipient's queue read from an inbox file, with the size of the file's recipient directory, needed to evaluate
/// the inbox read gate.
#[derive(Debug, Clone)]
pub struct InboxReadMeasurement {
    /// Granules holding at least one of the recipient's rows: the fewest any read of its queue can touch.
    pub covering_granules: u64,
    /// Bytes the recipient directory adds to the file's footer, framing included.
    pub directory_bytes: u64,
    pub granules_read: u64,
    pub recipients: u64,
}

/// Returns `true` when the recipient directory costs `INBOX_DIRECTORY_BYTES_PER_RECIPIENT` bytes per recipient plus
/// at most `INBOX_DIRECTORY_FRAMING_CEILING_BYTES`, and reading one recipient's queue touches no granule beyond those
/// holding its rows.
pub fn inbox_read_gate_passes(m: &InboxReadMeasurement) -> bool {
    m.directory_bytes <= m.recipients * INBOX_DIRECTORY_BYTES_PER_RECIPIENT + INBOX_DIRECTORY_FRAMING_CEILING_BYTES
        && m.granules_read <= m.covering_granules
}

/// The gated ceiling on what declaring a profile other than `Event` adds to a file, in bytes: one footer directory
/// entry (52 B) and its one-byte section, with the 4 KiB header block no larger.
pub const PROFILE_OVERHEAD_CEILING_BYTES: u64 = 64;

/// The same rows built once as an event file and once under another profile, needed to evaluate the profile overhead
/// gate.
#[derive(Debug, Clone)]
pub struct ProfileOverheadMeasurement {
    pub event_file_bytes: u64,
    /// Bytes before the first stripe: the fixed header block.
    pub event_header_block_bytes: u64,
    pub profiled_file_bytes: u64,
    /// Bytes before the first stripe: the fixed header block.
    pub profiled_header_block_bytes: u64,
}

/// Returns `true` when declaring a profile leaves the header block the same size and adds at most
/// `PROFILE_OVERHEAD_CEILING_BYTES` to the file.
pub fn profile_overhead_gate_passes(m: &ProfileOverheadMeasurement) -> bool {
    m.profiled_header_block_bytes == m.event_header_block_bytes
        && m.profiled_file_bytes.saturating_sub(m.event_file_bytes) <= PROFILE_OVERHEAD_CEILING_BYTES
}

/// The gated number of range requests one level of a graph walk may send to each file it reads: one, every edge block
/// that level needs from the file joined into a single request.
pub const GRAPH_WALK_REQUESTS_PER_FILE_PER_LEVEL: u64 = 1;

/// One breadth-first walk over a graph spread across several remote files, needed to evaluate the graph walk request
/// gate.
#[derive(Debug, Clone)]
pub struct GraphWalkMeasurement {
    pub bytes_fetched: u64,
    pub files: u64,
    /// Levels of ancestors the walk returned.
    pub levels: u64,
    /// Range requests sent to every file together, after each file was opened.
    pub requests: u64,
}

/// Returns `true` when a graph walk sent at most `GRAPH_WALK_REQUESTS_PER_FILE_PER_LEVEL` requests per file per level.
pub fn graph_walk_gate_passes(m: &GraphWalkMeasurement) -> bool {
    m.requests
        <= m.levels
            .saturating_mul(m.files)
            .saturating_mul(GRAPH_WALK_REQUESTS_PER_FILE_PER_LEVEL)
}
