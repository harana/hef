use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::artifacts::state_chunk::{SealedStateChunk, StateChunk, build_state_tree, decode_state_chunk};
use crate::benchmarks::{
    GraphWalkMeasurement, StateLookupMeasurement, graph_walk_gate_passes, state_lookup_gate_passes,
};
use crate::clock::FixedClock;
use crate::columns::{FreetextDeclaration, PromotionPlan, column_ids};
use crate::error::StorageError;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, SequenceRange, StreamId, TimestampValue};
use crate::file::constant::CHUNK_GROUP_BYTES;
use crate::layout::cache::CacheStore;
use crate::layout::graph::tests::room_graph;
use crate::layout::graph::{ChainCoverInput, GraphInput, RowEdges};
use crate::layout::reader::PayloadRead;
use crate::layout::{HEADER_BLOCK_LEN, LayoutTargets, Profile, required_features};
use crate::lifecycle::{FileType, PartState};
use crate::security::TenantDek;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};
use crate::writer::source_form::tests::{source_form_config, source_form_row};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap as StdBTreeMap, HashMap as StdHashMap};
use std::sync::{
    Mutex,
    mpsc::{Receiver, channel},
};
use std::time::Instant;

const TENANT: u128 = 3;

/// A remote object that records every range asked of it.
struct CountingSource {
    object: Vec<u8>,
    requests: Mutex<Vec<(u64, u64)>>,
}

impl CountingSource {
    fn new(object: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            object: object.to_vec(),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<(u64, u64)> {
        self.requests.lock().unwrap().clone()
    }
}

impl RangeSource for CountingSource {
    fn read_range(&self, _object: u128, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        self.requests.lock().unwrap().push((offset, len));
        let end = offset.checked_add(len).ok_or(FileError::OutOfBounds)?;
        self.object
            .get(offset as usize..end as usize)
            .map(<[u8]>::to_vec)
            .ok_or(FileError::OutOfBounds)
    }
}

/// An application-backed disk tier held in memory, so a test can read and corrupt what the cache stored.
#[derive(Default)]
struct DiskDouble {
    entries: Mutex<StdHashMap<BlockKey, Vec<u8>>>,
}

impl CacheStore for DiskDouble {
    fn get(&self, key: &BlockKey) -> Result<Option<Vec<u8>>, FileError> {
        Ok(self.entries.lock().unwrap().get(key).cloned())
    }

    fn put(&self, key: &BlockKey, bytes: &[u8]) -> Result<(), FileError> {
        self.entries.lock().unwrap().insert(*key, bytes.to_vec());
        Ok(())
    }

    fn remove(&self, key: &BlockKey) -> Result<(), FileError> {
        self.entries.lock().unwrap().remove(key);
        Ok(())
    }
}

fn noise(seed: u64, len: usize) -> String {
    let mut state = seed + 1;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            char::from(33 + ((state >> 32) % 94) as u8)
        })
        .collect()
}

fn row(i: u64, payload: StdBTreeMap<String, VariantValue>) -> HefRow {
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xF00D_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(TENANT),
                stream_id: StreamId(1),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000 + i as i64),
                ingested_at: TimestampValue::from_physical_nanos(2_000 + i as i64),
                source: "crm".to_owned(),
                event_type: "deal.updated".to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: i,
                entity_id_hash_high: 1,
                entity_id: None,
                actor_id_hash_low: 3,
                actor_id: None,
                account_id_hash_low: 4,
                account_id: None,
                trace_id_hash_low: 5,
                dedupe_hash_low: i,
                dedupe_hash_high: 6,
                schema_version: 1,
                flags: EventFlags(0),
            },
            payload: PayloadInput::Variant(VariantValue::Object(payload)),
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: i,
            connector_delivery_hash_high: 0,
            provenance: None,
            relationships: None,
        },
    }
}

fn config(index_granularity: usize, granule_bytes: usize, stripe_target_bytes: usize) -> HefBuildConfig {
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
        profile: crate::layout::Profile::Event,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity,
            index_granularity_bytes: granule_bytes,
            stripe_target_bytes,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(TENANT),
        time_series_bucket_nanos: None,
    }
}

/// One granule in one multi-megabyte stripe, so the stripe carries an outboard proof tree. Every eighth row's `blob`
/// is an integer and the rest are 64 KiB of incompressible text, so `blob` is too mixed to shred and its values make
/// the granule's residual arena several proof leaves long.
fn large_file() -> BuiltHef {
    let rows: Vec<_> = (0..64u64)
        .map(|i| {
            let blob = if i.is_multiple_of(8) {
                VariantValue::Int(i as i64)
            } else {
                VariantValue::String(noise(i, 64 * 1024))
            };
            row(
                i,
                StdBTreeMap::from([
                    ("amount".to_owned(), VariantValue::Int(i as i64)),
                    ("blob".to_owned(), blob),
                ]),
            )
        })
        .collect();
    let built = build_hef_file(rows, &config(64, 64 << 20, 64 << 20)).unwrap();
    assert_eq!(built.footer.stripes.len(), 1);
    assert!(built.tree_len.is_some(), "a multi-megabyte stripe carries proof nodes");
    built
}

/// Small stripes, one granule of four rows each.
fn multi_stripe_file(footer_dek: Option<[u8; 32]>) -> BuiltHef {
    let rows: Vec<_> = (0..40u64)
        .map(|i| {
            row(
                i,
                StdBTreeMap::from([
                    ("amount".to_owned(), VariantValue::Int(i as i64)),
                    ("note".to_owned(), VariantValue::String(format!("note {i}"))),
                ]),
            )
        })
        .collect();
    let mut config = config(4, 1 << 20, 1);
    if let Some(dek) = footer_dek {
        config.footer_encryption = crate::security::FooterEncryption::Encrypted;
        config.footer_dek = Some(dek);
    }
    let built = build_hef_file(rows, &config).unwrap();
    assert!(built.footer.stripes.len() > 1);
    built
}

fn entry(built: &BuiltHef) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: built.footer.exact_counts.row_count,
        },
        feature_metadata: None,
        file_seal: built.file_seal,
        file_id: built.file_id,
        file_type: FileType::HefFile,
        footer_len: Some(built.footer_len),
        graph_event_id_filter: None,
        graph_summary: None,
        optional_feature_flags: built.footer.optional_feature_flags,
        part_index: 0,
        part_state: PartState::Active,
        profile: built.footer.profile,
        required_feature_flags: built.footer.required_feature_flags,
        size_bytes: built.bytes.len() as u64,
        tenant_id: TenantId::new_test_id(TENANT),
        tree_len: built.tree_len,
    }
}

