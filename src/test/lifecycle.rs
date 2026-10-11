use super::*;
use crate::typed_id::TypedIdTestExt;

fn entry(file_id: u128, first_sequence: u64, size_bytes: u64, part_state: PartState) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence,
            last_sequence: first_sequence + 99,
        },
        feature_metadata: None,
        file_seal: [0u8; 32],
        file_id,
        file_type: FileType::HefFile,
        footer_len: None,
        graph_event_id_filter: None,
        graph_summary: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state,
        profile: Profile::Event,
        required_feature_flags: 0,
        size_bytes,
        tenant_id: TenantId::new_test_id(7),
        tree_len: None,
    }
}

fn generation(files: Vec<HefFileEntry>) -> ManifestGeneration {
    ManifestGeneration {
        files,
        footer_mirror: None,
        generation: 1,
        index_artifacts: Vec::new(),
        retirements: Vec::new(),
    }
}

fn policy() -> CompactionPolicy {
    CompactionPolicy {
        max_jobs_per_cycle: 4,
        max_merge_width: 4,
        min_output_over_largest: 2,
        roll_target_bytes: 1_000,
    }
}

#[test]
fn a_lone_trickle_tail_is_not_rewritten() {
    // One finished file and one fresh small file: rewriting the small one buys nothing, so no job is scheduled — the
    // failure the doubling bound exists to prevent is exactly "merge whenever a small file exists".
    let generation = generation(vec![
        entry(1, 100, 1_000, PartState::Active),
        entry(2, 200, 40, PartState::Active),
    ]);
    assert!(plan_compaction_cycle(&policy(), &generation, TenantId::new_test_id(7)).is_empty());
}

#[test]
fn the_doubling_bound_defers_a_small_tail_behind_a_larger_input() {
    // 400 + 50: merging rewrites 400 bytes to grow the largest input by an eighth — deferred. Once enough small files
    // accumulate for the combined size to double the largest input, the merge is worth its write amplification.
    let deferred = generation(vec![
        entry(1, 100, 400, PartState::Active),
        entry(2, 200, 50, PartState::Active),
    ]);
    assert!(plan_compaction_cycle(&policy(), &deferred, TenantId::new_test_id(7)).is_empty());

    let ripe = generation(vec![
        entry(1, 100, 400, PartState::Active),
        entry(2, 200, 150, PartState::Active),
        entry(3, 300, 150, PartState::Active),
        entry(4, 400, 150, PartState::Active),
    ]);
    let jobs = plan_compaction_cycle(&policy(), &ripe, TenantId::new_test_id(7));
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].file_ids, vec![1, 2, 3, 4]);
    assert_eq!(jobs[0].expected_output_bytes, 850);
}

#[test]
fn a_small_tail_behind_an_oversized_run_head_still_merges() {
    // The head is too large for its neighbours to double, but the three small files behind it double each other; the
    // sliding window merges them without rewriting the head.
    let generation = generation(vec![
        entry(1, 100, 800, PartState::Active),
        entry(2, 200, 60, PartState::Active),
        entry(3, 300, 60, PartState::Active),
        entry(4, 400, 60, PartState::Active),
    ]);
    let jobs = plan_compaction_cycle(&policy(), &generation, TenantId::new_test_id(7));
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].file_ids, vec![2, 3, 4]);
}

#[test]
fn merge_width_and_jobs_per_cycle_stay_bounded() {
    let files: Vec<HefFileEntry> = (0u32..40)
        .map(|i| entry(u128::from(i), 100 * u64::from(i) + 100, 100, PartState::Active))
        .collect();
    let generation = generation(files);
    let jobs = plan_compaction_cycle(&policy(), &generation, TenantId::new_test_id(7));
    assert_eq!(jobs.len(), policy().max_jobs_per_cycle, "work per cycle is bounded");
    for job in &jobs {
        assert!(job.file_ids.len() <= policy().max_merge_width, "merge width is bounded");
    }
}

#[test]
fn reaching_the_roll_target_qualifies_without_doubling() {
    // 900 + 150 only grows the largest input 1.17x, but the output reaches the roll target, so the pair is finished
    // in one merge rather than deferred forever.
    let generation = generation(vec![
        entry(1, 100, 900, PartState::Active),
        entry(2, 200, 150, PartState::Active),
    ]);
    let jobs = plan_compaction_cycle(&policy(), &generation, TenantId::new_test_id(7));
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].expected_output_bytes, 1_050);
}

