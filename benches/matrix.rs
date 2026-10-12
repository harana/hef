//! Compares HEF with SQLite on the reads and writes a Matrix homeserver or client makes against one room: its events,
//! its event graph (the `prev_events` and `auth_events` DAG) and its state, at several room sizes.
//!
//! Each room is generated once per size, deterministically: a create event, the creator's join, power levels, join
//! rules and history visibility, then members joining, chatting, and now and then changing state, with an occasional
//! fork in the DAG. Every event is a Matrix-shaped PDU of a few hundred bytes. The same room is then stored twice:
//!
//! - HEF: one graph-profile file per room, holding every event's JSON, its protocol id index, its `prev` and `auth`
//!   edges and the room's state root after each state event; the state itself as a tree of sealed state chunks; new
//!   events appended through the HEF journal (HEJ) with `fdatasync` before they are acknowledged.
//! - SQLite: a Synapse-like schema (events, prev edges, auth edges, current state, state deltas) in WAL mode with
//!   `synchronous = FULL`, so a commit is as durable as a HEF journal flush.
//!
//! Every benchmark group is one Matrix access pattern; inside it, each backend is measured at each room size, with
//! throughput declared where an operation touches many events. Once Criterion has run, a comparison table of mean
//! times (and each room's stored bytes) is printed and written to `<criterion dir>/matrix-summary.md`, since
//! Criterion itself reports one benchmark at a time.
//!
//! Run: `cargo bench --features write --bench matrix`, or name a group to run only it, e.g.
//! `cargo bench --features write --bench matrix -- read/auth_chain`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use criterion::{BatchSize, BenchmarkGroup, BenchmarkId, Criterion, SamplingMode, Throughput, measurement::WallTime};
use hashbrown::{HashMap, HashSet};
use hef::artifacts::batch::{EventInput, PayloadInput};
use hef::artifacts::overlay::LiveOverlayStore;
use hef::artifacts::state_chunk::*;
use hef::artifacts::watermark::WatermarkTracker;
use hef::columns::{FreetextDeclaration, PromotionPlan};
use hef::deletes::{DeletionVector, encode_deletion_vector};
use hef::error::StorageError;
use hef::events::variant::VariantValue;
use hef::events::*;
use hef::invariants::io::{SystemClock, ThreadPoolEncodeExecutor};
use hef::invariants::{JournalStorage, ShardAtomicity, ShardId};
use hef::layout::graph::{DepthRange, EdgeKind, GranuleEdges, GraphInput, RowEdges};
use hef::layout::reader::{DECODED_CACHE_BUDGET_BYTES, HefFile, PayloadRead, PayloadRef};
use hef::layout::{LayoutTargets, Profile};
use hef::security::{FooterEncryption, StateChunkKey, TenantDek};
use hef::writer::build::*;
use hef::writer::pipeline::{FlushReason, RouteDependency, WorkerCommitPipeline};
use hef::writer::reserve::SequenceAllocator;
use hef::writer::sim::SimSafeRetryStore;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::hint::black_box;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

/// Room sizes measured, in events. A 1,000,000-event room is left out: its fixture rebuilds the whole state tree after
/// every state event, which takes far longer than the benchmarks themselves.
const ROOM_SIZES: [u64; 3] = [1_000, 10_000, 100_000];
/// Events a `/messages` page, a sync timeline and a batch event fetch return.
const PAGE: u64 = 50;
/// Events a backfill walk collects, and events one federation transaction or backfill response persists.
const BATCH: u64 = 100;
/// State event types, interned to their index as HEF state-chunk type ids.
const STATE_TYPES: [&str; 7] = [
    "m.room.create",
    "m.room.member",
    "m.room.power_levels",
    "m.room.join_rules",
    "m.room.history_visibility",
    "m.room.topic",
    "m.room.name",
];
const CREATE: u32 = 0;
const MEMBER: u32 = 1;
const POWER_LEVELS: u32 = 2;
const JOIN_RULES: u32 = 3;
const ROOM_ID: &str = "!bench:example.org";
const HEF: &str = "HEF";
const SQLITE: &str = "SQLite";
const COMMIT_QUEUE_BYTES: usize = 16 << 20;
const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    CREATE TABLE events (
        sn INTEGER PRIMARY KEY, event_id TEXT NOT NULL UNIQUE, type TEXT NOT NULL, state_key TEXT,
        sender TEXT NOT NULL, depth INTEGER NOT NULL, origin_server_ts INTEGER NOT NULL, json BLOB NOT NULL);
    CREATE TABLE event_edges (
        event_sn INTEGER NOT NULL, prev_sn INTEGER NOT NULL, PRIMARY KEY (event_sn, prev_sn)) WITHOUT ROWID;
    CREATE INDEX event_edges_prev ON event_edges (prev_sn);
    CREATE TABLE event_auth (
        event_sn INTEGER NOT NULL, auth_sn INTEGER NOT NULL, PRIMARY KEY (event_sn, auth_sn)) WITHOUT ROWID;
    CREATE TABLE current_state (
        type TEXT NOT NULL, state_key TEXT NOT NULL, event_sn INTEGER NOT NULL,
        PRIMARY KEY (type, state_key)) WITHOUT ROWID;
    CREATE TABLE state_deltas (
        type TEXT NOT NULL, state_key TEXT NOT NULL, sn INTEGER NOT NULL,
        PRIMARY KEY (type, state_key, sn)) WITHOUT ROWID;
    CREATE INDEX state_deltas_sn ON state_deltas (sn);";
const CONNECTION_PRAGMAS: &str = "
    PRAGMA synchronous = FULL;
    PRAGMA cache_size = -262144;
    PRAGMA mmap_size = 1073741824;";

/// A small deterministic xorshift generator, so every run measures the same room and the same probes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

/// One Matrix event of a generated room, numbered by its position in the room (its sequence number, `sn`).
#[derive(Clone)]
struct Event {
    auth: Vec<u64>,
    depth: u64,
    event_id: String,
    event_type: u32,
    json: Vec<u8>,
    origin_ts: i64,
    prev: Vec<u64>,
    sender: String,
    /// `Some` for a state event: its interned state key id and the state key itself.
    state_key: Option<(u32, String)>,
}

impl Event {
    fn type_name(&self) -> &'static str {
        STATE_TYPES
            .get(self.event_type as usize)
            .copied()
            .unwrap_or("m.room.message")
    }

    fn state(&self) -> Option<StateKey> {
        self.state_key.as_ref().map(|(state_key_id, _)| StateKey {
            state_key_id: *state_key_id,
            type_id: self.event_type,
        })
    }
}

/// A generated room and the state it ends in.
struct Room {
    /// Current state after the last event: `(type id, state key id)` to the event that set it.
    current: BTreeMap<StateKey, u64>,
    events: Vec<Event>,
    members: u32,
    /// A Matrix state key string to its interned id; `""` is 0 and member `i` is `i + 1`.
    state_key_ids: HashMap<String, u32>,
}

/// A Matrix event id for event `sn`: `$` and 43 url-safe base64 characters, as room version 4 and later spell them.
fn event_id(sn: u64) -> String {
    format!("${}", pseudo_base64(sn, 43))
}

fn pseudo_base64(seed: u64, len: usize) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    (0..len).map(|_| ALPHABET[(rng.next() & 63) as usize] as char).collect()
}

fn user(member: u32) -> String {
    format!("@user{member}:example.org")
}

fn id_list(room: &[Event], sns: &[u64]) -> String {
    let ids: Vec<String> = sns
        .iter()
        .map(|sn| format!("\"{}\"", room[*sn as usize].event_id))
        .collect();
    ids.join(",")
}