fn open(
    source: &Arc<CountingSource>,
    cache: Option<&Arc<BlockCache>>,
    built: &BuiltHef,
    footer_dek: Option<&[u8; 32]>,
) -> HefFile {
    let source: Arc<dyn RangeSource> = source.clone();
    let header = HeaderCommitment::from_header_block(&built.bytes[..HEADER_BLOCK_LEN]).unwrap();
    HefFile::open_remote(source, cache.cloned(), &entry(built), &header, footer_dek).unwrap()
}

fn local(built: &BuiltHef, footer_dek: Option<&[u8; 32]>) -> HefFile {
    HefFile::open_with_keys(built.bytes.clone(), Some(&built.file_seal), footer_dek).unwrap()
}

fn intersects((offset, len): (u64, u64), start: u64, end: u64) -> bool {
    offset < end && start < offset + len
}

/// With `footer_len` in the manifest entry the open is one exact tail request, and a cold read of one column block
/// then costs at most two more (the stripe's marks pages, then the block), none of them the whole stripe.
#[test]
fn a_cold_point_read_costs_at_most_two_requests_once_the_footer_length_is_known() {
    let built = large_file();
    let source = CountingSource::new(&built.bytes);
    let file = open(&source, None, &built, None);
    assert_eq!(source.requests().len(), 1, "an exact tail open is a single request");

    let granule = built.footer.granules[0].granule_id;
    let read = file.read_column(column_ids::SEQUENCE, granule).unwrap();
    let requests = source.requests();
    assert!(
        requests.len() - 1 <= 2,
        "a cold point read issued {} requests",
        requests.len() - 1
    );
    let stripe = &built.footer.stripes[0];
    assert!(requests[1..].iter().all(|(_, len)| *len < stripe.byte_len));

    let expected = local(&built, None).read_column(column_ids::SEQUENCE, granule).unwrap();
    assert_eq!(read.data, expected.data);
    assert_eq!(read.presence, expected.presence);
}

/// Reading a granule in one stripe never asks for a byte of any other stripe, its marks pages included.
#[test]
fn a_pruned_stripe_is_never_fetched() {
    let built = multi_stripe_file(None);
    let source = CountingSource::new(&built.bytes);
    let file = open(&source, None, &built, None);
    let surviving = built.footer.granules[0];
    file.read_column(column_ids::SEQUENCE, surviving.granule_id).unwrap();

    let requests = source.requests();
    assert!(requests.len() - 1 <= 2);
    for stripe in built
        .footer
        .stripes
        .iter()
        .filter(|stripe| stripe.stripe_id != surviving.stripe_id)
    {
        let end = stripe.file_offset + stripe.byte_len;
        assert!(
            !requests
                .iter()
                .any(|request| intersects(*request, stripe.file_offset, end)),
            "stripe {} was pruned but fetched",
            stripe.stripe_id
        );
    }
}

/// A cold single-row payload read through the remote reader returns what the in-memory reader returns, and fetches
/// the proof leaves around that row's residual slot rather than the granule's whole residual arena.
#[test]
fn a_cold_single_row_payload_read_fetches_only_that_rows_residual_slot() {
    let built = large_file();
    let payload = built.footer.payload_granules[0];
    assert!(
        payload.residual_len > 2 * CHUNK_GROUP_BYTES as u64,
        "the arena spans several proof leaves"
    );
    let stripe = &built.footer.stripes[0];
    let base = if built.footer.required_feature_flags & required_features::STRIPE_RELATIVE_MARKS != 0 {
        stripe.file_offset
    } else {
        0
    };
    let residual_start = base + payload.residual_offset;
    let residual_end = residual_start + payload.residual_len;

    let source = CountingSource::new(&built.bytes);
    let file = open(&source, None, &built, None);
    let read = file.payload(1).unwrap();
    assert!(matches!(read, PayloadRead::Value(_)));
    assert_eq!(read, local(&built, None).payload(1).unwrap());
    assert!(
        source
            .requests()
            .iter()
            .all(|(offset, len)| !(*offset <= residual_start && residual_end <= offset + len)),
        "no request may fetch the whole residual arena"
    );
}

/// A second reader of the same file sharing the cache opens and reads with no request at all.
#[test]
fn a_second_open_through_a_shared_cache_issues_no_requests() {
    let built = large_file();
    let source = CountingSource::new(&built.bytes);
    let cache = Arc::new(BlockCache::new(64 << 20, None));
    let granule = built.footer.granules[0].granule_id;

    let first = open(&source, Some(&cache), &built, None);
    let first_column = first.read_column(column_ids::SEQUENCE, granule).unwrap();
    let first_payload = first.payload(1).unwrap();
    let after_first = source.requests().len();

    let second = open(&source, Some(&cache), &built, None);
    let second_column = second.read_column(column_ids::SEQUENCE, granule).unwrap();
    assert_eq!(second.payload(1).unwrap(), first_payload);
    assert_eq!(second_column.data, first_column.data);
    assert_eq!(
        source.requests().len(),
        after_first,
        "the second open was served from the cache"
    );
}

/// A cached piece that no longer proves (here a flipped byte on the disk tier) is dropped, fetched again from the
/// source, and replaced; the read still returns the right answer.
#[test]
fn a_corrupted_cached_piece_is_rejected_and_fetched_again() {
    let built = multi_stripe_file(None);
    let source = CountingSource::new(&built.bytes);
    let disk = Arc::new(DiskDouble::default());
    // A zero memory budget keeps every piece on the disk tier, where the test can reach it.
    let cache = Arc::new(BlockCache::new(0, Some(disk.clone())));
    let granule = built.footer.granules[0].granule_id;

    let expected = open(&source, Some(&cache), &built, None)
        .read_column(column_ids::SEQUENCE, granule)
        .unwrap();
    let kinds: Vec<BlockKind> = disk.entries.lock().unwrap().keys().map(|key| key.kind).collect();
    assert!(kinds.contains(&BlockKind::Footer) && kinds.contains(&BlockKind::Stripe));
    for bytes in disk.entries.lock().unwrap().values_mut() {
        bytes[0] ^= 0xFF;
    }
    let before = source.requests().len();

    let read = open(&source, Some(&cache), &built, None)
        .read_column(column_ids::SEQUENCE, granule)
        .unwrap();
    assert_eq!(read.data, expected.data);
    assert!(
        source.requests().len() >= before + 2,
        "both the tail and the stripe piece were fetched again"
    );
    for (key, bytes) in disk.entries.lock().unwrap().iter() {
        assert_eq!(
            bytes.as_slice(),
            &built.bytes[key.offset as usize..][..key.len as usize],
            "the corrupt copy was replaced by the stored bytes"
        );
    }
}

