use super::*;
use crate::invariants::sim::SerialEncodeExecutor;
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