/// The PDU JSON of `event`, as a homeserver stores it: references, content, hashes and signatures.
fn pdu_json(events: &[Event], sn: u64, event: &Event, content: &str) -> Vec<u8> {
    let state_key = event
        .state_key
        .as_ref()
        .map(|(_, key)| format!("\"state_key\":\"{key}\","))
        .unwrap_or_default();
    format!(
        "{{\"auth_events\":[{}],\"content\":{content},\"depth\":{},\"hashes\":{{\"sha256\":\"{}\"}},\
         \"origin\":\"example.org\",\"origin_server_ts\":{},\"prev_events\":[{}],\"room_id\":\"{ROOM_ID}\",\
         \"sender\":\"{}\",\"signatures\":{{\"example.org\":{{\"ed25519:a1\":\"{}\"}}}},{state_key}\"type\":\"{}\",\
         \"unsigned\":{{\"age_ts\":{}}}}}",
        id_list(events, &event.auth),
        event.depth,
        pseudo_base64(sn ^ 0xA5A5, 43),
        event.origin_ts,
        id_list(events, &event.prev),
        event.sender,
        pseudo_base64(sn ^ 0x5A5A, 86),
        event.type_name(),
        event.origin_ts,
    )
    .into_bytes()
}

/// What kind of event the generator appends next.
enum Next {
    Join(u32),
    Leave(u32),
    Message(u32),
    Room(u32),
}

impl Room {
    /// A room of `size` events with `size / 20` members (at least 10), generated deterministically.
    fn generate(size: u64) -> Self {
        let members = (size / 20).max(10) as u32;
        let mut room = Room {
            current: BTreeMap::new(),
            events: Vec::with_capacity(size as usize),
            members,
            state_key_ids: HashMap::from([(String::new(), 0)]),
        };
        for member in 0..members {
            room.state_key_ids.insert(user(member), member + 1);
        }
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut joined: Vec<bool> = vec![false; members as usize];
        let mut next_join = 0u32;
        for sn in 0..size {
            let next = match sn {
                0 => Next::Room(CREATE),
                1 => Next::Join(0),
                2 => Next::Room(POWER_LEVELS),
                3 => Next::Room(JOIN_RULES),
                4 => Next::Room(4),
                _ if next_join < members && rng.below(4) == 0 => Next::Join(next_join),
                _ => match rng.below(100) {
                    0 => {
                        let member = rng.below(u64::from(next_join)) as u32;
                        if member != 0 && joined[member as usize] {
                            Next::Leave(member)
                        } else {
                            Next::Join(member)
                        }
                    }
                    1 => Next::Room([POWER_LEVELS, 5, 6][rng.below(3) as usize]),
                    _ => {
                        let member = rng.below(u64::from(next_join)) as u32;
                        Next::Message(if joined[member as usize] { member } else { 0 })
                    }
                },
            };
            let (sender, event_type, state_key, content) = match next {
                Next::Join(member) => {
                    joined[member as usize] = true;
                    next_join = next_join.max(member + 1);
                    let content = format!("{{\"displayname\":\"User {member}\",\"membership\":\"join\"}}");
                    (member, MEMBER, Some(user(member)), content)
                }
                Next::Leave(member) => {
                    joined[member as usize] = false;
                    (
                        member,
                        MEMBER,
                        Some(user(member)),
                        "{\"membership\":\"leave\"}".to_owned(),
                    )
                }
                Next::Message(member) => {
                    let content = format!(
                        "{{\"body\":\"message number {sn} in a room with a few words of text\",\"msgtype\":\"m.text\"}}"
                    );
                    (member, u32::MAX, None, content)
                }
                Next::Room(event_type) => {
                    let content = match event_type {
                        CREATE => "{\"creator\":\"@user0:example.org\",\"room_version\":\"10\"}".to_owned(),
                        POWER_LEVELS => format!(
                            "{{\"ban\":50,\"events_default\":0,\"kick\":50,\"redact\":50,\"state_default\":50,\
                             \"users\":{{\"@user0:example.org\":100}},\"users_default\":0,\"version\":{sn}}}"
                        ),
                        JOIN_RULES => "{\"join_rule\":\"public\"}".to_owned(),
                        4 => "{\"history_visibility\":\"shared\"}".to_owned(),
                        _ => format!("{{\"name\":\"Room name {sn}\",\"topic\":\"Topic {sn}\"}}"),
                    };
                    (0, event_type, Some(String::new()), content)
                }
            };
            room.push(sn, user(sender), event_type, state_key, &content, &mut rng);
        }
        room
    }

    /// Appends event `sn` after the room's latest event: its prev edges point at the latest event (and, one time in
    /// ten, the one before it too, as a fork that merges), its auth edges at the create event, the current power
    /// levels, the sender's membership and, for a membership change, the join rules.
    fn push(
        &mut self,
        sn: u64,
        sender: String,
        event_type: u32,
        state_key: Option<String>,
        content: &str,
        rng: &mut Rng,
    ) {
        let mut prev: Vec<u64> = sn.checked_sub(1).into_iter().collect();
        if sn > 5 && rng.below(10) == 0 {
            prev.push(sn - 2);
        }
        let current = |type_id: u32, state_key_id: u32| self.current.get(&StateKey { state_key_id, type_id });
        let sender_key = self.state_key_ids[&sender];
        let mut auth: Vec<u64> = Vec::new();
        for parent in [
            current(CREATE, 0),
            current(POWER_LEVELS, 0),
            current(MEMBER, sender_key),
            (event_type == MEMBER).then(|| current(JOIN_RULES, 0)).flatten(),
        ]
        .into_iter()
        .flatten()
        {
            if !auth.contains(parent) {
                auth.push(*parent);
            }
        }
        let depth = prev
            .iter()
            .map(|parent| self.events[*parent as usize].depth + 1)
            .max()
            .unwrap_or(1);
        let state_key = state_key.map(|key| (self.state_key_ids[&key], key));
        let mut event = Event {
            auth,
            depth,
            event_id: event_id(sn),
            event_type,
            json: Vec::new(),
            origin_ts: 1_700_000_000_000 + sn as i64 * 1_000,
            prev,
            sender,
            state_key,
        };
        event.json = pdu_json(&self.events, sn, &event, content);
        if let Some(key) = event.state() {
            self.current.insert(key, sn);
        }
        self.events.push(event);
    }

    /// A new message event that would follow the room's latest `extra` appended events: sequence number
    /// `len + extra`, sent by a member, pointing back at the event before it.
    fn next_message(&self, extra: u64) -> Event {
        let sn = self.events.len() as u64 + extra;
        let member = (sn % u64::from(self.members)) as u32;
        let auth: Vec<u64> = [CREATE, POWER_LEVELS]
            .iter()
            .filter_map(|type_id| {
                self.current.get(&StateKey {
                    state_key_id: 0,
                    type_id: *type_id,
                })
            })
            .copied()
            .collect();
        let mut event = Event {
            auth,
            depth: sn + 1,
            event_id: event_id(sn),
            event_type: u32::MAX,
            json: Vec::new(),
            origin_ts: 1_700_000_000_000 + sn as i64 * 1_000,
            prev: vec![sn - 1],
            sender: user(member),
            state_key: None,
        };
        let content = format!("{{\"body\":\"appended message {sn}\",\"msgtype\":\"m.text\"}}");
        event.json = format!(
            "{{\"content\":{content},\"depth\":{},\"origin_server_ts\":{},\"room_id\":\"{ROOM_ID}\",\
             \"sender\":\"{}\",\"type\":\"m.room.message\",\"hashes\":{{\"sha256\":\"{}\"}},\
             \"signatures\":{{\"example.org\":{{\"ed25519:a1\":\"{}\"}}}}}}",
            event.depth,
            event.origin_ts,
            event.sender,
            pseudo_base64(sn ^ 0xA5A5, 43),
            pseudo_base64(sn ^ 0x5A5A, 86),
        )
        .into_bytes();
        event
    }

