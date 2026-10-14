use super::*;
use crate::events::variant::VariantValue;
use crate::events::{EventId, TimestampValue};
use crate::layout::constant::AUDIT_GENESIS;
use crate::layout::footer::{ObjectLockRetention, RetentionMode, encode_footer};
use crate::layout::{LayoutTargets, Profile, required_features};
use crate::typed_id::TypedIdTestExt;
use crate::writer::audit::AuditSettings;
use crate::writer::build::{BuildPayload, BuildRow, BuiltHef, HefBuildConfig, build_hef_file};
use crate::writer::source_form::tests::{source_form_config, source_form_row};
use std::collections::BTreeMap;
use std::time::Instant;

fn audit_config(chain_start: [u8; 32]) -> HefBuildConfig {
    HefBuildConfig {
        audit: Some(AuditSettings {
            chain_start,
            legal_hold: true,
            retention: Some(ObjectLockRetention {
                mode: RetentionMode::Compliance,
                retain_until: TimestampValue::from_physical_nanos(4_000_000_000_000_000_000),
            }),
        }),
        profile: Profile::Audit,
        ..source_form_config(4)
    }
}

/// Audit row `i`: a payload of a different shape on most rows, so every value kind the chain hashes reads back the
/// way it was written.
fn audit_row(i: u64) -> BuildRow {
    let payload = match i % 4 {
        0 => VariantValue::Object(BTreeMap::from([
            ("actor".to_owned(), VariantValue::String(format!("user-{i}"))),
            ("approved".to_owned(), VariantValue::Bool(i.is_multiple_of(3))),
            (
                "at".to_owned(),
                VariantValue::Timestamp(TimestampValue::from_physical_nanos(i as i64)),
            ),
            ("count".to_owned(), VariantValue::Int(i as i64)),
            ("ratio".to_owned(), VariantValue::Double(i as f64 / 3.0)),
            (
                "tags".to_owned(),
                VariantValue::Array(vec![VariantValue::String("break-glass".to_owned())]),
            ),
        ])),
        1 => VariantValue::Object(BTreeMap::from([(
            "key".to_owned(),
            VariantValue::Object(BTreeMap::from([("id".to_owned(), VariantValue::Uuid(u128::from(i)))])),
        )])),
        2 => VariantValue::String(format!("signature check {i}")),
        _ => VariantValue::Int(i as i64),
    };
    let mut row = source_form_row(i, payload);
    row.envelope.actor_id = i.is_multiple_of(2).then(|| format!("admin-{i}"));
    match i % 7 {
        5 => row.payload = BuildPayload::None,
        6 => row.payload = BuildPayload::ExternalRef(format!("blob/{i}")),
        _ => {}
    }
    row
}

fn rows(sequences: std::ops::Range<u64>) -> Vec<BuildRow> {
    sequences.map(audit_row).collect()
}

fn open(built: &BuiltHef) -> HefFile {
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap()
}

fn trail(built: &BuiltHef) -> AuditTrail {
    built.footer.audit.unwrap()
}

/// Three audit files, each starting where the one before it ended.
fn three_files() -> [BuiltHef; 3] {
    let first = build_hef_file(rows(0..10), &audit_config(AUDIT_GENESIS)).unwrap();
    let second = build_hef_file(rows(10..20), &audit_config(trail(&first).chain_end)).unwrap();
    let third = build_hef_file(rows(20..30), &audit_config(trail(&second).chain_end)).unwrap();
    [first, second, third]
}

/// `built` with its footer's chain end replaced by `chain_end`, as an attacker who rewrote the rows but kept the
/// recorded chain would leave it. The file seal no longer matches, so it opens only without one.
fn with_chain_end(built: &BuiltHef, chain_end: [u8; 32]) -> HefFile {
    let mut footer = built.footer.clone();
    footer.audit = Some(AuditTrail {
        chain_end,
        ..trail(built)
    });
    let blob = encode_footer(&footer);
    let old_len = u64::from_le_bytes(
        built.bytes[built.bytes.len() - 12..built.bytes.len() - 4]
            .try_into()
            .unwrap(),
    );
    let mut bytes = built.bytes[..built.bytes.len() - (old_len as usize + 12)].to_vec();
    bytes.extend_from_slice(&blob);
    bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    bytes.extend_from_slice(b"HEF1");
    assert!(HefFile::open(bytes.clone(), Some(&built.file_seal)).is_err());
    HefFile::open(bytes, None).unwrap()
}