#[test]
fn finished_files_other_states_and_other_tenants_are_never_candidates() {
    let mut other_tenant = entry(5, 500, 100, PartState::Active);
    other_tenant.tenant_id = TenantId::new_test_id(8);
    let mut sidecar = entry(6, 600, 100, PartState::Active);
    sidecar.file_type = FileType::DerivedColumns;
    let generation = generation(vec![
        entry(1, 100, 1_000, PartState::Active),
        entry(2, 200, 100, PartState::Outdated),
        entry(3, 300, 100, PartState::Sealed),
        other_tenant,
        sidecar,
        entry(4, 400, 100, PartState::Active),
    ]);
    assert!(plan_compaction_cycle(&policy(), &generation, TenantId::new_test_id(7)).is_empty());
}

#[test]
fn repeated_cycles_converge_without_thrashing() {
    // Apply each cycle's jobs (replace the merged files with one output) and count merges. Convergence must reach
    // quiescence, and the doubling bound caps rewrites-per-byte logarithmically — a naive policy that merges the tail
    // every cycle would spend far more merges over the same files.
    let mut files: Vec<HefFileEntry> = (0u32..32)
        .map(|i| entry(u128::from(i), 100 * u64::from(i) + 100, 60, PartState::Active))
        .collect();
    let mut merges = 0usize;
    let mut next_id = 1_000u128;
    for _ in 0..64 {
        let jobs = plan_compaction_cycle(&policy(), &generation(files.clone()), TenantId::new_test_id(7));
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            merges += 1;
            let first = files
                .iter()
                .position(|entry| job.file_ids.contains(&entry.file_id))
                .unwrap();
            let sequence = files[first].coverage.first_sequence;
            files.retain(|entry| !job.file_ids.contains(&entry.file_id));
            files.insert(
                first,
                entry(next_id, sequence, job.expected_output_bytes, PartState::Active),
            );
            next_id += 1;
        }
    }
    let final_jobs = plan_compaction_cycle(&policy(), &generation(files.clone()), TenantId::new_test_id(7));
    assert!(final_jobs.is_empty(), "cycles must reach quiescence");
    // 32 files of 60 bytes merge four at a time toward the roll target; quiescence takes about a dozen jobs. A
    // thrashing policy would spend one merge per cycle per remaining small file and blow far past this.
    assert!(merges <= 12, "expected convergence in few merges, took {merges}");
}

/// Dropping an index artifact is omitting it from the next generation: the diff names exactly the dropped objects as
/// the sweeper's retirement candidates, and a kept artifact is never named. Implements `hef-manifest-integration` —
/// "Index artifacts ride the manifest generation".
#[test]
fn dropped_index_artifacts_become_the_sweepers_retirement_candidates() {
    use crate::indexes::artifact::ArtifactKind;
    use crate::lifecycle::{IndexArtifactRef, retired_artifact_keys};

    let reference = |key: &str| IndexArtifactRef {
        artifact_blake3: [7u8; 32],
        column_id: 1000,
        covered_file_id: 0xAB,
        kind: ArtifactKind::BinaryFuse,
        object_key: key.to_owned(),
    };
    let mut previous = generation(Vec::new());
    previous.index_artifacts = vec![reference("keep.hia"), reference("drop.hia")];
    let mut next = generation(Vec::new());
    next.index_artifacts = vec![reference("keep.hia")];

    assert_eq!(retired_artifact_keys(&previous, &next), vec!["drop.hia".to_owned()]);
    assert!(retired_artifact_keys(&next, &next).is_empty());
}

