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

/// The gated ceiling on requests one level of a cold graph walk sends to each file of the subject.
pub const GRAPH_WALK_REQUESTS_PER_FILE_TARGET: u64 = 1;

/// The requests a cold graph walk sent, level by level, needed to evaluate the walk request gate.
#[derive(Debug, Clone)]
pub struct GraphWalkRequestsMeasurement {
    /// Files of the walked subject, so a level may send at most this many requests.
    pub files: u64,
    pub levels: u64,
    /// The most requests any one level sent.
    pub max_level_requests: u64,
    pub requests: u64,
}

/// Returns `true` when no level of a walk sent more than `GRAPH_WALK_REQUESTS_PER_FILE_TARGET` requests per file of
/// the subject.
pub fn graph_walk_requests_gate_passes(m: &GraphWalkRequestsMeasurement) -> bool {
    m.max_level_requests <= m.files * GRAPH_WALK_REQUESTS_PER_FILE_TARGET
}

/// The gated ceiling on footer bytes a graph file's event-id index costs per indexed event, its filter included.
pub const GRAPH_EVENT_ID_BYTES_TARGET: f64 = 12.0;

/// The gated ceiling on the share of absent event ids the event-id filter lets through to a file.
pub const GRAPH_EVENT_ID_FALSE_POSITIVE_TARGET: f64 = 0.02;

/// A graph file's event-id index measured against the events it indexes and a set of ids it does not hold, needed to
/// evaluate the event-id index gate.
#[derive(Debug, Clone)]
pub struct GraphEventIdIndexMeasurement {
    /// Absent ids the filter admitted.
    pub absent_admitted: u64,
    pub absent_probes: u64,
    pub events: u64,
    /// The whole footer section, its filter included.
    pub section_bytes: u64,
}

/// Returns `true` when the event-id index stays within `GRAPH_EVENT_ID_BYTES_TARGET` bytes per event and its filter
/// admits at most `GRAPH_EVENT_ID_FALSE_POSITIVE_TARGET` of the absent ids probed.
pub fn graph_event_id_index_gate_passes(m: &GraphEventIdIndexMeasurement) -> bool {
    m.section_bytes as f64 <= m.events as f64 * GRAPH_EVENT_ID_BYTES_TARGET
        && m.absent_admitted as f64 <= m.absent_probes as f64 * GRAPH_EVENT_ID_FALSE_POSITIVE_TARGET
}

/// The gated ceiling on edge-block bytes one event's chain-cover label adds: half its 8 raw bytes.
pub const GRAPH_CHAIN_LABEL_BYTES_TARGET: f64 = 4.0;

/// The gated ceiling on chain-cover footer bytes per link, each chain's range included: half a link's 16 raw bytes.
pub const GRAPH_CHAIN_LINK_BYTES_TARGET: f64 = 8.0;

/// A graph's chain-cover labels and links measured against the bytes they add to the file, needed to evaluate the
/// chain-cover size gate.
#[derive(Debug, Clone)]
pub struct GraphChainCoverMeasurement {
    /// Edge-block bytes with the labels, less the same blocks without them.
    pub label_bytes: u64,
    pub labels: u64,
    /// The footer's chain-cover section.
    pub link_bytes: u64,
    pub links: u64,
}

/// Returns `true` when labels cost at most `GRAPH_CHAIN_LABEL_BYTES_TARGET` bytes each and the chain-cover section at
/// most `GRAPH_CHAIN_LINK_BYTES_TARGET` bytes per link.
pub fn graph_chain_cover_gate_passes(m: &GraphChainCoverMeasurement) -> bool {
    m.label_bytes as f64 <= m.labels as f64 * GRAPH_CHAIN_LABEL_BYTES_TARGET
        && m.link_bytes as f64 <= m.links as f64 * GRAPH_CHAIN_LINK_BYTES_TARGET
}

/// The gated ceiling on new state-chunk bytes one state event writes to a 100,000-member room's state: one 16 KiB
/// chunk's worth.
pub const STATE_CHUNK_BYTES_PER_EVENT_TARGET: f64 = 16_384.0;

/// The state chunks a run of state events wrote, needed to evaluate the state-chunk write gate.
#[derive(Debug, Clone)]
pub struct StateChunkWriteMeasurement {
    /// Chunks new to each event's tree, sealed, summed over the events.
    pub bytes_written: u64,
    pub events: u64,
}

/// Returns `true` when state events write at most `STATE_CHUNK_BYTES_PER_EVENT_TARGET` bytes of new chunks each on
/// average.
pub fn state_chunk_write_gate_passes(m: &StateChunkWriteMeasurement) -> bool {
    m.bytes_written as f64 <= m.events as f64 * STATE_CHUNK_BYTES_PER_EVENT_TARGET
}