#[test]
fn a_run_of_audit_files_verifies_and_chains_across_files() {
    let files = three_files();
    for built in &files {
        assert!(built.footer.granules.len() > 1, "the rows span several granules");
        assert_eq!(built.header.profile, Profile::Audit);
        assert_ne!(
            built.footer.required_feature_flags & required_features::AUDIT_HASH_CHAIN,
            0
        );
        assert_ne!(built.header.feature_flags & required_features::AUDIT_HASH_CHAIN, 0);
        let file = open(built);
        assert_eq!(file.footer().audit, built.footer.audit);
    }
    let opened: Vec<HefFile> = files.iter().map(open).collect();
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &opened),
        Ok(trail(&files[2]).chain_end)
    );
    // One file on its own verifies from its own chain start.
    assert_eq!(
        verify_audit_files(trail(&files[1]).chain_start, &opened[1..2]),
        Ok(trail(&files[1]).chain_end)
    );
    assert_ne!(trail(&files[0]).chain_end, trail(&files[1]).chain_end);
}

/// Fields every row carries are stored as their own columns rather than in the payload; the chain still reads them
/// back exactly.
#[test]
fn a_file_whose_payload_fields_are_stored_as_columns_verifies() {
    let rows: Vec<BuildRow> = (0..12)
        .map(|i| {
            source_form_row(
                i,
                VariantValue::Object(BTreeMap::from([
                    (
                        "amount".to_owned(),
                        VariantValue::Decimal {
                            unscaled: i128::from(i) * 101,
                            scale: 2,
                        },
                    ),
                    (
                        "at".to_owned(),
                        VariantValue::Timestamp(TimestampValue::from_physical_nanos(i as i64)),
                    ),
                    ("blob".to_owned(), VariantValue::Binary(vec![i as u8; 3])),
                    ("n".to_owned(), VariantValue::Int(i as i64)),
                    ("ratio".to_owned(), VariantValue::Float(i as f32 / 4.0)),
                    ("who".to_owned(), VariantValue::String(format!("user-{}", i % 3))),
                ])),
            )
        })
        .collect();
    let built = build_hef_file(rows, &audit_config(AUDIT_GENESIS)).unwrap();
    assert!(
        !built.footer.shredded.is_empty(),
        "uniform fields are stored as columns"
    );
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[open(&built)]),
        Ok(trail(&built).chain_end)
    );
}

#[test]
fn the_retention_and_legal_hold_round_trip_through_the_file() {
    let built = build_hef_file(rows(0..3), &audit_config(AUDIT_GENESIS)).unwrap();
    let audit = open(&built).footer().audit.unwrap();
    assert!(audit.legal_hold);
    assert_eq!(
        audit.retention,
        Some(ObjectLockRetention {
            mode: RetentionMode::Compliance,
            retain_until: TimestampValue::from_physical_nanos(4_000_000_000_000_000_000),
        })
    );
    assert_eq!(audit.chain_start, AUDIT_GENESIS);
}

#[test]
fn a_tampered_row_is_detected() {
    let [first, second, _] = three_files();
    let mut tampered = rows(0..10);
    tampered[6].envelope.event_id = EventId::new_test_id(0xBAD);
    let rebuilt = build_hef_file(tampered, &audit_config(AUDIT_GENESIS)).unwrap();

    // Keeping the original chain end is caught by recomputing the rows.
    let forged = with_chain_end(&rebuilt, trail(&first).chain_end);
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[forged, open(&second)]),
        Err(AuditChainError::RowsAltered { file_index: 0 })
    );
    // An honest chain over the altered rows no longer meets the next file.
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[open(&rebuilt), open(&second)]),
        Err(AuditChainError::ChainBreak { file_index: 1 })
    );
    // A payload change is caught the same way as an envelope change.
    let mut tampered = rows(0..10);
    tampered[3].payload = BuildPayload::Whole(VariantValue::Int(-1));
    let rebuilt = build_hef_file(tampered, &audit_config(AUDIT_GENESIS)).unwrap();
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[with_chain_end(&rebuilt, trail(&first).chain_end)]),
        Err(AuditChainError::RowsAltered { file_index: 0 })
    );
}