    /// A membership change of member `member` following `extra` appended events: the state event a join or leave
    /// writes.
    fn next_membership(&self, extra: u64, member: u32, join: bool) -> Event {
        let mut event = self.next_message(extra);
        let membership = if join { "join" } else { "leave" };
        event.event_type = MEMBER;
        event.sender = user(member);
        event.state_key = Some((member + 1, user(member)));
        event.json = format!(
            "{{\"content\":{{\"membership\":\"{membership}\"}},\"depth\":{},\"room_id\":\"{ROOM_ID}\",\
             \"sender\":\"{}\",\"state_key\":\"{}\",\"type\":\"m.room.member\",\"signatures\":{{}}}}",
            event.depth, event.sender, event.sender,
        )
        .into_bytes();
        event
    }

    /// Random probe sequence numbers over the whole room, the same on every run.
    fn probes(&self, count: usize, seed: u64) -> Vec<u64> {
        let mut rng = Rng(seed);
        (0..count).map(|_| rng.below(self.events.len() as u64)).collect()
    }
}

/// HEF's tenant for the benchmark, and its key for sealing state chunks.
fn tenant() -> TenantId {
    TenantId::from_uuid(uuid::Uuid::from_u128(0xBE7C_0000_0000_0000_0000_0000_0000_0001))
}

/// The HEF journal row for `event`, as the Matrix event store appends it: room as the entity, sender as the actor,
/// the PDU JSON as the payload, and its `prev` and `auth` ids as relationships. `nonce` makes its dedupe identity
/// unique.
fn event_input(sn: u64, event: &Event, nonce: u64) -> EventInput {
    let refs: Vec<RelationshipRef> = event
        .prev
        .iter()
        .map(|parent| (RelationshipKind::Prev, *parent))
        .chain(event.auth.iter().map(|parent| (RelationshipKind::Auth, *parent)))
        .map(|(kind, parent)| RelationshipRef::to_external(kind, event_id(parent).as_bytes()).unwrap())
        .collect();
    EventInput {
        connector_delivery_hash_high: nonce,
        connector_delivery_hash_low: sn,
        envelope: EventEnvelope {
            account_id: None,
            account_id_hash_low: 0,
            actor_id: Some(event.sender.clone()),
            actor_id_hash_low: xxhash_rust::xxh3::xxh3_64(event.sender.as_bytes()),
            dedupe_hash_high: nonce,
            dedupe_hash_low: sn,
            entity_id: Some(ROOM_ID.to_owned()),
            entity_id_hash_high: 0,
            entity_id_hash_low: xxhash_rust::xxh3::xxh3_64(ROOM_ID.as_bytes()),
            entity_type: "room".to_owned(),
            event_id: EventId::from_uuid(uuid::Uuid::from_u128(u128::from(sn) << 64 | u128::from(nonce))),
            event_type: event.type_name().to_owned(),
            flags: EventFlags(0),
            ingested_at: TimestampValue::from_physical_nanos(event.origin_ts * 1_000_000),
            occurred_at: TimestampValue::from_physical_nanos(event.origin_ts * 1_000_000),
            schema_version: 1,
            source: "matrix".to_owned(),
            stream_id: StreamId(0),
            stream_sequence: sn,
            tenant_id: tenant(),
            trace_id_hash_low: 0,
        },
        payload: PayloadInput::Variant(VariantValue::Binary(event.json.clone())),
        provenance: None,
        relationships: (!refs.is_empty()).then(|| EventRelationships::new(refs).unwrap()),
        source_delivery: None,
        source_schema: None,
    }
}

/// A tenant's state chunks held by address, standing in for the object store (and its local disk cache) a deployment
/// keeps them in.
#[derive(Default)]
struct ChunkStore(HashMap<[u8; 32], Vec<u8>>);

impl ChunkStore {
    /// Stores every chunk of `tree` it does not hold yet, and returns the bytes that added.
    fn store(&mut self, tree: &StateTree) -> u64 {
        let mut added = 0;
        for chunk in &tree.chunks {
            if !self.0.contains_key(&chunk.address) {
                added += chunk.bytes.len() as u64;
                self.0.insert(chunk.address, chunk.bytes.clone());
            }
        }
        added
    }
}

impl StateChunkStore for ChunkStore {
    fn get(&self, address: &[u8; 32]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.0.get(address).cloned())
    }
}

/// A room's state as HEF keeps it: the tree after every state event, sealed into content-addressed chunks.
struct HefState {
    chunk_bytes: u64,
    chunks: ChunkStore,
    empty: StateRoot,
    key: StateChunkKey,
    /// The tree of the room's current state, the one the next state change is built against.
    latest: StateTree,
    roots: BTreeMap<u64, StateRoot>,
}

impl HefState {
    /// Builds the state tree after every state event of `room`, storing each tree's new chunks.
    fn build(room: &Room) -> Self {
        let key = StateChunkKey::derive(&TenantDek::new([7; 32]));
        let mut chunks = ChunkStore::default();
        let empty_tree = build_state_tree(&key, &BTreeMap::new()).unwrap();
        let mut chunk_bytes = chunks.store(&empty_tree);
        let mut state = BTreeMap::new();
        let mut roots = BTreeMap::new();
        let mut latest = empty_tree.clone();
        for (sn, event) in room.events.iter().enumerate() {
            if let Some(state_key) = event.state() {
                state.insert(state_key, sn as u64);
                latest = build_state_tree(&key, &state).unwrap();
                chunk_bytes += chunks.store(&latest);
                roots.insert(sn as u64, latest.root);
            }
        }
        HefState {
            chunk_bytes,
            chunks,
            empty: empty_tree.root,
            key,
            latest,
            roots,
        }
    }
}

/// The build rows and graph of a whole room, ready to seal as one HEF graph-profile file.
fn hef_rows(room: &Room, roots: &BTreeMap<u64, StateRoot>) -> (Vec<BuildRow>, HefBuildConfig) {
    let rows: Vec<HefRow> = room
        .events
        .iter()
        .enumerate()
        .map(|(sn, event)| {
            let mut input = event_input(sn as u64, event, 0);
            // The file stores the DAG natively as graph edges, so the rows do not repeat it as relationships.
            input.relationships = None;
            HefRow {
                epoch: 1,
                event: input,
                sequence: sn as u64 + 1,
            }
        })
        .collect();
    let mut rows = <HefRow as BuildInput>::into_build_rows(rows);
    for (row, event) in rows.iter_mut().zip(&room.events) {
        row.external_id = Some(event.event_id.as_bytes().to_vec());
    }
    let depths = room.events.iter().map(|event| event.depth);
    let graph = GraphInput {
        chain_cover: None,
        depth: Some(DepthRange {
            max: depths.clone().max().unwrap_or(0),
            min: depths.min().unwrap_or(0),
        }),
        first_sn: 0,
        inbound: Vec::new(),
        rows: room
            .events
            .iter()
            .map(|event| RowEdges {
                auth: event.auth.clone(),
                prev: event.prev.clone(),
            })
            .collect(),
        state_roots: roots.clone(),
        subject: ROOM_ID.as_bytes().to_vec(),
        unresolved: Vec::new(),
    };
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        audit: None,
        created_at_physical: 1,
        footer_dek: None,
        footer_encryption: FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        graph: Some(graph),
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        profile: Profile::Graph,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets::default(),
        tenant_id: tenant(),
        time_series_bucket_nanos: None,
    };
    (rows, config)
}

/// The JSON bytes of a payload HEF read back.
fn json_of(read: PayloadRead) -> Vec<u8> {
    match read {
        PayloadRead::Value(VariantValue::Binary(json)) => json,
        _ => panic!("every event stores its JSON as binary"),
    }
}

