use super::*;
use crate::benchmarks::{AuditFileOverheadMeasurement, audit_file_overhead_gate_passes};
use crate::invariants::sim::SerialEncodeExecutor;
use crate::layout::LayoutTargets;
use crate::layout::constant::AUDIT_GENESIS;
use crate::layout::footer::RetentionMode;
use crate::writer::build::{
    AnalyticalColumn, BuiltHef, HefRowChange, build_hef_file, update_hef_file_from_changes_profiled_with_executor,
    update_hef_file_from_changes_with_executor, update_hef_file_from_source_changes_profiled_with_executor,
    update_hef_file_from_source_changes_with_executor, update_hef_file_profiled_with_executor,
    update_hef_file_with_executor,
};
use crate::writer::compaction::plan_rewrite;
use crate::writer::constant::AUDIT_REWRITE_REFUSED;
use crate::writer::projection::build_entity_projection;
use crate::writer::source_form::tests::{plain_row, source_form_config};
use std::time::Instant;

fn settings() -> AuditSettings {
    AuditSettings {
        chain_start: AUDIT_GENESIS,
        legal_hold: false,
        retention: Some(ObjectLockRetention {
            mode: RetentionMode::Governance,
            retain_until: crate::events::TimestampValue::from_physical_nanos(1),
        }),
    }
}

fn audit_config() -> HefBuildConfig {
    HefBuildConfig {
        audit: Some(settings()),
        profile: Profile::Audit,
        ..source_form_config(4)
    }
}

fn rows() -> Vec<BuildRow> {
    (0..10).map(plain_row).collect()
}

fn audit_file() -> BuiltHef {
    build_hef_file(rows(), &audit_config()).unwrap()
}

#[test]
fn every_update_path_refuses_an_audit_file() {
    let source = audit_file();
    let before = rows();
    let mut after = rows();
    after[2] = plain_row(99);
    after[2].sequence = before[2].sequence;
    let changes = vec![HefRowChange {
        after: after[2].clone(),
        before: before[2].clone(),
        row_ordinal: 2,
    }];
    let config = audit_config();
    let encode = &SerialEncodeExecutor;
    let refused = Err(AUDIT_REWRITE_REFUSED);
    assert_eq!(
        update_hef_file_with_executor(&source, &before, after.clone(), &config, encode).map(|_| ()),
        refused
    );
    assert_eq!(
        update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &config, encode).map(|_| ()),
        refused
    );
    assert_eq!(
        update_hef_file_from_source_changes_with_executor(&source, &changes, &config, encode).map(|_| ()),
        refused
    );
    assert_eq!(
        update_hef_file_profiled_with_executor(&source, &before, after.clone(), &config, encode).map(|_| ()),
        refused
    );
    assert_eq!(
        update_hef_file_from_changes_profiled_with_executor(&source, after.clone(), &changes, &config, encode)
            .map(|_| ()),
        refused
    );
    assert_eq!(
        update_hef_file_from_source_changes_profiled_with_executor(&source, &changes, &config, encode).map(|_| ()),
        refused
    );
    // Rewriting an audit file into an event file is refused just the same.
    assert_eq!(
        update_hef_file_with_executor(&source, &before, after, &source_form_config(4), encode).map(|_| ()),
        refused
    );
}

#[test]
fn a_splice_rewrite_refuses_an_audit_file() {
    let source = audit_file();
    let event_file = build_hef_file(rows(), &source_form_config(4)).unwrap();
    assert_eq!(plan_rewrite(&source, &source), Err(AUDIT_REWRITE_REFUSED));
    assert_eq!(plan_rewrite(&event_file, &source), Err(AUDIT_REWRITE_REFUSED));
    assert!(plan_rewrite(&event_file, &event_file).is_ok());
}

#[test]
fn an_entity_projection_refuses_audit_rows() {
    assert_eq!(
        build_entity_projection(rows(), &audit_config(), &SerialEncodeExecutor).map(|_| ()),
        Err(AUDIT_REWRITE_REFUSED)
    );
}

#[test]
fn audit_settings_go_with_the_audit_profile_only() {
    let missing = HefBuildConfig {
        audit: None,
        ..audit_config()
    };
    assert_eq!(
        build_hef_file(rows(), &missing).map(|_| ()),
        Err(FormatError::Structural {
            rule: "an audit file needs its audit settings"
        })
    );
    let stray = HefBuildConfig {
        audit: Some(settings()),
        ..source_form_config(4)
    };
    assert_eq!(
        build_hef_file(rows(), &stray).map(|_| ()),
        Err(FormatError::Structural {
            rule: "audit settings apply only to an audit file"
        })
    );
}