/// Reads through a cache too small to hold the file return exactly what the in-memory reader returns, and the cache
/// stays within its budget.
#[test]
fn eviction_never_changes_results() {
    let built = multi_stripe_file(None);
    let source = CountingSource::new(&built.bytes);
    let budget = 4096;
    let cache = Arc::new(BlockCache::new(budget, None));
    let expected = local(&built, None);

    for _ in 0..2 {
        let file = open(&source, Some(&cache), &built, None);
        for granule in &built.footer.granules {
            assert_eq!(
                file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap().data,
                expected
                    .read_column(column_ids::SEQUENCE, granule.granule_id)
                    .unwrap()
                    .data
            );
        }
        for row in 0..built.footer.exact_counts.row_count {
            assert_eq!(file.payload(row).unwrap(), expected.payload(row).unwrap(), "row {row}");
        }
        assert!(cache.memory_bytes() <= budget);
    }
}

/// A file whose footer is sealed opens remotely with its key, and the cache only ever holds the object's stored
/// bytes: the sealed footer stays ciphertext there.
#[test]
fn a_sealed_footer_is_cached_only_in_its_stored_form() {
    let dek = [7u8; 32];
    let built = multi_stripe_file(Some(dek));
    let source = CountingSource::new(&built.bytes);
    let disk = Arc::new(DiskDouble::default());
    let cache = Arc::new(BlockCache::new(1 << 20, Some(disk.clone())));

    let file = open(&source, Some(&cache), &built, Some(&dek));
    assert_eq!(file.payload(0).unwrap(), local(&built, Some(&dek)).payload(0).unwrap());

    let plaintext_footer = crate::layout::footer::encode_footer(&built.footer);
    let entries = disk.entries.lock().unwrap();
    assert!(entries.keys().any(|key| key.kind == BlockKind::Footer));
    for (key, bytes) in entries.iter() {
        assert_eq!(
            bytes.as_slice(),
            &built.bytes[key.offset as usize..][..key.len as usize]
        );
        assert!(
            !bytes
                .windows(plaintext_footer.len().min(64))
                .any(|window| window == &plaintext_footer[..plaintext_footer.len().min(64)]),
            "no plaintext footer bytes may reach the cache"
        );
    }
}

/// A fresh reader made from a remote one reads the same values without the first reader's fetched ranges.
#[test]
fn a_fresh_reader_of_a_remote_file_reads_the_same_values() {
    let built = multi_stripe_file(None);
    let source = CountingSource::new(&built.bytes);
    let file = open(&source, None, &built, None);
    let first = file.payload(5).unwrap();
    let fresh = file.fresh_reader().unwrap();
    assert_eq!(fresh.payload(5).unwrap(), first);
    assert_eq!(fresh.header().file_id, built.file_id);
    assert_eq!(fresh.header().row_count, built.header.row_count);
}

/// A remote object that records each batch of ranges asked of it, and each range asked on its own, and can hold its
/// first batch back until released.
struct BatchSource {
    batches: Mutex<Vec<Vec<ByteRange>>>,
    hold_first: Mutex<Option<Receiver<()>>>,
    object: Vec<u8>,
    singles: Mutex<Vec<(u64, u64)>>,
}

impl BatchSource {
    fn new(object: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            batches: Mutex::new(Vec::new()),
            hold_first: Mutex::new(None),
            object: object.to_vec(),
            singles: Mutex::new(Vec::new()),
        })
    }

    fn batches(&self) -> Vec<Vec<ByteRange>> {
        self.batches.lock().unwrap().clone()
    }

    fn singles(&self) -> usize {
        self.singles.lock().unwrap().len()
    }

    fn bytes(&self, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        self.object
            .get(offset as usize..(offset + len) as usize)
            .map(<[u8]>::to_vec)
            .ok_or(FileError::OutOfBounds)
    }
}

impl RangeSource for BatchSource {
    fn read_range(&self, _object: u128, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        self.singles.lock().unwrap().push((offset, len));
        self.bytes(offset, len)
    }

    fn read_ranges(&self, _object: u128, ranges: &[ByteRange]) -> Result<Vec<Vec<u8>>, FileError> {
        self.batches.lock().unwrap().push(ranges.to_vec());
        let gate = self.hold_first.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.recv().ok();
        }
        ranges.iter().map(|range| self.bytes(range.offset, range.len)).collect()
    }
}

/// Events `first_sn..first_sn + rows` of subject `room`, each pointing back along `prev` at the events `offsets`
/// before it that exist, and authorized by event 0.
fn graph_input(first_sn: u64, rows: u64, offsets: &[u64]) -> GraphInput {
    GraphInput {
        chain_cover: None,
        depth: None,
        first_sn,
        inbound: Vec::new(),
        rows: (first_sn..first_sn + rows)
            .map(|sn| RowEdges {
                auth: if sn > 0 { vec![0] } else { Vec::new() },
                prev: offsets.iter().filter_map(|offset| sn.checked_sub(*offset)).collect(),
            })
            .collect(),
        state_roots: StdBTreeMap::new(),
        subject: b"room".to_vec(),
        unresolved: Vec::new(),
    }
}

/// A graph file of `rows` small rows from sequence number `first_sn`, four rows per granule.
fn graph_file(first_sn: u64, rows: u64, offsets: &[u64]) -> BuiltHef {
    graph_file_of(graph_input(first_sn, rows, offsets), 4)
}

/// The protocol id of event `sn`.
fn event_id(sn: u64) -> Vec<u8> {
    format!("$event{sn}:example.org").into_bytes()
}