#[test]
fn a_dropped_row_is_detected() {
    let [first, second, _] = three_files();
    let mut dropped = rows(0..10);
    dropped.remove(4);
    let rebuilt = build_hef_file(dropped, &audit_config(AUDIT_GENESIS)).unwrap();

    let forged = with_chain_end(&rebuilt, trail(&first).chain_end);
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[forged]),
        Err(AuditChainError::RowsAltered { file_index: 0 })
    );
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[open(&rebuilt), open(&second)]),
        Err(AuditChainError::ChainBreak { file_index: 1 })
    );
}

#[test]
fn a_reordered_missing_or_foreign_file_is_detected() {
    let [first, second, third] = three_files();
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[open(&first), open(&third), open(&second)]),
        Err(AuditChainError::ChainBreak { file_index: 1 })
    );
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[open(&second), open(&first), open(&third)]),
        Err(AuditChainError::ChainBreak { file_index: 0 })
    );
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[open(&first), open(&third)]),
        Err(AuditChainError::ChainBreak { file_index: 1 })
    );
    let event_file = build_hef_file(rows(0..3), &source_form_config(4)).unwrap();
    assert_eq!(
        verify_audit_files(AUDIT_GENESIS, &[open(&first), open(&event_file)]),
        Err(AuditChainError::NotAudit { file_index: 1 })
    );
}

#[test]
fn a_reader_from_before_audit_files_refuses_one_and_event_files_stay_readable() {
    let pre_audit_known: u64 = (1 << 29) - 1;
    let built = build_hef_file(rows(0..3), &audit_config(AUDIT_GENESIS)).unwrap();
    assert_ne!(built.footer.required_feature_flags & !pre_audit_known, 0);
    assert_eq!(required_features::ALL & required_features::AUDIT_HASH_CHAIN, 0);
    let event_file = build_hef_file(rows(0..3), &source_form_config(4)).unwrap();
    assert_eq!(event_file.footer.required_feature_flags & !pre_audit_known, 0);
    assert_eq!(event_file.footer.audit, None);
}

/// The chain's hashes are part of the format: a reader verifying a file written by an older build must recompute the
/// same hashes, so the hash of a fixed run of rows covering every payload kind is pinned.
#[test]
fn the_chain_hash_of_a_fixed_run_of_rows_is_pinned() {
    let built = build_hef_file(rows(0..28), &audit_config(AUDIT_GENESIS)).unwrap();
    let hex: String = trail(&built)
        .chain_end
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(hex, "ec86e17f82cffca034f9215649764f95e75cd09687e10cea62ebd3fdc51d2a0e");
}

/// Checks a run of eight audit files of 12,500 rows each with `verify_audit_files`, seven times, and prints the median
/// time with the rows and file megabytes checked per second. Opening each file against its seal is not timed. Run by
/// hand: `cargo test --release --all-features --lib audit_chain_verification_throughput -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode audit chain verification benchmark"]
fn audit_chain_verification_throughput() {
    const FILES: u64 = 8;
    const ROWS_PER_FILE: u64 = 12_500;
    let mut built = Vec::new();
    let mut chain_end = AUDIT_GENESIS;
    for file in 0..FILES {
        let config = HefBuildConfig {
            targets: LayoutTargets::default(),
            ..audit_config(chain_end)
        };
        let next = build_hef_file(rows(file * ROWS_PER_FILE..(file + 1) * ROWS_PER_FILE), &config).unwrap();
        chain_end = trail(&next).chain_end;
        built.push(next);
    }
    let mut samples = Vec::new();
    for _ in 0..7 {
        let opened: Vec<HefFile> = built.iter().map(open).collect();
        let started = Instant::now();
        let verified = verify_audit_files(AUDIT_GENESIS, &opened);
        samples.push(started.elapsed().as_secs_f64());
        assert_eq!(verified, Ok(chain_end));
    }
    samples.sort_unstable_by(f64::total_cmp);
    let median = samples[3];
    let rows = FILES * ROWS_PER_FILE;
    let file_bytes: usize = built.iter().map(|file| file.bytes.len()).sum();
    println!(
        "audit chain verification, {FILES} files, {rows} rows, {file_bytes} bytes: median {:.1} ms, {:.0} rows/s, \
         {:.1} MB/s",
        median * 1e3,
        rows as f64 / median,
        file_bytes as f64 / median / 1e6
    );
}