#[test]
fn an_audit_build_refuses_what_the_row_hash_does_not_cover() {
    let mut carrying = rows();
    carrying[3].external_id = Some(b"$event".to_vec());
    assert_eq!(
        build_hef_file(carrying, &audit_config()).map(|_| ()),
        Err(FormatError::Structural {
            rule: "an audit row carries only its envelope and payload"
        })
    );
    let analytical = HefBuildConfig {
        analytical_columns: vec![AnalyticalColumn {
            column_id: crate::columns::column_ids::CONTEXT_BASE,
            data: crate::encoding::ColumnData::U64(vec![0; 10]),
            internal_only: false,
            kind: crate::layout::footer::ColumnKind::U64,
            name: "score".to_owned(),
            substring_searchable: false,
        }],
        ..audit_config()
    };
    assert_eq!(
        build_hef_file(rows(), &analytical).map(|_| ()),
        Err(FormatError::Structural {
            rule: "an audit file carries no analytical columns"
        })
    );
}

#[test]
fn an_audit_file_chains_from_the_supplied_start() {
    let first = audit_file();
    let trail = first.footer.audit.unwrap();
    assert_eq!(trail.chain_start, AUDIT_GENESIS);
    let next = build_hef_file(
        rows(),
        &HefBuildConfig {
            audit: Some(AuditSettings {
                chain_start: trail.chain_end,
                ..settings()
            }),
            ..audit_config()
        },
    )
    .unwrap();
    let next_trail = next.footer.audit.unwrap();
    assert_eq!(next_trail.chain_start, trail.chain_end);
    // The same rows chained from a different start end somewhere else.
    assert_ne!(next_trail.chain_end, trail.chain_end);
    assert_eq!(next_trail.retention, settings().retention);
}

fn footer_len(built: &BuiltHef) -> usize {
    let len = built.bytes.len();
    u64::from_le_bytes(built.bytes[len - 12..len - 4].try_into().unwrap()) as usize
}

/// The bytes an audit file of `count` rows with `retention` adds over the same rows built as an event file, checking
/// every one of them sits in the footer.
fn audit_added_bytes(count: u64, retention: Option<ObjectLockRetention>) -> u64 {
    let rows: Vec<BuildRow> = (0..count).map(plain_row).collect();
    let config = HefBuildConfig {
        audit: Some(AuditSettings {
            retention,
            ..settings()
        }),
        ..audit_config()
    };
    let audit = build_hef_file(rows.clone(), &config).unwrap();
    let event = build_hef_file(rows, &source_form_config(4)).unwrap();
    let added = audit.bytes.len() - event.bytes.len();
    assert_eq!(footer_len(&audit) - footer_len(&event), added, "only the footer grows");
    added as u64
}

#[test]
fn an_audit_file_adds_a_fixed_footer_cost_and_nothing_per_row() {
    // Two 52-byte section directory entries, the one-byte profile section, and the audit section: two 32-byte hashes,
    // the legal hold byte and the retention tag, then the retention's mode byte and retain-until time when it has one.
    for (retention, expected) in [(settings().retention, 2 * 52 + 1 + 75), (None, 2 * 52 + 1 + 66)] {
        let measurement = AuditFileOverheadMeasurement {
            added_bytes_at_few_rows: audit_added_bytes(10, retention),
            added_bytes_at_many_rows: audit_added_bytes(1_000, retention),
        };
        println!(
            "audit file overhead, retention {}: {measurement:?}",
            retention.is_some()
        );
        assert_eq!(measurement.added_bytes_at_few_rows, expected);
        assert!(audit_file_overhead_gate_passes(&measurement), "{measurement:?}");
    }
}

/// Builds 100,000 rows as an event file and as an audit file, seven times each, and prints the median build times and
/// what hashing the rows into the chain adds per row. Run by hand:
/// `cargo test --release --all-features --lib audit_chain_build_cost -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode audit chain build benchmark"]
fn audit_chain_build_cost() {
    const ROWS: u64 = 100_000;
    let rows: Vec<BuildRow> = (0..ROWS).map(plain_row).collect();
    let event_config = HefBuildConfig {
        targets: LayoutTargets::default(),
        ..source_form_config(4)
    };
    let audit_config = HefBuildConfig {
        audit: Some(settings()),
        profile: Profile::Audit,
        ..event_config.clone()
    };
    let time_build = |config: &HefBuildConfig| {
        let input = rows.clone();
        let started = Instant::now();
        let built = build_hef_file(input, config).unwrap();
        let elapsed = started.elapsed().as_nanos();
        std::hint::black_box(built);
        elapsed
    };
    let mut event_samples = Vec::new();
    let mut audit_samples = Vec::new();
    for iteration in 0..7 {
        if iteration % 2 == 0 {
            event_samples.push(time_build(&event_config));
            audit_samples.push(time_build(&audit_config));
        } else {
            audit_samples.push(time_build(&audit_config));
            event_samples.push(time_build(&event_config));
        }
    }
    event_samples.sort_unstable();
    audit_samples.sort_unstable();
    let event_median = event_samples[3];
    let audit_median = audit_samples[3];
    let extra = audit_median as f64 - event_median as f64;
    println!(
        "audit chain build, {ROWS} rows: event median {:.1} ms, audit median {:.1} ms, chain adds {:.0} ns/row \
         ({:+.1}%)",
        event_median as f64 / 1e6,
        audit_median as f64 / 1e6,
        extra / ROWS as f64,
        100.0 * extra / event_median as f64
    );
}