/// A graph file holding `graph`, one small row per event, four rows per granule, each row carrying its protocol id.
fn build_graph(graph: GraphInput) -> BuiltHef {
    graph_file_of(graph, 4)
}

/// A graph file holding `graph`'s events as small rows, `rows_per_granule` to a granule, each row carrying its
/// protocol id.
fn graph_file_of(graph: GraphInput, rows_per_granule: usize) -> BuiltHef {
    let mut config = source_form_config(rows_per_granule);
    let sns = graph.first_sn..graph.first_sn + graph.rows.len() as u64;
    config.graph = Some(graph);
    config.profile = Profile::Graph;
    let rows: Vec<_> = sns
        .map(|i| {
            let mut row = source_form_row(i, VariantValue::Int(i as i64));
            row.external_id = Some(event_id(i));
            row
        })
        .collect();
    build_hef_file(rows, &config).unwrap()
}

fn open_graph_file(source: &Arc<BatchSource>, built: &BuiltHef) -> HefFile {
    let source: Arc<dyn RangeSource> = source.clone();
    let header = HeaderCommitment::from_header_block(&built.bytes[..HEADER_BLOCK_LEN]).unwrap();
    let entry = HefFileEntry {
        tenant_id: TenantId::new_test_id(7),
        ..entry(built)
    };
    HefFile::open_remote(source, None, &entry, &header, None).unwrap()
}

fn options(max_in_flight: usize) -> RemoteGraphOptions {
    RemoteGraphOptions {
        hedge_floor_nanos: 60_000_000_000,
        max_in_flight,
    }
}

fn remote_graph(files: Vec<HefFile>, options: RemoteGraphOptions) -> RemoteGraph {
    RemoteGraph::new(files, Arc::new(FixedClock::at(0)), options).unwrap()
}

/// Two files of one subject, events 0..32 and 32..64, and their sources.
fn two_files(offsets: &[u64]) -> (RemoteGraph, [Arc<BatchSource>; 2]) {
    two_graph_files([graph_input(0, 32, offsets), graph_input(32, 32, offsets)])
}

/// A remote graph over one file per input, and the files' sources.
fn two_graph_files(graphs: [GraphInput; 2]) -> (RemoteGraph, [Arc<BatchSource>; 2]) {
    let built = graphs.map(build_graph);
    let sources = [BatchSource::new(&built[0].bytes), BatchSource::new(&built[1].bytes)];
    let files = vec![
        open_graph_file(&sources[0], &built[0]),
        open_graph_file(&sources[1], &built[1]),
    ];
    assert!(built[0].footer.granules.len() > 1 && built[0].footer.stripes.len() > 1);
    (remote_graph(files, options(16)), sources)
}

#[test]
fn one_lookup_step_fetches_every_granule_it_needs_in_one_request_per_file() {
    let (graph, sources) = two_files(&[1]);
    let sns: Vec<u64> = (0..64).collect();
    let parents = graph.parents_of(b"room", &sns, EdgeKind::Prev).unwrap();
    let expected: Vec<Vec<u64>> = sns.iter().map(|sn| sn.checked_sub(1).into_iter().collect()).collect();
    assert_eq!(parents, expected);
    for source in &sources {
        let batches = source.batches();
        assert_eq!(batches.len(), 1, "one batch per file");
        assert_eq!(batches[0].len(), 1, "adjacent edge blocks join into one request");
    }

    let children = graph.children_of(b"room", &sns, EdgeKind::Auth).unwrap();
    assert_eq!(
        children[0],
        (1..32).collect::<Vec<_>>(),
        "children stay with the parent's file"
    );
    assert_eq!(graph.children(b"room", 40, EdgeKind::Prev).unwrap(), [41]);
    assert!(
        sources.iter().all(|source| source.batches().len() == 1),
        "held edge blocks are never fetched again"
    );
}

#[test]
fn a_walk_costs_at_most_one_request_per_file_per_level() {
    let (graph, sources) = two_files(&[1, 9]);
    let requests = || sources.iter().map(|source| source.batches().len()).sum::<usize>();
    let singles = || sources.iter().map(|source| source.singles()).sum::<usize>();
    let opened = singles();
    let mut seen = 0;
    let mut previous = requests();
    for level in graph.walk(b"room", 63, EdgeKind::Prev, 64) {
        let level = level.unwrap();
        let requests_now = requests();
        assert!(
            requests_now - previous <= 2,
            "a level costs at most one request per file"
        );
        for source in &sources {
            assert!(source.batches().iter().all(|batch| batch.len() == 1));
        }
        previous = requests_now;
        seen += level.len();
    }
    assert_eq!(seen, 63, "every ancestor of event 63 is reached once");
    assert!(requests() > 0);
    assert_eq!(singles(), opened, "no level fetches anything outside its batches");

    let again = previous;
    let levels: Vec<_> = graph.walk(b"room", 63, EdgeKind::Prev, 64).collect();
    assert!(levels.iter().all(Result::is_ok));
    assert_eq!(requests(), again, "a second walk is served from the held blocks");
}

/// A walk decodes each granule's edges once and keeps the decode, so walking the same graph again decodes nothing.
#[test]
fn a_second_walk_reuses_the_decoded_edges_of_the_first() {
    let (graph, _sources) = two_files(&[1, 9]);
    let walk = |graph: &RemoteGraph| {
        graph
            .walk(b"room", 63, EdgeKind::Prev, 64)
            .map(Result::unwrap)
            .collect::<Vec<_>>()
    };
    let first = walk(&graph);
    let held: Vec<u64> = graph.files.iter().map(HefFile::decoded_cache_bytes).collect();
    assert!(
        held.iter().all(|&bytes| bytes > 0),
        "every file holds its decoded edges"
    );
    let file = &graph.files[0];
    let granule = file.graph_granule_of(0).unwrap();
    assert!(Arc::ptr_eq(
        &file.cached_granule_edges(granule).unwrap(),
        &file.cached_granule_edges(granule).unwrap()
    ));
    assert_eq!(walk(&graph), first);
    let after: Vec<u64> = graph.files.iter().map(HefFile::decoded_cache_bytes).collect();
    assert_eq!(after, held, "the second walk decoded nothing new");
}

