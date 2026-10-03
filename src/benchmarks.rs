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
