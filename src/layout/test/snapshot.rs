use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::benchmarks::{SnapshotLookupMeasurement, snapshot_lookup_gate_passes};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, SequenceRange, StreamId, TenantId, TimestampValue};
use crate::file::{FileError, RangeSource};
use crate::layout::reader::HeaderCommitment;
use crate::layout::{HEADER_BLOCK_LEN, LayoutTargets};
use crate::lifecycle::{FileType, HefFileEntry, PartState};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const TENANT: u128 = 11;

/// A remote object that records every range asked of it.
struct CountingSource {
    object: Vec<u8>,
    requests: Mutex<Vec<(u64, u64)>>,
}

impl RangeSource for CountingSource {
    fn read_range(&self, _object: u128, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        self.requests.lock().unwrap().push((offset, len));
        self.object
            .get(offset as usize..(offset + len) as usize)
            .map(<[u8]>::to_vec)
            .ok_or(FileError::OutOfBounds)
    }
}

fn row(sequence: u64, key: &str) -> HefRow {
    HefRow {
        epoch: 7,
        sequence,
        event: EventInput {
            envelope: EventEnvelope {
                account_id: None,
                account_id_hash_low: 0,
                actor_id: None,
                actor_id_hash_low: 0,
                dedupe_hash_high: 0,
                dedupe_hash_low: sequence,
                entity_id: Some(key.to_owned()),
                entity_id_hash_high: 0,
                entity_id_hash_low: sequence,
                entity_type: "device".to_owned(),
                event_id: EventId::new_test_id(0xD0_0000 + u128::from(sequence)),
                event_type: "snapshot".to_owned(),
                flags: EventFlags(0),
                ingested_at: TimestampValue::from_physical_nanos(1),
                occurred_at: TimestampValue::from_physical_nanos(1),
                schema_version: 1,
                source: "matrix".to_owned(),
                stream_id: StreamId(1),
                stream_sequence: sequence,
                tenant_id: TenantId::new_test_id(TENANT),
                trace_id_hash_low: 0,
            },
            payload: PayloadInput::Variant(value(key)),
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: 0,
            connector_delivery_hash_high: 0,
            provenance: None,
            relationships: None,
        },
    }
}

fn value(key: &str) -> VariantValue {
    VariantValue::Object(BTreeMap::from([
        ("key".to_owned(), VariantValue::String(key.to_owned())),
        ("len".to_owned(), VariantValue::Int(key.len() as i64)),
    ]))
}

fn config(index_granularity: usize) -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        audit: None,
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        graph: None,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        profile: Profile::Snapshot,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity,
            ..LayoutTargets::default()
        },
        tenant_id: TenantId::new_test_id(TENANT),
    }
}

/// A snapshot of `keys`, which must already be sorted, one row each, in granules of `index_granularity` rows.
fn snapshot(keys: &[String], index_granularity: usize) -> BuiltHef {
    let rows = keys
        .iter()
        .enumerate()
        .map(|(index, key)| row(index as u64 + 1, key))
        .collect();
    build_hef_file(rows, &config(index_granularity)).unwrap()
}

/// Matrix-style user ids: every key shares the leading `@user` and differs within the model's eight bytes.
fn user_keys(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("@user{i:06}:example.org")).collect()
}

/// Device keys whose first eight bytes past the shared prefix repeat for 300 rows at a time, so one model key covers
/// runs several granules long.
fn device_keys() -> Vec<String> {
    (0..4)
        .flat_map(|user| (0..300).map(move |device| format!("@u{user}:example.org|DEVICE{device:04}")))
        .collect()
}

fn local(built: &BuiltHef) -> HefFile {
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap()
}

fn counting_source(built: &BuiltHef) -> Arc<CountingSource> {
    Arc::new(CountingSource {
        object: built.bytes.clone(),
        requests: Mutex::new(Vec::new()),
    })
}

/// Opens `built` from `source` as a remote reader would: one request for the tail.
fn open_remote(source: &Arc<CountingSource>, built: &BuiltHef) -> HefFile {
    let entry = HefFileEntry {
        coverage: SequenceRange {
            epoch: 7,
            first_sequence: 1,
            last_sequence: built.footer.exact_counts.row_count,
        },
        feature_metadata: None,
        file_seal: built.file_seal,
        file_id: built.file_id,
        file_type: FileType::HefFile,
        footer_len: Some(built.footer_len),
        optional_feature_flags: built.footer.optional_feature_flags,
        part_index: 0,
        part_state: PartState::Active,
        profile: Profile::Snapshot,
        required_feature_flags: built.footer.required_feature_flags,
        size_bytes: built.bytes.len() as u64,
        tenant_id: TenantId::new_test_id(TENANT),
        tree_len: built.tree_len,
    };
    let header = HeaderCommitment::from_header_block(&built.bytes[..HEADER_BLOCK_LEN]).unwrap();
    let range_source: Arc<dyn RangeSource> = source.clone();
    HefFile::open_remote(range_source, None, &entry, &header, None).unwrap()
}