/// One room stored as a HEF graph file plus its state chunks.
struct HefRoom {
    file: HefFile,
    file_bytes: u64,
    state: HefState,
    /// The room's state chunks opened and decoded once read, within the budget the file reader gives its own decoded
    /// caches.
    state_cache: StateChunkCache,
}

impl HefRoom {
    fn build(room: &Room) -> Self {
        let state = HefState::build(room);
        let (rows, config) = hef_rows(room, &state.roots);
        let built = build_hef_file_with_executor(rows, &config, &ThreadPoolEncodeExecutor).unwrap();
        let file_bytes = built.bytes.len() as u64;
        let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
        let state_cache = StateChunkCache::new(state.key.clone(), DECODED_CACHE_BUDGET_BYTES);
        HefRoom {
            file,
            file_bytes,
            state,
            state_cache,
        }
    }

    fn event_by_id(&self, event_id: &str) -> Vec<u8> {
        let sn = self.file.graph_sn_of(event_id.as_bytes()).unwrap().unwrap();
        json_of(self.file.payload(self.file.graph_row_of(sn).unwrap()).unwrap())
    }

    fn events_by_id(&self, event_ids: &[&str]) -> Vec<Vec<u8>> {
        let refs: Vec<PayloadRef> = event_ids
            .iter()
            .map(|id| {
                let sn = self.file.graph_sn_of(id.as_bytes()).unwrap().unwrap();
                PayloadRef {
                    row_ordinal: self.file.graph_row_of(sn).unwrap(),
                }
            })
            .collect();
        self.payloads(&refs)
    }

    fn payloads(&self, refs: &[PayloadRef]) -> Vec<Vec<u8>> {
        self.file
            .read_payloads(refs)
            .unwrap()
            .payloads
            .into_iter()
            .map(json_of)
            .collect()
    }

    /// The `limit` events before `before`, newest first, with their JSON.
    fn page_before(&self, before: u64, limit: u64) -> Vec<Vec<u8>> {
        let refs: Vec<PayloadRef> = (before.saturating_sub(limit)..before)
            .rev()
            .map(|sn| PayloadRef {
                row_ordinal: self.file.graph_row_of(sn).unwrap(),
            })
            .collect();
        self.payloads(&refs)
    }

    /// The decoded edges of the granule holding `sn`, taken from the reader's decoded-edge cache once per walk.
    fn edges<'a>(&self, cache: &'a mut HashMap<u32, Arc<GranuleEdges>>, sn: u64) -> &'a GranuleEdges {
        let granule = self.file.graph_granule_of(sn).unwrap();
        cache
            .entry(granule)
            .or_insert_with(|| self.file.cached_granule_edges(granule).unwrap())
    }

    /// Walks `prev` edges breadth first from `from`, newest parent first, collecting up to `limit` events, and returns
    /// their JSON: a backfill response.
    fn backfill(&self, from: u64, limit: u64) -> Vec<Vec<u8>> {
        let mut cache = HashMap::new();
        let mut seen: HashSet<u64> = [from].into_iter().collect();
        let mut queue = VecDeque::from([from]);
        let mut found = Vec::new();
        while let Some(sn) = queue.pop_front() {
            found.push(PayloadRef {
                row_ordinal: self.file.graph_row_of(sn).unwrap(),
            });
            if found.len() as u64 == limit {
                break;
            }
            let mut parents = self.edges(&mut cache, sn).parents(sn, EdgeKind::Prev).to_vec();
            parents.sort_unstable_by(|a, b| b.cmp(a));
            for parent in parents {
                if seen.insert(parent) {
                    queue.push_back(parent);
                }
            }
        }
        self.payloads(&found)
    }

    /// Every event `from` reaches through `auth` edges: its auth chain.
    fn auth_chain(&self, from: u64) -> HashSet<u64> {
        let mut cache = HashMap::new();
        let mut chain = HashSet::new();
        let mut stack = vec![from];
        while let Some(sn) = stack.pop() {
            for parent in self.edges(&mut cache, sn).parents(sn, EdgeKind::Auth) {
                if chain.insert(*parent) {
                    stack.push(*parent);
                }
            }
        }
        chain
    }

    fn children(&self, sn: u64) -> Vec<u64> {
        let granule = self.file.graph_granule_of(sn).unwrap();
        self.file
            .cached_granule_edges(granule)
            .unwrap()
            .children(sn, EdgeKind::Prev)
    }

    /// The state root in force right after event `sn`: the root of the latest state event at or before it.
    fn root_at(&self, sn: u64) -> StateRoot {
        let roots = &self.file.footer().state_roots;
        let after = roots.partition_point(|entry| entry.row_ordinal <= sn);
        roots[after - 1].root
    }

    fn state_get(&self, root: &StateRoot, state_key: StateKey) -> Option<u64> {
        self.state_cache.state_get(&self.state.chunks, root, state_key).unwrap()
    }

    fn state_diff(&self, before: &StateRoot, after: &StateRoot) -> Vec<StateChange> {
        state_diff(&self.state.chunks, &self.state.key, before, after).unwrap()
    }
}

/// Keeps a HEF journal (HEJ) in one file on local disk, appending whole aligned frames and syncing with
/// `fdatasync`, as the Matrix event store's journal storage does.
struct FileJournal {
    extent: u64,
    file: File,
}

impl FileJournal {
    fn open(path: &Path) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        FileJournal { extent: 0, file }
    }
}

fn io_error(error: std::io::Error) -> StorageError {
    StorageError::Io {
        detail: error.to_string(),
        op: "bench journal",
    }
}

impl JournalStorage for FileJournal {
    fn append(&mut self, _shard: ShardId, frame: &[u8]) -> Result<u64, StorageError> {
        let offset = self.extent;
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.file.write_all(frame).map_err(io_error)?;
        self.extent += frame.len() as u64;
        Ok(offset)
    }

    fn sync(&mut self, _shard: ShardId) -> Result<(), StorageError> {
        self.file.sync_data().map_err(io_error)
    }

    fn truncate(&mut self, _shard: ShardId, len: u64) -> Result<(), StorageError> {
        self.file.set_len(len).map_err(io_error)?;
        self.extent = len;
        Ok(())
    }

    fn read(&self, _shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, StorageError> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut bytes = vec![0; len as usize];
        file.read_exact(&mut bytes).map_err(io_error)?;
        Ok(bytes)
    }

    fn extent(&self, _shard: ShardId) -> Result<u64, StorageError> {
        Ok(self.extent)
    }

    fn atomicity(&self, _shard: ShardId) -> Result<ShardAtomicity, StorageError> {
        Ok(ShardAtomicity::buffered_unprobed())
    }
}

/// HEF's write path for one room's new events: the commit pipeline over a journal file, acknowledging each flush
/// only once its frame is synced to disk.
struct HefJournal {
    _dir: TempDir,
    allocator: SequenceAllocator,
    clock: SystemClock,
    nonce: u64,
    overlay: LiveOverlayStore,
    pipeline: WorkerCommitPipeline,
    retry: SimSafeRetryStore,
    storage: FileJournal,
    watermarks: WatermarkTracker,
}