/// The gated ceiling on the 99th-percentile time a state lookup spends on the reader's own work: all of a warm lookup,
/// and what a cold lookup takes beyond its object-store round trips.
pub const STATE_LOOKUP_LOCAL_P99_MICROS_TARGET: f64 = 1_000.0;

/// Point lookups of one key in the state at an event (state root, then state get), against an empty chunk cache and
/// repeated against a cache holding their chunks, needed to evaluate the state lookup gate.
#[derive(Debug, Clone)]
pub struct StateLookupMeasurement {
    /// The most chunks one cold lookup fetched; each fetch waits on the one before.
    pub cold_fetches_max: u64,
    /// Round trips included, at `round_trip_micros` each.
    pub cold_p99_micros: f64,
    pub round_trip_micros: f64,
    /// Levels of the state tree, so a cold lookup may fetch at most this many chunks.
    pub tree_levels: u64,
    pub warm_p99_micros: f64,
}

/// Returns `true` when a cold lookup fetches at most one chunk per tree level and, beyond those round trips, the cold
/// and warm 99th percentiles stay within `STATE_LOOKUP_LOCAL_P99_MICROS_TARGET`.
pub fn state_lookup_gate_passes(m: &StateLookupMeasurement) -> bool {
    m.cold_fetches_max <= m.tree_levels
        && m.cold_p99_micros <= m.tree_levels as f64 * m.round_trip_micros + STATE_LOOKUP_LOCAL_P99_MICROS_TARGET
        && m.warm_p99_micros <= STATE_LOOKUP_LOCAL_P99_MICROS_TARGET
}

/// Replaying a room's join history into graph files and state chunks, needed to evaluate the room replay gate.
#[derive(Debug, Clone)]
pub struct RoomReplayMeasurement {
    pub events: u64,
    pub graph_bytes: u64,
    /// Every chunk new to some event's state tree, sealed, as uploads would store them.
    pub state_chunk_bytes: u64,
    pub wall_millis: f64,
}

/// Returns `true` when a replay writes at most `STATE_CHUNK_BYTES_PER_EVENT_TARGET` bytes of new state chunks per
/// event. Wall time is recorded, not gated.
pub fn room_replay_gate_passes(m: &RoomReplayMeasurement) -> bool {
    m.state_chunk_bytes as f64 <= m.events as f64 * STATE_CHUNK_BYTES_PER_EVENT_TARGET
}

/// The gated ceiling on Chimp bytes per reading of a slowly varying full-precision gauge, against 8 raw bytes.
pub const XOR_FLOAT_BYTES_PER_VALUE_TARGET: f64 = 3.5;

/// One float column's readings stored as Chimp XOR floats and in the smallest encoding that predates Chimp, needed to
/// evaluate the XOR float gate.
#[derive(Debug, Clone)]
pub struct XorFloatMeasurement {
    pub best_existing_bytes: u64,
    pub chimp_bytes: u64,
    pub values: u64,
}

/// Returns `true` when Chimp stores the readings in fewer bytes than every earlier encoding and within
/// `XOR_FLOAT_BYTES_PER_VALUE_TARGET` bytes per value.
pub fn xor_float_gate_passes(m: &XorFloatMeasurement) -> bool {
    m.chimp_bytes < m.best_existing_bytes && m.chimp_bytes as f64 <= m.values as f64 * XOR_FLOAT_BYTES_PER_VALUE_TARGET
}

/// The gated ceiling on column blocks a rollup aggregate reads for a range that cuts two buckets: each edge bucket's
/// time and value blocks, in at most two granules.
pub const TIME_SERIES_ROLLUP_EDGE_BLOCK_READS_TARGET: u64 = 8;

/// Column blocks a time-series aggregate read through rollups and by scanning every row, needed to evaluate the rollup
/// gate.
#[derive(Debug, Clone)]
pub struct TimeSeriesRollupMeasurement {
    /// A range of whole buckets, answered through rollups.
    pub aligned_block_reads: u64,
    /// A range that cuts a bucket at each end, answered through rollups.
    pub cut_block_reads: u64,
    /// The same cut range answered by scanning every row.
    pub scan_block_reads: u64,
}

/// Returns `true` when a range of whole buckets reads no column block and a cut range reads at most
/// `TIME_SERIES_ROLLUP_EDGE_BLOCK_READS_TARGET` blocks, fewer than a scan of it.
pub fn time_series_rollup_gate_passes(m: &TimeSeriesRollupMeasurement) -> bool {
    m.aligned_block_reads == 0
        && m.cut_block_reads <= TIME_SERIES_ROLLUP_EDGE_BLOCK_READS_TARGET
        && m.cut_block_reads < m.scan_block_reads
}