#[test]
fn every_stored_key_returns_its_own_value_and_absent_keys_return_none() {
    let cases: [(Vec<String>, &[&str]); 2] = [
        (
            user_keys(2_000),
            &["@user", "@user000000:example.orf", "@user999999:x", "@other"],
        ),
        (
            device_keys(),
            &["@u0:example.org|DEVICE0300", "@u2:example.org|DEVICE", "@u4", ""],
        ),
    ];
    for (keys, absent) in cases {
        let built = snapshot(&keys, 64);
        let index = built.footer.snapshot_keys.as_ref().unwrap();
        assert_eq!(index.granule_first_keys.len(), built.footer.granules.len());
        let file = local(&built);
        for key in &keys {
            assert_eq!(
                file.snapshot_get(key).unwrap(),
                Some(PayloadRead::Value(value(key))),
                "{key}"
            );
        }
        for key in absent {
            assert_eq!(file.snapshot_get(key).unwrap(), None, "{key}");
        }
    }
}

/// Keys whose model bytes repeat for a run longer than a granule widen the window by the run; the granules' first keys
/// then pick the one holding the key (the lookups themselves are checked above).
#[test]
fn a_run_of_keys_sharing_one_model_key_widens_the_window_by_the_run() {
    let built = snapshot(&device_keys(), 64);
    let index = built.footer.snapshot_keys.as_ref().unwrap();
    assert_eq!(index.common_prefix, b"@u");
    assert_eq!(index.longest_run, 300);
}

/// The acceptance bound: a remote open reads the tail once, and each lookup then reads one granule more, whether
/// the key is present or not, which holds every cold lookup to the snapshot lookup gate.
#[test]
fn a_remote_lookup_costs_one_tail_read_and_one_granule_read() {
    for (keys, absent) in [
        (user_keys(2_000), "@user000123:example.orz"),
        (device_keys(), "@u1:example.org|DEVICE01999"),
    ] {
        let built = snapshot(&keys, 64);
        assert!(built.footer.stripes.len() > 1);
        assert_eq!(
            built.footer.stripes.len(),
            built.footer.granules.len(),
            "a snapshot gives every granule its own stripe"
        );
        let source = counting_source(&built);
        let opened = open_remote(&source, &built);
        assert_eq!(source.requests.lock().unwrap().len(), 1, "the open reads only the tail");
        let tail_bytes = source.requests.lock().unwrap()[0].1;
        let cold_lookup = |requests: &[(u64, u64)]| SnapshotLookupMeasurement {
            bytes_fetched: tail_bytes + requests.iter().map(|(_, len)| len).sum::<u64>(),
            requests: 1 + requests.len() as u32,
            stored_bytes: built.bytes.len() as u64,
        };
        let mut measurements = Vec::new();

        for key in keys.iter().step_by(37) {
            let file = opened.fresh_reader().unwrap();
            let before = source.requests.lock().unwrap().len();
            assert_eq!(file.snapshot_get(key).unwrap(), Some(PayloadRead::Value(value(key))));
            let requests = source.requests.lock().unwrap().clone();
            assert_eq!(requests.len() - before, 1, "lookup for {key}");
            let (offset, len) = requests[before];
            assert!(
                built
                    .footer
                    .stripes
                    .iter()
                    .any(|stripe| stripe.file_offset == offset && stripe.byte_len == len),
                "the one read is exactly one granule's stripe"
            );
            measurements.push(cold_lookup(&requests[before..]));
        }
        let file = opened.fresh_reader().unwrap();
        let before = source.requests.lock().unwrap().len();
        assert_eq!(file.snapshot_get(absent).unwrap(), None);
        let requests = source.requests.lock().unwrap().clone();
        assert!(requests.len() - before <= 1);
        measurements.push(cold_lookup(&requests[before..]));

        let largest = measurements.iter().max_by_key(|m| m.bytes_fetched).unwrap();
        println!(
            "snapshot lookup: {} keys, at most {} requests and {} of {} B fetched per cold lookup",
            keys.len(),
            largest.requests,
            largest.bytes_fetched,
            largest.stored_bytes
        );
        assert!(measurements.iter().all(snapshot_lookup_gate_passes));
    }
}

#[test]
fn a_lookup_refuses_a_file_that_is_not_a_snapshot() {
    let mut event = config(64);
    event.profile = Profile::Event;
    let built = build_hef_file(vec![row(1, "a")], &event).unwrap();
    assert!(local(&built).snapshot_get("a").is_err());
}