/// `coverage_index` must agree with the per-call `covers` scan it replaces in hot eviction paths, including when an
/// `Outdated` predecessor and its `Active` replacement transiently overlap the same sequences, and it must never let
/// one tenant's coverage answer for another's range.
#[test]
fn coverage_index_agrees_with_covers_including_overlapping_entries() {
    let mut other_tenant = entry(9, 100, 100, PartState::Active);
    other_tenant.tenant_id = TenantId::new_test_id(8);
    let generation = generation(vec![
        entry(1, 100, 100, PartState::Outdated),
        entry(2, 150, 100, PartState::Active),
        entry(3, 300, 100, PartState::Sealed),
        other_tenant,
    ]);
    let index = generation.coverage_index();
    let tenant = TenantId::new_test_id(7);

    let probes = [
        SequenceRange {
            epoch: 1,
            first_sequence: 100,
            last_sequence: 199,
        },
        SequenceRange {
            epoch: 1,
            first_sequence: 150,
            last_sequence: 249,
        },
        SequenceRange {
            epoch: 1,
            first_sequence: 200,
            last_sequence: 260,
        },
        SequenceRange {
            epoch: 1,
            first_sequence: 300,
            last_sequence: 399,
        },
        SequenceRange {
            epoch: 2,
            first_sequence: 100,
            last_sequence: 150,
        },
    ];
    for range in probes {
        assert_eq!(
            index.covers(&range, tenant),
            generation.covers(&range, tenant),
            "coverage_index disagreed with covers for {range:?}"
        );
    }
    // The Sealed entry (300..399) is not yet visible to any snapshot, so it never counts as coverage.
    assert!(!index.covers(
        &SequenceRange {
            epoch: 1,
            first_sequence: 300,
            last_sequence: 399,
        },
        tenant
    ));
    // Another tenant's identical range never counts, even though its own file covers it.
    assert!(!generation.covers(
        &SequenceRange {
            epoch: 1,
            first_sequence: 100,
            last_sequence: 199,
        },
        TenantId::new_test_id(999)
    ));
}

#[test]
fn a_graph_event_id_filter_in_the_entry_prunes_files_that_cannot_hold_an_event() {
    let ids: Vec<Vec<u8>> = (0..50u64)
        .map(|sn| format!("$held{sn}:example.org").into_bytes())
        .collect();
    let index = crate::layout::graph::EventIdIndex::build(ids.iter().map(Vec::as_slice).zip(0..))
        .unwrap()
        .unwrap();
    let mut graph = entry(1, 1, 100, PartState::Active);
    graph.profile = Profile::Graph;
    graph.graph_event_id_filter = Some(index.filter);
    assert!(ids.iter().all(|id| graph.may_hold_graph_event(id)));
    let pruned = (0..1_000u64)
        .filter(|n| !graph.may_hold_graph_event(format!("$absent{n}:example.org").as_bytes()))
        .count();
    assert!(pruned > 900, "only {pruned} of 1000 absent ids pruned");

    let json = serde_json::to_value(&graph).unwrap();
    assert_eq!(serde_json::from_value::<HefFileEntry>(json.clone()).unwrap(), graph);
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("graph_event_id_filter");
    let legacy: HefFileEntry = serde_json::from_value(legacy).unwrap();
    assert_eq!(legacy.graph_event_id_filter, None);
    assert!(
        legacy.may_hold_graph_event(b"$absent0:example.org"),
        "no filter, no pruning"
    );
}

#[test]
fn a_manifest_entry_written_before_profiles_reads_as_an_event_file() {
    let mut graph = entry(1, 1, 100, PartState::Active);
    graph.profile = Profile::Graph;
    let json = serde_json::to_value(&graph).unwrap();
    assert_eq!(serde_json::from_value::<HefFileEntry>(json.clone()).unwrap(), graph);
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("profile");
    assert_eq!(
        serde_json::from_value::<HefFileEntry>(legacy).unwrap().profile,
        Profile::Event
    );
}

const MIB: u64 = 1024 * 1024;

/// A graph file of `subject` holding events `first_sn` through `first_sn + 99`, `size_bytes` long.
fn graph_entry(file_id: u128, subject: &[u8], first_sn: u64, size_bytes: u64) -> HefFileEntry {
    let mut graph = entry(file_id, first_sn + 1, size_bytes, PartState::Active);
    graph.profile = Profile::Graph;
    graph.graph_summary = Some(GraphFileSummary {
        chains: Vec::new(),
        depth: None,
        max_event_sn: first_sn + 99,
        min_event_sn: first_sn,
        subject: subject.to_vec(),
    });
    graph
}