/// Under a budget smaller than one granule's edges a reader keeps only the granule it touched last, and a walk still
/// returns every ancestor.
#[test]
fn decoded_edges_stay_within_the_reader_budget() {
    let built = [graph_file(0, 32, &[1, 9]), graph_file(32, 32, &[1, 9])];
    let sources = [BatchSource::new(&built[0].bytes), BatchSource::new(&built[1].bytes)];
    let files = sources
        .iter()
        .zip(&built)
        .map(|(source, built)| open_graph_file(source, built).with_decoded_cache_budget(1))
        .collect();
    let graph = remote_graph(files, options(16));
    let seen: usize = graph
        .walk(b"room", 63, EdgeKind::Prev, 64)
        .map(|level| level.unwrap().len())
        .sum();
    assert_eq!(seen, 63);
    for file in &graph.files {
        let largest = file
            .footer()
            .granules
            .iter()
            .map(|granule| file.granule_edges(granule.granule_id).unwrap().decoded_bytes())
            .max()
            .unwrap();
        assert!(file.decoded_cache_bytes() <= largest);
    }
}

/// Requests and bytes every source has been asked for so far.
fn fetched(sources: &[Arc<BatchSource>]) -> (u64, u64) {
    let ranges: Vec<ByteRange> = sources.iter().flat_map(|source| source.batches()).flatten().collect();
    (ranges.len() as u64, ranges.iter().map(|range| range.len).sum())
}

/// Opens each of `built` behind a source of its own and serves them as one remote graph.
fn remote_graph_of(built: &[BuiltHef]) -> (RemoteGraph, Vec<Arc<BatchSource>>) {
    let sources: Vec<_> = built.iter().map(|built| BatchSource::new(&built.bytes)).collect();
    let files = sources
        .iter()
        .zip(built)
        .map(|(source, built)| open_graph_file(source, built))
        .collect();
    (remote_graph(files, options(16)), sources)
}

#[test]
fn a_walk_across_four_files_meets_the_graph_walk_gate() {
    let built: Vec<_> = (0..4).map(|file| graph_file(file * 32, 32, &[1, 9])).collect();
    let (graph, sources) = remote_graph_of(&built);
    // The limit is exactly event 127's ancestor count, so the walk ends on its last level without another step.
    let levels = graph
        .walk(b"room", 127, EdgeKind::Prev, 127)
        .map(Result::unwrap)
        .count();
    let (requests, bytes_fetched) = fetched(&sources);
    let measurement = GraphWalkMeasurement {
        bytes_fetched,
        files: sources.len() as u64,
        levels: levels as u64,
        requests,
    };
    println!("graph walk: {measurement:?}");
    assert!(requests > 0);
    assert!(graph_walk_gate_passes(&measurement));
}