#[test]
fn the_index_round_trips_and_refuses_damaged_bytes() {
    let keys = user_keys(500);
    let borrowed: Vec<&str> = keys.iter().map(String::as_str).collect();
    let index = SnapshotKeyIndex::build(&borrowed, &[0, 200, 400]).unwrap();
    let bytes = index.encode();
    assert_eq!(SnapshotKeyIndex::decode(&bytes).unwrap(), index);
    assert!(SnapshotKeyIndex::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(SnapshotKeyIndex::decode(&trailing).is_err());
}

#[test]
fn the_index_refuses_keys_out_of_order_or_repeated() {
    assert!(SnapshotKeyIndex::build(&["b", "a"], &[0]).is_err());
    assert!(SnapshotKeyIndex::build(&["a", "a"], &[0]).is_err());
    assert!(SnapshotKeyIndex::build(&["a", "b"], &[0]).is_ok());
}

/// One entry of the per-partition rkyv archive a snapshot file replaces: a key and its value as JSON text.
#[derive(rkyv::Archive, rkyv::Serialize)]
struct ArchiveEntry {
    key: String,
    value: String,
}

/// Compares point lookups in a 100,000-key snapshot file against the per-partition rkyv archive it replaces, and
/// prints stored bytes, requests and bytes fetched per cold lookup, and time per lookup. The rkyv archive is read
/// whole into aligned memory, as a mapped file is, and validated once per open. Run by hand:
/// `cargo test --release --all-features --lib snapshot_lookup_against_rkyv -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode snapshot lookup benchmark"]
fn snapshot_lookup_against_rkyv() {
    let keys = user_keys(100_000);
    let built = snapshot(&keys, 8192);
    let probes: Vec<&String> = keys.iter().step_by(997).collect();
    let count = probes.len() as u32;
    let warm_rounds = 100u32;

    let source = counting_source(&built);
    let started = std::time::Instant::now();
    for key in &probes {
        let file = open_remote(&source, &built);
        assert_eq!(file.snapshot_get(key).unwrap(), Some(PayloadRead::Value(value(key))));
    }
    let hef_cold = started.elapsed() / count;
    let requests = source.requests.lock().unwrap().clone();
    let hef_gets = requests.len() as f64 / f64::from(count);
    let hef_bytes = requests.iter().map(|(_, len)| len).sum::<u64>() / u64::from(count);
    let tail_bytes = requests[0].1;

    // Warm: one open local reader, every probed granule's keys already decoded by a first pass.
    let file = local(&built);
    for key in &probes {
        file.snapshot_get(key).unwrap();
    }
    let started = std::time::Instant::now();
    for _ in 0..warm_rounds {
        for key in &probes {
            std::hint::black_box(file.snapshot_get(key).unwrap());
        }
    }
    let hef_warm = started.elapsed() / (count * warm_rounds);

    let entries: Vec<ArchiveEntry> = keys
        .iter()
        .map(|key| ArchiveEntry {
            key: key.clone(),
            value: format!("{{\"key\":\"{key}\",\"len\":{}}}", key.len()),
        })
        .collect();
    let archive = rkyv::to_bytes::<rkyv::rancor::Error>(&entries).unwrap();
    let lookup = |archived: &rkyv::Archived<Vec<ArchiveEntry>>, key: &str| {
        archived
            .binary_search_by(|entry| entry.key.as_str().cmp(key))
            .ok()
            .and_then(|at| archived.get(at))
            .map(|entry| entry.value.as_str().len())
    };
    let started = std::time::Instant::now();
    for key in &probes {
        let mut mapped = rkyv::util::AlignedVec::<16>::with_capacity(archive.len());
        mapped.extend_from_slice(&archive);
        let archived = rkyv::access::<rkyv::Archived<Vec<ArchiveEntry>>, rkyv::rancor::Error>(&mapped).unwrap();
        assert!(lookup(archived, key).is_some());
    }
    let rkyv_cold = started.elapsed() / count;
    let archived = rkyv::access::<rkyv::Archived<Vec<ArchiveEntry>>, rkyv::rancor::Error>(&archive).unwrap();
    let started = std::time::Instant::now();
    for _ in 0..warm_rounds {
        for key in &probes {
            std::hint::black_box(lookup(archived, key));
        }
    }
    let rkyv_warm = started.elapsed() / (count * warm_rounds);

    println!("keys: {}, probes: {count}", keys.len());
    println!(
        "hef  snapshot: {} B stored, {hef_gets:.2} GETs and {hef_bytes} B fetched per cold lookup \
         ({tail_bytes} B tail), cold open+lookup {hef_cold:?}, warm lookup {hef_warm:?}",
        built.bytes.len()
    );
    println!(
        "rkyv archive:  {} B stored, 1 GET and {} B fetched per cold lookup, \
         cold copy+validate+lookup {rkyv_cold:?}, warm lookup {rkyv_warm:?}",
        archive.len(),
        archive.len()
    );
}