impl HefJournal {
    fn open() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileJournal::open(&dir.path().join("0000000000000001.hej"));
        HefJournal {
            _dir: dir,
            allocator: SequenceAllocator::new(1),
            clock: SystemClock::new(),
            nonce: 0,
            overlay: LiveOverlayStore::new(),
            pipeline: WorkerCommitPipeline::new(
                1,
                ShardId(1),
                tenant(),
                RouteDependency::AppendOnly,
                COMMIT_QUEUE_BYTES,
            ),
            retry: SimSafeRetryStore::new(),
            storage,
            watermarks: WatermarkTracker::new(),
        }
    }

    /// Appends `events` and returns once all of them are synced. The events are queued, then flushed together as a
    /// federation transaction or a backfill response is persisted: a large batch lands as several frames of up to the
    /// flush target, made durable by one sync.
    fn append(&mut self, first_sn: u64, events: &[Event]) {
        self.nonce += 1;
        for (event, sn) in events.iter().zip(first_sn..) {
            let input = event_input(sn, event, self.nonce);
            self.pipeline
                .submit(input, self.allocator.epoch(), &mut self.retry, &self.clock)
                .unwrap();
        }
        self.flush(FlushReason::ForceCommit);
        // Published frames stay readable from the overlay until a file covers them; the benchmark never publishes,
        // so it drops them here to keep memory flat across iterations.
        if self.overlay.segment_count() > 1024 {
            self.overlay = LiveOverlayStore::new();
        }
    }

    fn flush(&mut self, reason: FlushReason) {
        self.pipeline
            .flush(
                reason,
                &mut self.allocator,
                &mut self.storage,
                &mut self.watermarks,
                &mut self.retry,
                &mut self.overlay,
                &self.clock,
            )
            .unwrap();
    }
}

/// One room stored in SQLite, in a database file of its own.
struct SqliteRoom {
    _dir: TempDir,
    db: Connection,
    path: PathBuf,
}

fn sqlite_insert(db: &Connection, sn: u64, event: &Event) {
    let mut insert = db
        .prepare_cached(
            "INSERT INTO events (sn, event_id, type, state_key, sender, depth, origin_server_ts, json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .unwrap();
    insert
        .execute(params![
            sn as i64,
            event.event_id,
            event.type_name(),
            event.state_key.as_ref().map(|(_, key)| key),
            event.sender,
            event.depth as i64,
            event.origin_ts,
            event.json,
        ])
        .unwrap();
    let mut edge = db
        .prepare_cached("INSERT INTO event_edges (event_sn, prev_sn) VALUES (?1, ?2)")
        .unwrap();
    for parent in &event.prev {
        edge.execute(params![sn as i64, *parent as i64]).unwrap();
    }
    let mut auth = db
        .prepare_cached("INSERT INTO event_auth (event_sn, auth_sn) VALUES (?1, ?2)")
        .unwrap();
    for parent in &event.auth {
        auth.execute(params![sn as i64, *parent as i64]).unwrap();
    }
    if let Some((_, state_key)) = &event.state_key {
        db.prepare_cached("INSERT OR REPLACE INTO current_state (type, state_key, event_sn) VALUES (?1, ?2, ?3)")
            .unwrap()
            .execute(params![event.type_name(), state_key, sn as i64])
            .unwrap();
        db.prepare_cached("INSERT INTO state_deltas (type, state_key, sn) VALUES (?1, ?2, ?3)")
            .unwrap()
            .execute(params![event.type_name(), state_key, sn as i64])
            .unwrap();
    }
}

fn sqlite_open(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch(CONNECTION_PRAGMAS).unwrap();
    db
}

/// Creates the schema in a new database at `path` and writes every event of `room` in one transaction.
fn sqlite_import(path: &Path, room: &Room) -> Connection {
    let mut db = sqlite_open(path);
    db.execute_batch(SCHEMA).unwrap();
    let transaction = db.transaction().unwrap();
    for (sn, event) in room.events.iter().enumerate() {
        sqlite_insert(&transaction, sn as u64, event);
    }
    transaction.commit().unwrap();
    db
}

impl SqliteRoom {
    fn build(room: &Room) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("room.db");
        let db = sqlite_import(&path, room);
        db.execute_batch("ANALYZE; PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        SqliteRoom { _dir: dir, db, path }
    }

    fn file_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).unwrap().len()
    }

    /// A private copy of this room's database, for a benchmark that writes to it.
    fn copy(&self) -> SqliteRoom {
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("room.db");
        std::fs::copy(&self.path, &path).unwrap();
        let db = sqlite_open(&path);
        SqliteRoom { _dir: dir, db, path }
    }

    fn event_by_id(&self, event_id: &str) -> Vec<u8> {
        self.db
            .prepare_cached("SELECT json FROM events WHERE event_id = ?1")
            .unwrap()
            .query_row([event_id], |row| row.get(0))
            .unwrap()
    }

    fn events_by_id(&self, event_ids: &[&str]) -> Vec<Vec<u8>> {
        event_ids.iter().map(|id| self.event_by_id(id)).collect()
    }

    fn page_before(&self, before: u64, limit: u64) -> Vec<Vec<u8>> {
        let mut page = self
            .db
            .prepare_cached("SELECT json FROM events WHERE sn < ?1 ORDER BY sn DESC LIMIT ?2")
            .unwrap();
        page.query_map(params![before as i64, limit as i64], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn backfill(&self, from: u64, limit: u64) -> Vec<Vec<u8>> {
        let mut parents = self
            .db
            .prepare_cached("SELECT prev_sn FROM event_edges WHERE event_sn = ?1 ORDER BY prev_sn DESC")
            .unwrap();
        let mut json = self.db.prepare_cached("SELECT json FROM events WHERE sn = ?1").unwrap();
        let mut seen: HashSet<u64> = [from].into_iter().collect();
        let mut queue = VecDeque::from([from]);
        let mut found = Vec::new();
        while let Some(sn) = queue.pop_front() {
            found.push(json.query_row([sn as i64], |row| row.get(0)).unwrap());
            if found.len() as u64 == limit {
                break;
            }
            let rows = parents.query_map([sn as i64], |row| row.get::<_, i64>(0)).unwrap();
            for parent in rows {
                let parent = parent.unwrap() as u64;
                if seen.insert(parent) {
                    queue.push_back(parent);
                }
            }
        }
        found
    }

    fn auth_chain(&self, from: u64) -> HashSet<u64> {
        let mut chain = self
            .db
            .prepare_cached(
                "WITH RECURSIVE chain(sn) AS (
                     SELECT auth_sn FROM event_auth WHERE event_sn = ?1
                     UNION SELECT event_auth.auth_sn FROM event_auth JOIN chain ON event_auth.event_sn = chain.sn)
                 SELECT sn FROM chain",
            )
            .unwrap();
        chain
            .query_map([from as i64], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(|sn| sn.unwrap() as u64)
            .collect()
    }

    fn children(&self, sn: u64) -> Vec<u64> {
        let mut children = self
            .db
            .prepare_cached("SELECT event_sn FROM event_edges WHERE prev_sn = ?1")
            .unwrap();
        children
            .query_map([sn as i64], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(|sn| sn.unwrap() as u64)
            .collect()
    }

    fn current_state_get(&self, event_type: &str, state_key: &str) -> Option<u64> {
        self.db
            .prepare_cached("SELECT event_sn FROM current_state WHERE type = ?1 AND state_key = ?2")
            .unwrap()
            .query_row([event_type, state_key], |row| row.get::<_, i64>(0))
            .optional()
            .unwrap()
            .map(|sn| sn as u64)
    }

    fn state_at(&self, sn: u64, event_type: &str, state_key: &str) -> Option<u64> {
        self.db
            .prepare_cached(
                "SELECT sn FROM state_deltas WHERE type = ?1 AND state_key = ?2 AND sn <= ?3
                 ORDER BY sn DESC LIMIT 1",
            )
            .unwrap()
            .query_row(params![event_type, state_key, sn as i64], |row| row.get::<_, i64>(0))
            .optional()
            .unwrap()
            .map(|sn| sn as u64)
    }

    fn full_state(&self) -> Vec<(String, String, u64)> {
        let mut state = self
            .db
            .prepare_cached("SELECT type, state_key, event_sn FROM current_state")
            .unwrap();
        state
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn state_diff(&self, after: u64, upto: u64) -> Vec<(String, String, u64)> {
        let mut diff = self
            .db
            .prepare_cached(
                "SELECT type, state_key, MAX(sn) FROM state_deltas WHERE sn > ?1 AND sn <= ?2
                 GROUP BY type, state_key",
            )
            .unwrap();
        diff.query_map(params![after as i64, upto as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// Writes `events` from `first_sn` on in one durable transaction.
    fn append(&mut self, first_sn: u64, events: &[Event]) {
        let transaction = self.db.transaction().unwrap();
        for (event, sn) in events.iter().zip(first_sn..) {
            sqlite_insert(&transaction, sn, event);
        }
        transaction.commit().unwrap();
    }
}

/// Writes `bytes` to `file` in place of what it held, and waits until they are on disk.
fn write_durably(file: &mut File, bytes: &[u8]) {
    file.set_len(0).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_data().unwrap();
}

/// One room size's fixture: the generated room and its HEF and SQLite copies.
struct Fixture {
    hef: HefRoom,
    room: Room,
    size: u64,
    sqlite: SqliteRoom,
}

impl Fixture {
    fn build(size: u64) -> Self {
        let room = Room::generate(size);
        let hef = HefRoom::build(&room);
        let sqlite = SqliteRoom::build(&room);
        Fixture {
            hef,
            room,
            size,
            sqlite,
        }
    }

    fn id(&self, backend: &str) -> BenchmarkId {
        BenchmarkId::new(backend, self.size)
    }

    /// A member's interned state key and its Matrix spelling.
    fn member_key(&self, member: u32) -> (StateKey, String) {
        let name = user(member);
        let state_key = StateKey {
            state_key_id: self.room.state_key_ids[&name],
            type_id: MEMBER,
        };
        (state_key, name)
    }
}

/// Cycles through `items` one per call, so every iteration probes a different event.
fn cycle<T: Copy>(items: &[T]) -> impl FnMut() -> T + '_ {
    let mut next = 0;
    move || {
        let item = items[next % items.len()];
        next += 1;
        item
    }
}

type Group<'a> = BenchmarkGroup<'a, WallTime>;

fn read_group<'a>(criterion: &'a mut Criterion, name: &str) -> Group<'a> {
    let mut group = criterion.benchmark_group(name);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(30);
    group
}

fn write_group<'a>(criterion: &'a mut Criterion, name: &str) -> Group<'a> {
    let mut group = criterion.benchmark_group(name);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));
    group.sample_size(15);
    group
}