/// Walks back from the last event of the 100,000-event room, split into four files of 25,000 events in 8192-row
/// granules, along `prev` and then `auth` edges. For each it prints the levels, requests and bytes fetched per level,
/// and time per level for a cold walk and for a second, warm walk over the same graph. Run by hand:
/// `cargo test --release --all-features --lib benchmark_graph_walk_over_a_room -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode graph walk benchmark"]
fn benchmark_graph_walk_over_a_room() {
    let events = 100_000u64;
    let room = room_graph(events);
    let per_file = 25_000usize;
    let built: Vec<_> = room
        .rows
        .chunks(per_file)
        .enumerate()
        .map(|(file, rows)| {
            let graph = GraphInput {
                first_sn: (file * per_file) as u64,
                inbound: Vec::new(),
                rows: rows.to_vec(),
                subject: room.subject.clone(),
                ..GraphInput::default()
            };
            graph_file_of(graph, 8192)
        })
        .collect();
    for kind in [EdgeKind::Prev, EdgeKind::Auth] {
        let (graph, sources) = remote_graph_of(&built);
        let (mut cold, mut requests, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
        let mut walk = graph.walk(&room.subject, events - 1, kind, usize::MAX);
        loop {
            let before = fetched(&sources);
            let started = Instant::now();
            let Some(level) = walk.next() else { break };
            cold.push(started.elapsed());
            level.unwrap();
            let after = fetched(&sources);
            requests.push(after.0 - before.0);
            bytes.push(after.1 - before.1);
        }
        let (total_requests, bytes_fetched) = fetched(&sources);
        let mut warm = Vec::new();
        let mut walk = graph.walk(&room.subject, events - 1, kind, usize::MAX);
        loop {
            let started = Instant::now();
            let Some(level) = walk.next() else { break };
            warm.push(started.elapsed());
            level.unwrap();
        }
        assert_eq!(warm.len(), cold.len());
        assert_eq!(fetched(&sources).0, total_requests, "the warm walk sent nothing");

        let levels = cold.len() as u32;
        let per_level = |times: &mut Vec<Duration>| {
            let mean = times.iter().sum::<Duration>() / levels;
            times.sort_unstable();
            (times[times.len() / 2], mean)
        };
        let (cold_median, cold_mean) = per_level(&mut cold);
        let (warm_median, warm_mean) = per_level(&mut warm);
        println!(
            "graph walk {kind:?}: {levels} levels over {} files, {total_requests} requests and {bytes_fetched} bytes \
             in all; per level: requests mean {:.5} max {}, bytes mean {:.0} max {}, cold median {cold_median:?} mean \
             {cold_mean:?}, warm median {warm_median:?} mean {warm_mean:?}",
            sources.len(),
            requests.iter().sum::<u64>() as f64 / f64::from(levels),
            requests.iter().max().unwrap(),
            bytes.iter().sum::<u64>() as f64 / f64::from(levels),
            bytes.iter().max().unwrap(),
        );
        assert!(graph_walk_gate_passes(&GraphWalkMeasurement {
            bytes_fetched,
            files: sources.len() as u64,
            levels: u64::from(levels),
            requests: total_requests,
        }));
    }
}

/// One stripe of three 64-row granules whose rows mostly carry 64 KiB of incompressible text (every eighth an
/// integer, so the field stays in the payload arena). Neighbouring edge blocks then sit about 3.5 MiB apart, so even
/// widened to their 1 MiB proof leaves they lie more than the joining gap apart.
fn spread_graph_file() -> BuiltHef {
    let mut config = source_form_config(64);
    config.targets.index_granularity_bytes = 64 << 20;
    config.targets.stripe_target_bytes = 64 << 20;
    config.graph = Some(graph_input(0, 192, &[1]));
    config.profile = Profile::Graph;
    let rows: Vec<_> = (0..192u64)
        .map(|i| {
            let blob = if i.is_multiple_of(8) {
                VariantValue::Int(i as i64)
            } else {
                VariantValue::String(noise(i, 64 * 1024))
            };
            let mut row = source_form_row(i, VariantValue::Object(StdBTreeMap::from([("blob".to_owned(), blob)])));
            row.external_id = Some(event_id(i));
            row
        })
        .collect();
    let built = build_hef_file(rows, &config).unwrap();
    assert_eq!((built.footer.stripes.len(), built.footer.granules.len()), (1, 3));
    built
}

#[test]
fn the_in_flight_cap_splits_a_step_into_batches() {
    let built = spread_graph_file();
    let sns: Vec<u64> = (0..192).collect();
    for (cap, expected) in [(1, vec![1, 1, 1]), (2, vec![2, 1]), (3, vec![3])] {
        let source = BatchSource::new(&built.bytes);
        let graph = remote_graph(vec![open_graph_file(&source, &built)], options(cap));
        let parents = graph.parents_of(b"room", &sns, EdgeKind::Prev).unwrap();
        assert_eq!(parents[191], [190]);
        let sizes: Vec<usize> = source.batches().iter().map(Vec::len).collect();
        assert_eq!(sizes, expected, "cap {cap}");
        let fetched: u64 = source.batches().iter().flatten().map(|range| range.len).sum();
        assert!(
            fetched < built.footer.stripes[0].byte_len / 2,
            "only the edge blocks' proof leaves are fetched"
        );
    }
}

#[test]
fn a_slow_batch_is_hedged_and_the_first_answer_wins() {
    let built = graph_file(0, 32, &[1]);
    let source = BatchSource::new(&built.bytes);
    let (release, gate) = channel();
    *source.hold_first.lock().unwrap() = Some(gate);
    let options = RemoteGraphOptions {
        hedge_floor_nanos: 10_000_000,
        max_in_flight: 8,
    };
    let graph = remote_graph(vec![open_graph_file(&source, &built)], options);
    assert_eq!(graph.parents(b"room", 20, EdgeKind::Prev).unwrap(), [19]);
    let batches = source.batches();
    assert_eq!(batches.len(), 2, "the stalled batch was sent again");
    assert_eq!(batches[0], batches[1]);
    release.send(()).unwrap();
}

#[test]
fn the_hedge_delay_follows_the_95th_percentile_above_the_floor() {
    let floor = 5_000_000;
    let graph = remote_graph(
        Vec::new(),
        RemoteGraphOptions {
            hedge_floor_nanos: floor,
            max_in_flight: 4,
        },
    );
    for millis in 1..=19 {
        graph.record(millis * 1_000_000);
    }
    assert_eq!(graph.hedge_delay(), floor, "too few samples to trust a percentile");
    for millis in 20..=100 {
        graph.record(millis * 1_000_000);
    }
    assert_eq!(graph.hedge_delay(), 95_000_000);
    for _ in 0..200 {
        graph.record(1_000);
    }
    assert_eq!(graph.hedge_delay(), floor, "never below the floor");
}

#[test]
fn a_remote_graph_refuses_files_it_cannot_serve() {
    let clock = || -> Arc<dyn crate::clock::Clock> { Arc::new(FixedClock::at(0)) };
    let open_one = |built: &BuiltHef| open_graph_file(&BatchSource::new(&built.bytes), built);
    let built = graph_file(0, 8, &[1]);
    assert!(RemoteGraph::new(vec![open_one(&built)], clock(), options(0)).is_err());
    let overlapping = graph_file(4, 8, &[1]);
    assert!(RemoteGraph::new(vec![open_one(&built), open_one(&overlapping)], clock(), options(4)).is_err());
    let events = multi_stripe_file(None);
    let source = CountingSource::new(&events.bytes);
    assert!(RemoteGraph::new(vec![open(&source, None, &events, None)], clock(), options(4)).is_err());

    let graph = remote_graph(vec![open_one(&built)], options(4));
    assert!(
        graph.parents(b"room", 8, EdgeKind::Prev).is_err(),
        "no file holds event 8"
    );
    assert!(
        graph.parents(b"other", 3, EdgeKind::Prev).is_err(),
        "no file holds that subject"
    );
}

#[test]
fn resolving_ids_in_small_stripes_costs_one_request_per_file() {
    let (graph, sources) = two_files(&[1]);
    let singles: Vec<usize> = sources.iter().map(|source| source.singles()).collect();
    let mut ids: Vec<Vec<u8>> = (0..64).map(event_id).collect();
    ids.push(b"$absent:example.org".to_vec());
    let asked: Vec<&[u8]> = ids.iter().map(Vec::as_slice).collect();
    let expected: Vec<Option<u64>> = (0..64).map(Some).chain([None]).collect();
    assert_eq!(graph.resolve_ids(b"room", &asked).unwrap(), expected);
    for (source, before) in sources.iter().zip(&singles) {
        let batches = source.batches();
        assert_eq!(
            batches.len(),
            1,
            "the proof leaves around these small stripes' marks pages hold their id blocks too"
        );
        assert_eq!(batches[0].len(), 1, "both stripes' marks pages join into one request");
        assert_eq!(source.singles(), *before, "nothing is fetched outside the batches");
    }

    assert_eq!(graph.resolve_ids(b"room", &asked).unwrap(), expected);
    assert!(
        sources.iter().all(|source| source.batches().len() == 1),
        "held blocks are never fetched again"
    );
    assert_eq!(graph.resolve_ids(b"other", &asked).unwrap(), vec![None; asked.len()]);
}

#[test]
fn an_id_a_files_index_rules_out_costs_that_file_nothing() {
    let (graph, sources) = two_files(&[1]);
    let id = event_id(40);
    assert_eq!(graph.resolve_ids(b"room", &[&id]).unwrap(), [Some(40)]);
    assert!(sources[0].batches().is_empty(), "the first file cannot hold event 40");
    assert_eq!(sources[1].batches().len(), 1);
    assert_eq!(graph.resolve_ids(b"room", &[b"$absent:example.org"]).unwrap(), [None]);
    assert!(sources[0].batches().is_empty() && sources[1].batches().len() == 1);
}

#[test]
fn resolving_ids_in_a_large_stripe_fetches_marks_then_id_blocks() {
    let built = spread_graph_file();
    let source = BatchSource::new(&built.bytes);
    let graph = remote_graph(vec![open_graph_file(&source, &built)], options(16));
    let singles = source.singles();
    let ids = [event_id(0), event_id(100), event_id(191)];
    let asked: Vec<&[u8]> = ids.iter().map(Vec::as_slice).collect();
    assert_eq!(
        graph.resolve_ids(b"room", &asked).unwrap(),
        [Some(0), Some(100), Some(191)]
    );
    let batches = source.batches();
    assert_eq!(
        batches.len(),
        2,
        "one batch for the marks pages, then one for the id blocks"
    );
    assert_eq!(batches[0].len(), 1);
    assert!(batches[1].len() <= 3, "one request per granule at most");
    assert_eq!(source.singles(), singles, "nothing is fetched outside the batches");
    let fetched: u64 = batches.iter().flatten().map(|range| range.len).sum();
    assert!(
        fetched < built.footer.stripes[0].byte_len / 2,
        "only the marks pages' and id blocks' proof leaves are fetched"
    );
}

fn label(sn: u64) -> ChainLabel {
    ChainLabel {
        chain_id: (sn % 3) as u32,
        seq: sn as u32,
    }
}

fn link(from_chain: u32, from_seq: u64) -> ChainLink {
    ChainLink {
        from_chain,
        from_seq: from_seq as u32,
        to_chain: 0,
        to_seq: 0,
    }
}

/// [`graph_input`] with each event on chain `sn % 3` at `seq` `sn`, and one link from chains 1 and 2 per file.
fn chain_graph(first_sn: u64, rows: u64) -> GraphInput {
    let mut graph = graph_input(first_sn, rows, &[1]);
    graph.chain_cover = Some(ChainCoverInput {
        labels: (first_sn..first_sn + rows).map(label).collect(),
        links: vec![link(2, first_sn + 2), link(1, first_sn + 1)],
    });
    graph
}

#[test]
fn chain_labels_ride_the_edge_blocks_in_one_request_per_file() {
    let (graph, sources) = two_graph_files([chain_graph(0, 32), chain_graph(32, 32)]);
    let sns: Vec<u64> = (0..64).collect();
    let labels = graph.chain_labels(b"room", &sns).unwrap();
    assert_eq!(labels, sns.iter().map(|&sn| Some(label(sn))).collect::<Vec<_>>());
    for source in &sources {
        let batches = source.batches();
        assert_eq!(batches.len(), 1, "one batch per file");
        assert_eq!(batches[0].len(), 1, "adjacent edge blocks join into one request");
    }

    let parents = graph.parents_of(b"room", &sns, EdgeKind::Prev).unwrap();
    assert_eq!(parents[40], [39]);
    assert_eq!(
        graph.chain_links(b"room", &[1, 2]).unwrap(),
        [link(1, 1), link(1, 33), link(2, 2), link(2, 34)]
    );
    assert!(graph.chain_links(b"room", &[0, 7]).unwrap().is_empty());
    assert!(
        sources.iter().all(|source| source.batches().len() == 1),
        "the edges came with the labels, and links come from the footers"
    );
}

#[test]
fn a_file_without_chain_cover_labels_its_events_none() {
    let (graph, _) = two_files(&[1]);
    assert_eq!(graph.chain_labels(b"room", &[3, 40]).unwrap(), [None, None]);
    assert!(graph.chain_links(b"room", &[0, 1]).unwrap().is_empty());
    assert!(graph.chain_labels(b"room", &[64]).is_err(), "no file holds event 64");
}

/// A tenant's state chunks held in memory, counting every read.
#[derive(Default)]
struct CountingChunks {
    chunks: StdHashMap<[u8; 32], Vec<u8>>,
    reads: Cell<usize>,
}

impl CountingChunks {
    fn store(&mut self, chunks: &[SealedStateChunk]) {
        for chunk in chunks {
            self.chunks.insert(chunk.address, chunk.bytes.clone());
        }
    }

    fn take_reads(&self) -> usize {
        self.reads.replace(0)
    }
}

impl StateChunkStore for CountingChunks {
    fn get(&self, address: &[u8; 32]) -> Result<Option<Vec<u8>>, StorageError> {
        self.reads.set(self.reads.get() + 1);
        Ok(self.chunks.get(address).cloned())
    }
}

fn member(user: u32) -> StateKey {
    StateKey {
        state_key_id: user,
        type_id: 2,
    }
}

#[test]
fn state_is_read_at_a_state_event_through_the_graph() {
    let key = StateChunkKey::derive(&TenantDek::new([7; 32]));
    let before: StdBTreeMap<StateKey, u64> = (0..2000).map(|user| (member(user), 3)).collect();
    let mut after = before.clone();
    after.insert(member(500), 41);
    after.remove(&member(1500));
    let trees = [
        build_state_tree(&key, &before).unwrap(),
        build_state_tree(&key, &after).unwrap(),
    ];
    let mut chunks = CountingChunks::default();
    for tree in &trees {
        chunks.store(&tree.chunks);
    }
    let mut first = graph_input(0, 32, &[1]);
    first.state_roots = StdBTreeMap::from([(3, trees[0].root)]);
    let mut second = graph_input(32, 32, &[1]);
    second.state_roots = StdBTreeMap::from([(41, trees[1].root)]);
    let (graph, sources) = two_graph_files([first, second]);
    let state = graph.state(&chunks, &key);

    let roots = [
        state.state_root(b"room", 3).unwrap().unwrap(),
        state.state_root(b"room", 41).unwrap().unwrap(),
    ];
    assert_eq!(roots, [trees[0].root, trees[1].root]);
    assert_eq!(state.state_root(b"room", 4).unwrap(), None, "not a state event");
    assert!(state.state_root(b"room", 64).is_err(), "no file holds event 64");
    assert!(
        sources.iter().all(|source| source.batches().is_empty()),
        "roots come from the footers"
    );

    assert_eq!(state.state_get(&roots[1], member(500)).unwrap(), Some(41));
    assert_eq!(chunks.take_reads(), 2, "one chunk per level: the root, then one leaf");
    assert_eq!(state.state_get(&roots[1], member(1500)).unwrap(), None);
    assert_eq!(state.state_get(&roots[0], member(1500)).unwrap(), Some(3));
    chunks.take_reads();

    let changes = state.state_diff(&roots[0], &roots[1]).unwrap();
    assert_eq!(
        changes,
        [
            StateChange {
                after: Some(41),
                before: Some(3),
                key: member(500),
            },
            StateChange {
                after: None,
                before: Some(3),
                key: member(1500),
            },
        ]
    );
    let reads = chunks.take_reads();
    assert!(
        reads < trees[0].chunks.len() + trees[1].chunks.len(),
        "shared leaves are never read ({reads} reads)"
    );
    assert!(state.state_diff(&roots[1], &roots[1]).unwrap().is_empty());
    assert_eq!(chunks.take_reads(), 0, "a state compared with itself reads nothing");
}

/// A tenant's state chunks in a simulated object store with a node-local cache in front: a read the cache misses is
/// counted as a fetch, which the caller charges one simulated round trip, and the chunk is then held, so a repeated
/// read is served locally.
#[derive(Default)]
struct CachedChunks {
    cache: RefCell<StdHashMap<[u8; 32], Vec<u8>>>,
    fetches: Cell<u64>,
    remote: StdHashMap<[u8; 32], Vec<u8>>,
}

impl StateChunkStore for CachedChunks {
    fn get(&self, address: &[u8; 32]) -> Result<Option<Vec<u8>>, StorageError> {
        if let Some(bytes) = self.cache.borrow().get(address) {
            return Ok(Some(bytes.clone()));
        }
        self.fetches.set(self.fetches.get() + 1);
        let bytes = self.remote.get(address).cloned();
        if let Some(bytes) = &bytes {
            self.cache.borrow_mut().insert(*address, bytes.clone());
        }
        Ok(bytes)
    }
}

/// The 99th percentile of `samples`.
fn p99(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() * 99 / 100]
}