/// `count` consecutive graph files of one subject, each `size_bytes` long, with file ids from 1.
fn graph_run(count: u64, size_bytes: u64) -> Vec<HefFileEntry> {
    (0..count)
        .map(|i| graph_entry(u128::from(i) + 1, b"room", i * 100, size_bytes))
        .collect()
}

fn graph_jobs(files: Vec<HefFileEntry>) -> Vec<Vec<u128>> {
    plan_graph_compaction_cycle(
        &GraphCompactionPolicy::default(),
        &generation(files),
        TenantId::new_test_id(7),
    )
    .into_iter()
    .map(|job| job.file_ids)
    .collect()
}

#[test]
fn graph_files_merge_when_they_are_small_together_or_too_many() {
    assert_eq!(
        graph_jobs(graph_run(3, 2 * MIB)),
        [vec![1, 2, 3]],
        "under 8 MiB together"
    );
    assert!(
        graph_jobs(graph_run(3, 3 * MIB)).is_empty(),
        "8 MiB or more in eight files or fewer waits"
    );
    assert!(
        graph_jobs(graph_run(8, 5 * MIB)).is_empty(),
        "eight files are not too many"
    );
    assert_eq!(
        graph_jobs(graph_run(9, 5 * MIB)),
        [(1..=9).collect::<Vec<u128>>()],
        "nine are"
    );
    assert!(
        graph_jobs(graph_run(1, MIB)).is_empty(),
        "one file has nothing to merge with"
    );
}

#[test]
fn a_graph_merge_stops_before_its_output_passes_256_mib() {
    // Nine 60 MiB files: four fit under 256 MiB, then four more, and the last waits alone.
    let jobs = plan_graph_compaction_cycle(
        &GraphCompactionPolicy::default(),
        &generation(graph_run(9, 60 * MIB)),
        TenantId::new_test_id(7),
    );
    let ids: Vec<Vec<u128>> = jobs.iter().map(|job| job.file_ids.clone()).collect();
    assert_eq!(ids, [vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
    assert!(jobs.iter().all(|job| job.expected_output_bytes == 240 * MIB));
}

#[test]
fn a_graph_merge_never_crosses_a_subject_a_gap_or_a_finished_file() {
    let mut files = graph_run(3, MIB);
    files.push(graph_entry(10, b"other", 300, MIB));
    assert_eq!(graph_jobs(files), [vec![1, 2, 3]], "another subject's file stays out");

    let gap = vec![
        graph_entry(1, b"room", 0, MIB),
        graph_entry(2, b"room", 100, MIB),
        graph_entry(3, b"room", 250, MIB),
        graph_entry(4, b"room", 350, MIB),
    ];
    assert_eq!(graph_jobs(gap), [vec![1, 2], vec![3, 4]]);

    let mut finished = graph_run(5, MIB);
    if let Some(middle) = finished.get_mut(2) {
        middle.size_bytes = 64 * MIB;
    }
    assert_eq!(graph_jobs(finished), [vec![1, 2], vec![4, 5]]);

    let mut plain = graph_run(3, MIB);
    for file in &mut plain {
        file.graph_summary = None;
    }
    assert!(
        graph_jobs(plain).is_empty(),
        "files without a graph summary are not graph files"
    );
}

#[test]
fn the_event_file_planner_leaves_graph_files_alone() {
    let files = graph_run(3, 40);
    assert!(plan_compaction_cycle(&policy(), &generation(files), TenantId::new_test_id(7)).is_empty());
}

#[test]
fn a_graph_summary_round_trips_through_the_entry_and_defaults_to_absent() {
    let mut graph = graph_entry(1, b"!room:example.org", 4096, 100);
    if let Some(summary) = &mut graph.graph_summary {
        summary.chains = vec![ChainRange {
            chain_id: 3,
            max_seq: 40,
            min_seq: 7,
        }];
        summary.depth = Some(DepthRange { max: 9_000, min: 8_000 });
    }
    let json = serde_json::to_value(&graph).unwrap();
    assert_eq!(serde_json::from_value::<HefFileEntry>(json.clone()).unwrap(), graph);
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("graph_summary");
    assert_eq!(
        serde_json::from_value::<HefFileEntry>(legacy).unwrap().graph_summary,
        None
    );
}