fn heavy_group<'a>(criterion: &'a mut Criterion, name: &str) -> Group<'a> {
    let mut group = criterion.benchmark_group(name);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_millis(100));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(10);
    group
}

/// Create: new events arriving one by one (a client send), as a federation transaction, and a whole room imported
/// at once (a join over federation, or a client's first sync of a room).
fn create(criterion: &mut Criterion, fixtures: &[Fixture]) {
    let mut group = write_group(criterion, "create/append_event");
    for fixture in fixtures {
        let room = &fixture.room;
        let next = room.events.len() as u64;
        group.bench_function(fixture.id(HEF), |b| {
            let mut journal = HefJournal::open();
            let mut appended = 0;
            b.iter(|| {
                let event = room.next_message(appended);
                journal.append(next + appended, std::slice::from_ref(&event));
                appended += 1;
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut db = fixture.sqlite.copy();
            let mut appended = 0;
            b.iter(|| {
                let event = room.next_message(appended);
                db.append(next + appended, std::slice::from_ref(&event));
                appended += 1;
            });
        });
    }
    group.finish();

    let mut group = write_group(criterion, "create/append_batch");
    group.throughput(Throughput::Elements(BATCH));
    for fixture in fixtures {
        let room = &fixture.room;
        let next = room.events.len() as u64;
        let batch = |appended: u64| -> Vec<Event> { (0..BATCH).map(|i| room.next_message(appended + i)).collect() };
        group.bench_function(fixture.id(HEF), |b| {
            let mut journal = HefJournal::open();
            let mut appended = 0;
            b.iter(|| {
                journal.append(next + appended, &batch(appended));
                appended += BATCH;
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut db = fixture.sqlite.copy();
            let mut appended = 0;
            b.iter(|| {
                db.append(next + appended, &batch(appended));
                appended += BATCH;
            });
        });
    }
    group.finish();

    let mut group = heavy_group(criterion, "create/import_room");
    for fixture in fixtures {
        let room = &fixture.room;
        group.throughput(Throughput::Elements(fixture.size));
        group.bench_function(fixture.id(HEF), |b| {
            let dir = tempfile::tempdir().unwrap();
            let mut out = File::create(dir.path().join("room.hef")).unwrap();
            b.iter(|| {
                let state = HefState::build(room);
                let (rows, config) = hef_rows(room, &state.roots);
                let built = build_hef_file_with_executor(rows, &config, &ThreadPoolEncodeExecutor).unwrap();
                write_durably(&mut out, &built.bytes);
                black_box(state.chunk_bytes)
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            b.iter_batched(
                || tempfile::tempdir().unwrap(),
                |dir| {
                    let db = sqlite_import(&dir.path().join("room.db"), room);
                    drop(db);
                    dir
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// Read: the event fetches, timeline pages, DAG walks and state lookups sync, `/messages`, `/context`, backfill,
/// event authorization and state resolution make.
fn read(criterion: &mut Criterion, fixtures: &[Fixture]) {
    let mut group = read_group(criterion, "read/event_by_id");
    for fixture in fixtures {
        let ids: Vec<&str> = fixture
            .room
            .probes(4096, 1)
            .iter()
            .map(|sn| fixture.room.events[*sn as usize].event_id.as_str())
            .collect();
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&ids);
            b.iter(|| fixture.hef.event_by_id(next()));
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&ids);
            b.iter(|| fixture.sqlite.event_by_id(next()));
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/event_batch");
    group.throughput(Throughput::Elements(PAGE));
    for fixture in fixtures {
        let batches: Vec<Vec<&str>> = (0..64)
            .map(|seed| {
                fixture
                    .room
                    .probes(PAGE as usize, seed + 2)
                    .iter()
                    .map(|sn| fixture.room.events[*sn as usize].event_id.as_str())
                    .collect()
            })
            .collect();
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = 0;
            b.iter(|| {
                next += 1;
                fixture.hef.events_by_id(&batches[next % batches.len()])
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = 0;
            b.iter(|| {
                next += 1;
                fixture.sqlite.events_by_id(&batches[next % batches.len()])
            });
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/timeline_latest");
    group.throughput(Throughput::Elements(PAGE));
    for fixture in fixtures {
        group.bench_function(fixture.id(HEF), |b| {
            b.iter(|| fixture.hef.page_before(fixture.size, PAGE))
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            b.iter(|| fixture.sqlite.page_before(fixture.size, PAGE))
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/paginate_back");
    group.throughput(Throughput::Elements(PAGE));
    for fixture in fixtures {
        let starts: Vec<u64> = fixture.room.probes(4096, 3).iter().map(|sn| (*sn).max(PAGE)).collect();
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&starts);
            b.iter(|| fixture.hef.page_before(next(), PAGE));
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&starts);
            b.iter(|| fixture.sqlite.page_before(next(), PAGE));
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/backfill_walk");
    group.throughput(Throughput::Elements(BATCH));
    for fixture in fixtures {
        let starts: Vec<u64> = fixture.room.probes(4096, 4).iter().map(|sn| (*sn).max(BATCH)).collect();
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&starts);
            b.iter(|| fixture.hef.backfill(next(), BATCH));
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&starts);
            b.iter(|| fixture.sqlite.backfill(next(), BATCH));
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/auth_chain");
    for fixture in fixtures {
        let starts = fixture.room.probes(4096, 5);
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&starts);
            b.iter(|| fixture.hef.auth_chain(next()));
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&starts);
            b.iter(|| fixture.sqlite.auth_chain(next()));
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/event_children");
    for fixture in fixtures {
        let probes = fixture.room.probes(4096, 6);
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&probes);
            b.iter(|| fixture.hef.children(next()));
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&probes);
            b.iter(|| fixture.sqlite.children(next()));
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/current_state_get");
    for fixture in fixtures {
        let members: Vec<u32> = fixture
            .room
            .probes(4096, 7)
            .iter()
            .map(|sn| (sn % u64::from(fixture.room.members)) as u32)
            .collect();
        let latest = fixture.hef.state.latest.root;
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&members);
            b.iter(|| {
                let state_key = StateKey {
                    state_key_id: fixture.room.state_key_ids[&user(next())],
                    type_id: MEMBER,
                };
                fixture.hef.state_get(&latest, state_key)
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&members);
            b.iter(|| {
                fixture
                    .sqlite
                    .current_state_get(STATE_TYPES[MEMBER as usize], &user(next()))
            });
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/state_at_event");
    for fixture in fixtures {
        let probes: Vec<(u64, u32)> = fixture
            .room
            .probes(4096, 8)
            .iter()
            .map(|sn| (*sn, (sn % u64::from(fixture.room.members)) as u32))
            .collect();
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&probes);
            b.iter(|| {
                let (sn, member) = next();
                let (state_key, _) = fixture.member_key(member);
                fixture.hef.state_get(&fixture.hef.root_at(sn), state_key)
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&probes);
            b.iter(|| {
                let (sn, member) = next();
                fixture.sqlite.state_at(sn, STATE_TYPES[MEMBER as usize], &user(member))
            });
        });
    }
    group.finish();

    let mut group = read_group(criterion, "read/full_state");
    for fixture in fixtures {
        group.throughput(Throughput::Elements(fixture.room.current.len() as u64));
        let (empty, latest) = (fixture.hef.state.empty, fixture.hef.state.latest.root);
        group.bench_function(fixture.id(HEF), |b| b.iter(|| fixture.hef.state_diff(&empty, &latest)));
        group.bench_function(fixture.id(SQLITE), |b| b.iter(|| fixture.sqlite.full_state()));
    }
    group.finish();

    let mut group = read_group(criterion, "read/state_delta");
    for fixture in fixtures {
        let span = fixture.size / 10;
        let probes: Vec<u64> = fixture.room.probes(4096, 9).iter().map(|sn| (*sn).max(span)).collect();
        group.bench_function(fixture.id(HEF), |b| {
            let mut next = cycle(&probes);
            b.iter(|| {
                let upto = next();
                fixture
                    .hef
                    .state_diff(&fixture.hef.root_at(upto - span), &fixture.hef.root_at(upto))
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut next = cycle(&probes);
            b.iter(|| {
                let upto = next();
                fixture.sqlite.state_diff(upto - span, upto)
            });
        });
    }
    group.finish();
}

/// Update: a membership change (the new state written with the event) and a redaction (the event's JSON replaced by
/// its redacted form).
fn update(criterion: &mut Criterion, fixtures: &[Fixture]) {
    let mut group = write_group(criterion, "update/set_state");
    for fixture in fixtures {
        let room = &fixture.room;
        let next = room.events.len() as u64;
        let change = |appended: u64| -> Event {
            let member = (appended % u64::from(room.members)) as u32;
            room.next_membership(appended, member, appended / u64::from(room.members) % 2 == 1)
        };
        group.bench_function(fixture.id(HEF), |b| {
            let mut journal = HefJournal::open();
            let mut chunks = ChunkStore(fixture.hef.state.chunks.0.clone());
            let mut root = fixture.hef.state.latest.root;
            let mut appended = 0;
            b.iter(|| {
                let event = change(appended);
                let sn = next + appended;
                journal.append(sn, std::slice::from_ref(&event));
                let changes = BTreeMap::from([(event.state().unwrap(), Some(sn))]);
                let update = update_state_tree(&chunks, &fixture.hef.state.key, &root, &changes).unwrap();
                for chunk in update.new_chunks {
                    chunks.0.insert(chunk.address, chunk.bytes);
                }
                root = update.root;
                appended += 1;
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut db = fixture.sqlite.copy();
            let mut appended = 0;
            b.iter(|| {
                db.append(next + appended, std::slice::from_ref(&change(appended)));
                appended += 1;
            });
        });
    }
    group.finish();

    let mut group = write_group(criterion, "update/redact");
    for fixture in fixtures {
        let room = &fixture.room;
        let targets = room.probes(4096, 10);
        let redacted = |sn: u64| -> Event {
            let mut event = room.events[sn as usize].clone();
            event.json = format!(
                "{{\"content\":{{}},\"depth\":{},\"event_id\":\"{}\",\"room_id\":\"{ROOM_ID}\",\"sender\":\"{}\",\
                 \"type\":\"{}\",\"unsigned\":{{\"redacted_because\":{{}}}}}}",
                event.depth,
                event.event_id,
                event.sender,
                event.type_name()
            )
            .into_bytes();
            event
        };
        group.bench_function(fixture.id(HEF), |b| {
            let mut journal = HefJournal::open();
            let mut deleted = DeletionVector::new();
            let mut next = cycle(&targets);
            b.iter(|| {
                // The redacted form is appended as the row that replaces the original, and the original's row is
                // marked deleted so the file never serves it again. An event already redacted would get the same
                // redacted form again, so nothing is written, as SQLite writes nothing for an UPDATE that leaves the
                // row unchanged.
                let sn = next();
                let row = fixture.hef.file.graph_row_of(sn).unwrap();
                if !deleted.is_deleted(row) {
                    journal.append(sn, std::slice::from_ref(&redacted(sn)));
                    deleted.mark_deleted(row);
                }
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut db = fixture.sqlite.copy();
            let mut next = cycle(&targets);
            b.iter(|| {
                let sn = next();
                let event = redacted(sn);
                let transaction = db.db.transaction().unwrap();
                transaction
                    .prepare_cached("UPDATE events SET json = ?1 WHERE event_id = ?2")
                    .unwrap()
                    .execute(params![event.json, event.event_id])
                    .unwrap();
                transaction.commit().unwrap();
            });
        });
    }
    group.finish();
}

/// Delete: one event removed for good (an admin delete), and the older half of a room purged (purge history).
fn delete(criterion: &mut Criterion, fixtures: &[Fixture]) {
    let mut group = write_group(criterion, "delete/event");
    for fixture in fixtures {
        let targets = fixture.room.probes(4096, 11);
        group.bench_function(fixture.id(HEF), |b| {
            let dir = tempfile::tempdir().unwrap();
            let mut out = File::create(dir.path().join("room.dv")).unwrap();
            let mut deleted = DeletionVector::new();
            let mut next = cycle(&targets);
            b.iter(|| {
                deleted.mark_deleted(fixture.hef.file.graph_row_of(next()).unwrap());
                write_durably(&mut out, &encode_deletion_vector(&deleted).1);
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            let mut db = fixture.sqlite.copy();
            let mut next = cycle(&targets);
            b.iter(|| {
                let sn = next() as i64;
                let transaction = db.db.transaction().unwrap();
                for statement in [
                    "DELETE FROM events WHERE sn = ?1",
                    "DELETE FROM event_edges WHERE event_sn = ?1",
                    "DELETE FROM event_auth WHERE event_sn = ?1",
                ] {
                    transaction.prepare_cached(statement).unwrap().execute([sn]).unwrap();
                }
                transaction.commit().unwrap();
            });
        });
    }
    group.finish();

    let mut group = heavy_group(criterion, "delete/purge_history");
    for fixture in fixtures {
        let cut = fixture.size / 2;
        group.throughput(Throughput::Elements(cut));
        group.bench_function(fixture.id(HEF), |b| {
            let dir = tempfile::tempdir().unwrap();
            let mut out = File::create(dir.path().join("room.dv")).unwrap();
            b.iter(|| {
                let mut deleted = DeletionVector::new();
                for sn in 0..cut {
                    deleted.mark_deleted(fixture.hef.file.graph_row_of(sn).unwrap());
                }
                write_durably(&mut out, &encode_deletion_vector(&deleted).1);
            });
        });
        group.bench_function(fixture.id(SQLITE), |b| {
            b.iter_batched(
                || fixture.sqlite.copy(),
                |mut db| {
                    let transaction = db.db.transaction().unwrap();
                    for statement in [
                        "DELETE FROM events WHERE sn < ?1",
                        "DELETE FROM event_edges WHERE event_sn < ?1",
                        "DELETE FROM event_auth WHERE event_sn < ?1",
                        "DELETE FROM state_deltas WHERE sn < ?1",
                    ] {
                        transaction.execute(statement, [cut as i64]).unwrap();
                    }
                    transaction.commit().unwrap();
                    db
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// Checks that both backends answer every read the same way before anything is timed.
fn check(fixture: &Fixture) {
    let (hef, sqlite, room) = (&fixture.hef, &fixture.sqlite, &fixture.room);
    for sn in room.probes(64, 99) {
        let id = &room.events[sn as usize].event_id;
        assert_eq!(hef.event_by_id(id), sqlite.event_by_id(id));
        assert_eq!(hef.auth_chain(sn), sqlite.auth_chain(sn));
        let mut children = sqlite.children(sn);
        children.sort_unstable();
        assert_eq!(hef.children(sn), children);
        if sn >= BATCH {
            assert_eq!(hef.backfill(sn, BATCH), sqlite.backfill(sn, BATCH));
            assert_eq!(hef.page_before(sn, PAGE), sqlite.page_before(sn, PAGE));
        }
        let member = (sn % u64::from(room.members)) as u32;
        let (state_key, name) = fixture.member_key(member);
        assert_eq!(
            hef.state_get(&hef.root_at(sn), state_key),
            sqlite.state_at(sn, STATE_TYPES[MEMBER as usize], &name)
        );
    }
    assert_eq!(
        hef.state_diff(&hef.state.empty, &hef.state.latest.root).len(),
        sqlite.full_state().len()
    );
}

/// Where Criterion writes its results, found as Criterion finds it.
fn criterion_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("CRITERION_HOME") {
        return PathBuf::from(home);
    }
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("criterion")
}

/// One result Criterion wrote during this run: group, backend, room size and mean time in nanoseconds.
struct Measured {
    backend: String,
    group: String,
    mean_nanos: f64,
    size: u64,
}

fn collect(dir: &Path, since: SystemTime, out: &mut Vec<Measured>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let benchmark = path.join("new").join("benchmark.json");
        let estimates = path.join("new").join("estimates.json");
        let fresh = std::fs::metadata(&estimates)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified >= since);
        if fresh
            && let (Ok(benchmark), Ok(estimates)) =
                (std::fs::read_to_string(&benchmark), std::fs::read_to_string(&estimates))
        {
            let benchmark: serde_json::Value = serde_json::from_str(&benchmark).unwrap_or_default();
            let estimates: serde_json::Value = serde_json::from_str(&estimates).unwrap_or_default();
            if let (Some(group), Some(backend), Some(size), Some(mean)) = (
                benchmark["group_id"].as_str(),
                benchmark["function_id"].as_str(),
                benchmark["value_str"].as_str().and_then(|size| size.parse().ok()),
                estimates["mean"]["point_estimate"].as_f64(),
            ) {
                out.push(Measured {
                    backend: backend.to_owned(),
                    group: group.to_owned(),
                    mean_nanos: mean,
                    size,
                });
            }
        }
        collect(&path, since, out);
    }
}

fn format_time(nanos: f64) -> String {
    match nanos {
        n if n >= 1e9 => format!("{:.2} s", n / 1e9),
        n if n >= 1e6 => format!("{:.2} ms", n / 1e6),
        n if n >= 1e3 => format!("{:.2} us", n / 1e3),
        n => format!("{n:.0} ns"),
    }
}

/// Prints, and writes beside Criterion's own reports, one table comparing the backends on every operation measured
/// in this run, and one of each room's stored bytes.
fn summarise(since: SystemTime, footprints: &[(u64, u64, u64, u64)], groups: &[&str]) {
    let dir = criterion_dir();
    let mut measured = Vec::new();
    collect(&dir, since, &mut measured);
    if measured.is_empty() {
        return;
    }
    let mut table = String::from(
        "| Operation | Room events | HEF mean | SQLite mean | SQLite / HEF |\n|---|---:|---:|---:|---:|\n",
    );
    for group in groups {
        for size in ROOM_SIZES {
            let mean = |backend: &str| {
                measured
                    .iter()
                    .find(|m| m.group == *group && m.backend == backend && m.size == size)
                    .map(|m| m.mean_nanos)
            };
            let (hef, sqlite) = (mean(HEF), mean(SQLITE));
            if hef.is_none() && sqlite.is_none() {
                continue;
            }
            let ratio = match (hef, sqlite) {
                (Some(hef), Some(sqlite)) => format!("{:.2}x", sqlite / hef),
                _ => "-".to_owned(),
            };
            let show = |value: Option<f64>| value.map_or("-".to_owned(), format_time);
            table.push_str(&format!(
                "| {group} | {size} | {} | {} | {ratio} |\n",
                show(hef),
                show(sqlite)
            ));
        }
    }
    table.push_str(
        "\nSQLite / HEF above 1 means HEF is faster.\n\n| Room events | HEF file bytes | HEF state chunk bytes | \
         HEF total bytes | SQLite file bytes |\n|---:|---:|---:|---:|---:|\n",
    );
    for (size, file, chunks, sqlite) in footprints {
        table.push_str(&format!(
            "| {size} | {file} | {chunks} | {} | {sqlite} |\n",
            file + chunks
        ));
    }
    println!("\nMatrix storage benchmark summary (mean time per operation)\n\n{table}");
    let summary = dir.join("matrix-summary.md");
    if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&summary, &table).is_ok() {
        println!("Written to {}", summary.display());
    }
}

const GROUPS: [&str; 18] = [
    "create/append_event",
    "create/append_batch",
    "create/import_room",
    "read/event_by_id",
    "read/event_batch",
    "read/timeline_latest",
    "read/paginate_back",
    "read/backfill_walk",
    "read/auth_chain",
    "read/event_children",
    "read/current_state_get",
    "read/state_at_event",
    "read/full_state",
    "read/state_delta",
    "update/set_state",
    "update/redact",
    "delete/event",
    "delete/purge_history",
];

fn main() {
    let since = SystemTime::now();
    let fixtures: Vec<Fixture> = ROOM_SIZES.iter().map(|size| Fixture::build(*size)).collect();
    for fixture in &fixtures {
        check(fixture);
    }
    let footprints: Vec<(u64, u64, u64, u64)> = fixtures
        .iter()
        .map(|f| (f.size, f.hef.file_bytes, f.hef.state.chunk_bytes, f.sqlite.file_bytes()))
        .collect();
    let mut criterion = Criterion::default().configure_from_args();
    create(&mut criterion, &fixtures);
    read(&mut criterion, &fixtures);
    update(&mut criterion, &fixtures);
    delete(&mut criterion, &fixtures);
    criterion.final_summary();
    summarise(since, &footprints, &GROUPS);
}