/// The state at an event of a 100,000-member room, read through `RemoteGraph::state`: the state root from the event's
/// file, then one member's entry. The room takes 1,024 state events (membership changes of random members) after its
/// initial state, each with its own root, in two graph files. Each of 1,000 lookups at a random event runs cold, against
/// an empty chunk cache with every fetch charged a simulated 20 ms object-store round trip, then warm, repeated against
/// the cache its cold run filled. Prints the 99th percentiles and the most chunks a cold lookup fetched. Run by hand:
/// `cargo test --release --features write --lib state_at_an_event_cold_and_warm -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode state lookup benchmark"]
fn state_at_an_event_cold_and_warm() {
    const MEMBERS: u64 = 100_000;
    const STATE_EVENTS: u64 = 1_024;
    const LOOKUPS: usize = 1_000;
    const ROUND_TRIP_MICROS: f64 = 20_000.0;
    let mut random = {
        let mut state = 0x5EED_5EED_5EED_5EEDu64;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
    };
    let key = StateChunkKey::derive(&TenantDek::new([7; 32]));
    let mut state: StdBTreeMap<StateKey, u64> = (0..MEMBERS as u32).map(|user| (member(user), 0)).collect();
    let mut chunks = CachedChunks::default();
    let mut roots = StdBTreeMap::new();
    let mut tree_levels = 0;
    for sn in 1..=STATE_EVENTS {
        state.insert(member((random() % MEMBERS) as u32), sn);
        let tree = build_state_tree(&key, &state).unwrap();
        let root_chunk = tree
            .chunks
            .iter()
            .find(|chunk| chunk.address == tree.root.address)
            .unwrap();
        let plaintext = key.open(&tree.root.address, &root_chunk.bytes).unwrap();
        tree_levels = match decode_state_chunk(&plaintext).unwrap() {
            StateChunk::Interior { level, .. } => u64::from(level) + 1,
            StateChunk::Leaf(_) => 1,
        };
        roots.insert(sn, tree.root);
        for chunk in tree.chunks {
            chunks.remote.insert(chunk.address, chunk.bytes);
        }
    }
    let half = STATE_EVENTS.div_ceil(2);
    let mut first = graph_input(0, half, &[1]);
    first.state_roots = roots.range(..half).map(|(sn, root)| (*sn, *root)).collect();
    let mut second = graph_input(half, STATE_EVENTS + 1 - half, &[1]);
    second.state_roots = roots.range(half..).map(|(sn, root)| (*sn, *root)).collect();
    let (graph, _) = two_graph_files([first, second]);
    let reader = graph.state(&chunks, &key);

    let (mut cold, mut warm) = (Vec::with_capacity(LOOKUPS), Vec::with_capacity(LOOKUPS));
    let mut cold_fetches_max = 0;
    for _ in 0..LOOKUPS {
        let sn = 1 + random() % STATE_EVENTS;
        let user = member((random() % MEMBERS) as u32);
        let lookup = || {
            let root = reader.state_root(b"room", sn).unwrap().unwrap();
            std::hint::black_box(reader.state_get(&root, user).unwrap());
        };
        chunks.cache.borrow_mut().clear();
        chunks.fetches.set(0);
        let started = std::time::Instant::now();
        lookup();
        let fetches = chunks.fetches.get();
        cold.push(started.elapsed().as_secs_f64() * 1e6 + fetches as f64 * ROUND_TRIP_MICROS);
        cold_fetches_max = cold_fetches_max.max(fetches);
        let started = std::time::Instant::now();
        lookup();
        warm.push(started.elapsed().as_secs_f64() * 1e6);
        assert_eq!(chunks.fetches.get(), fetches, "a warm lookup fetches nothing");
    }
    let measurement = StateLookupMeasurement {
        cold_fetches_max,
        cold_p99_micros: p99(cold),
        round_trip_micros: ROUND_TRIP_MICROS,
        tree_levels,
        warm_p99_micros: p99(warm),
    };
    println!(
        "state at an event, {MEMBERS} members, {LOOKUPS} lookups: cold p99 {:.3} ms (at most {} fetches, \
         {ROUND_TRIP_MICROS} us round trip, {} tree levels), warm p99 {:.1} us",
        measurement.cold_p99_micros / 1e3,
        measurement.cold_fetches_max,
        measurement.tree_levels,
        measurement.warm_p99_micros
    );
    assert!(state_lookup_gate_passes(&measurement));
}
