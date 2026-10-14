use super::*;
use crate::artifacts::state_chunk::{SealedStateChunk, StateKey, StateTree, build_state_tree, state_get};
use crate::clock::FixedClock;
use crate::events::SequenceRange;
use crate::layout::Profile;
use crate::lifecycle::{FileType, PartState};
use crate::object_store::sim::SimObjectStore;
use crate::security::TenantDek;
use crate::typed_id::TypedIdTestExt;
use crate::writer::sim::SimStateChunkLedger;
use crate::writer::upload::upload_state_chunks;
use std::collections::{BTreeMap, HashMap};

const HOUR: i64 = 60 * 60 * 1_000_000_000;

/// The roots each published file records, as a test hands them to a sweep.
#[derive(Default)]
struct RootsByFile(HashMap<u128, Vec<StateRoot>>);

impl FileStateRoots for RootsByFile {
    fn state_roots(&self, entry: &HefFileEntry) -> Result<Vec<StateRoot>, StateChunkError> {
        Ok(self.0.get(&entry.file_id).cloned().unwrap_or_default())
    }
}

/// Two successive states of a 4,000-member room, both uploaded at the start: `old` is recorded by no file, `new` by
/// one `Active` graph file in `head`.
struct World {
    clock: FixedClock,
    head: ManifestGeneration,
    ledger: SimStateChunkLedger,
    new: StateTree,
    objects: SimObjectStore,
    old: StateTree,
    roots: RootsByFile,
}

fn tenant() -> TenantId {
    TenantId::new_test_id(9)
}

fn key() -> StateChunkKey {
    StateChunkKey::derive(&TenantDek::new([5; 32]))
}

fn member(user: u32) -> StateKey {
    StateKey {
        state_key_id: user,
        type_id: 1,
    }
}

fn graph_file(file_id: u128, part_state: PartState) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence: 0,
            last_sequence: 99,
        },
        feature_metadata: None,
        file_seal: [0; 32],
        file_id,
        file_type: FileType::HefFile,
        footer_len: None,
        graph_event_id_filter: None,
        graph_summary: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state,
        profile: Profile::Graph,
        required_feature_flags: required_features::ALL
            | required_features::GRAPH_DENSE_IDS
            | required_features::GRAPH_CSR_FORWARD
            | required_features::GRAPH_CSR_REVERSE
            | required_features::GRAPH_STATE_ROOTS,
        size_bytes: 1,
        tenant_id: tenant(),
        tree_len: None,
    }
}

fn world() -> World {
    let mut state: BTreeMap<StateKey, u64> = (0..4_000).map(|user| (member(user), u64::from(user))).collect();
    let old = build_state_tree(&key(), &state).unwrap();
    state.insert(member(2_000), 9_999);
    let new = build_state_tree(&key(), &state).unwrap();
    let world = World {
        clock: FixedClock::at(1_760_000_000 * 1_000_000_000),
        head: ManifestGeneration {
            files: vec![graph_file(1, PartState::Active)],
            ..ManifestGeneration::default()
        },
        ledger: SimStateChunkLedger::default(),
        new,
        objects: SimObjectStore::new(),
        old,
        roots: RootsByFile(HashMap::new()),
    };
    upload(&world, &world.old.chunks).unwrap();
    upload(&world, world.new.new_chunks_iter(&world.old)).unwrap();
    let mut world = world;
    world.roots.0.insert(1, vec![world.new.root]);
    world
}

fn upload<'a>(world: &World, chunks: impl IntoIterator<Item = &'a SealedStateChunk>) -> Result<usize, StateChunkError> {
    upload_state_chunks(&world.objects, &world.ledger, tenant(), chunks, &world.clock)
}

fn sweep(world: &World) -> Result<StateChunkSweep, StateChunkError> {
    sweep_state_chunks(
        &world.head,
        tenant(),
        &world.roots,
        &key(),
        &world.ledger,
        &world.objects,
        &world.clock,
    )
}

/// The chunks only the unreferenced old state has, sorted.
fn orphans(world: &World) -> Vec<[u8; 32]> {
    let mut orphans: Vec<[u8; 32]> = world
        .old
        .new_chunks_iter(&world.new)
        .map(|chunk| chunk.address)
        .collect();
    orphans.sort_unstable();
    orphans
}

fn stored(world: &World, address: &[u8; 32]) -> bool {
    world
        .objects
        .object(&state_chunk_object_key(tenant(), address))
        .is_some()
}

fn read(world: &World, root: &StateRoot) -> Result<Option<u64>, StateChunkError> {
    let chunks = ObjectStoreStateChunks {
        objects: &world.objects,
        tenant_id: tenant(),
    };
    state_get(&chunks, &key(), root, member(2_000))
}

#[test]
fn a_referenced_chunk_is_never_deleted() {
    let mut world = world();
    assert!(!orphans(&world).is_empty());
    for part_state in [PartState::Active, PartState::Outdated, PartState::DeleteOnDestroy] {
        world.head.files = vec![graph_file(1, part_state)];
        world.clock.advance(30 * 24 * HOUR);
        let swept = sweep(&world).unwrap();
        assert_eq!(swept.marked, world.new.chunks.len() as u64);
        assert!(world.new.chunks.iter().all(|chunk| stored(&world, &chunk.address)));
        assert_eq!(read(&world, &world.new.root).unwrap(), Some(9_999));
    }
}

#[test]
fn an_unreferenced_chunk_inside_the_grace_period_is_kept() {
    let world = world();
    let objects = world.objects.keys().len();
    world.clock.advance(STATE_CHUNK_GRACE_NANOS - 1);
    assert!(sweep(&world).unwrap().deleted.is_empty());
    assert_eq!(world.objects.keys().len(), objects);
    assert_eq!(read(&world, &world.old.root).unwrap(), Some(2_000));
}

#[test]
fn an_orphan_is_deleted_once_the_grace_period_passes() {
    let world = world();
    world.clock.advance(STATE_CHUNK_GRACE_NANOS);
    let mut deleted = sweep(&world).unwrap().deleted;
    deleted.sort_unstable();
    assert_eq!(deleted, orphans(&world));
    assert!(deleted.iter().all(|address| !stored(&world, address)));
    let mut records = Vec::new();
    world.ledger.records_after(None, usize::MAX, &mut records).unwrap();
    let recorded: Vec<[u8; 32]> = records.iter().map(|record| record.address).collect();
    assert!(deleted.iter().all(|address| !recorded.contains(address)));
    assert!(matches!(
        read(&world, &world.old.root),
        Err(StateChunkError::Missing { .. })
    ));
    assert_eq!(read(&world, &world.new.root).unwrap(), Some(9_999));
    assert!(sweep(&world).unwrap().deleted.is_empty());
}

#[test]
fn reuse_restarts_the_grace_period_and_a_claimed_chunk_waits_for_its_sweep() {
    let world = world();
    world.clock.advance(STATE_CHUNK_GRACE_NANOS + HOUR);
    assert_eq!(
        upload(&world, &world.old.chunks).unwrap(),
        0,
        "every chunk is still stored"
    );
    assert!(sweep(&world).unwrap().deleted.is_empty());

    // A sweep that claimed an orphan and stopped before deleting it: an upload reusing it is refused until a sweep
    // finishes the deletion, after which the upload stores it afresh.
    world.clock.advance(STATE_CHUNK_GRACE_NANOS + HOUR);
    let orphan = *orphans(&world).last().unwrap();
    let unused_since = world.clock.now_nanos() - STATE_CHUNK_GRACE_NANOS;
    assert!(world.ledger.claim(&orphan, unused_since).unwrap());
    assert!(matches!(
        upload(&world, &world.old.chunks),
        Err(StateChunkError::BeingDeleted { .. })
    ));
    assert!(sweep(&world).unwrap().deleted.contains(&orphan));
    assert!(upload(&world, &world.old.chunks).unwrap() >= 1);
    assert_eq!(read(&world, &world.old.root).unwrap(), Some(2_000));
}

#[test]
fn a_missing_referenced_chunk_fails_the_sweep_before_anything_is_deleted() {
    let world = world();
    world.clock.advance(STATE_CHUNK_GRACE_NANOS);
    world
        .objects
        .delete(&state_chunk_object_key(tenant(), &world.new.root.address))
        .unwrap();
    assert!(matches!(sweep(&world), Err(StateChunkError::Missing { .. })));
    assert!(orphans(&world).iter().all(|address| stored(&world, address)));
}

/// A ledger whose records are only addresses `0..count` (big-endian in the last eight bytes) with no stored objects;
/// those below `recent_from` were last used long before `now`, the rest at `now`.
fn bare_ledger(count: u64, recent_from: u64, now: i64) -> SimStateChunkLedger {
    let ledger = SimStateChunkLedger::default();
    for i in 0..count {
        let used = if i < recent_from {
            now - 2 * STATE_CHUNK_GRACE_NANOS
        } else {
            now
        };
        ledger.touch(&bare_address(i), used).unwrap();
    }
    ledger
}

fn bare_address(i: u64) -> [u8; 32] {
    let mut address = [0; 32];
    address[24..].copy_from_slice(&i.to_be_bytes());
    address
}

fn bare_sweep(ledger: &dyn StateChunkLedger, clock: &FixedClock) -> StateChunkSweep {
    let objects = SimObjectStore::new();
    let head = ManifestGeneration::default();
    sweep_state_chunks(
        &head,
        tenant(),
        &RootsByFile::default(),
        &key(),
        ledger,
        &objects,
        clock,
    )
    .unwrap()
}

#[test]
fn a_sweep_over_several_pages_deletes_every_due_chunk_once_and_keeps_the_rest() {
    let now = 1_760_000_000 * 1_000_000_000;
    let clock = FixedClock::at(now);
    // Exact page multiples and one past, with every page's last record (the next page's cursor) deleted.
    for count in [
        STATE_CHUNK_SWEEP_PAGE_RECORDS as u64,
        2 * STATE_CHUNK_SWEEP_PAGE_RECORDS as u64,
        2 * STATE_CHUNK_SWEEP_PAGE_RECORDS as u64 + 1,
    ] {
        let due = count - 3;
        let ledger = bare_ledger(count, due, now);

        let sweep = bare_sweep(&ledger, &clock);

        let expected: Vec<[u8; 32]> = (0..due).map(bare_address).collect();
        assert_eq!(sweep.deleted, expected, "count {count}");
        let mut left = Vec::new();
        ledger.records_after(None, usize::MAX, &mut left).unwrap();
        let left: Vec<[u8; 32]> = left.iter().map(|record| record.address).collect();
        assert_eq!(left, (due..count).map(bare_address).collect::<Vec<_>>());
    }
}

/// A ledger that lets an upload reuse `reused` and add `added` just after the sweep reads its first page.
struct UploadDuringSweep {
    added: [u8; 32],
    ledger: SimStateChunkLedger,
    now: i64,
    reused: [u8; 32],
}

impl StateChunkLedger for UploadDuringSweep {
    fn touch(&self, address: &[u8; 32], now_nanos: i64) -> Result<bool, StorageError> {
        self.ledger.touch(address, now_nanos)
    }

    fn clear_marks(&self) -> Result<(), StorageError> {
        self.ledger.clear_marks()
    }

    fn mark(&self, address: &[u8; 32]) -> Result<bool, StorageError> {
        self.ledger.mark(address)
    }

    fn records_after(
        &self,
        after: Option<&[u8; 32]>,
        limit: usize,
        page: &mut Vec<StateChunkRecord>,
    ) -> Result<(), StorageError> {
        self.ledger.records_after(after, limit, page)?;
        if after.is_none() {
            assert!(self.ledger.touch(&self.reused, self.now)?);
            assert!(self.ledger.touch(&self.added, self.now)?);
        }
        Ok(())
    }

    fn claim(&self, address: &[u8; 32], unused_since_nanos: i64) -> Result<bool, StorageError> {
        self.ledger.claim(address, unused_since_nanos)
    }

    fn forget(&self, address: &[u8; 32]) -> Result<(), StorageError> {
        self.ledger.forget(address)
    }
}

#[test]
fn a_chunk_an_upload_uses_mid_sweep_is_kept() {
    let now = 1_760_000_000 * 1_000_000_000;
    let clock = FixedClock::at(now);
    let count = 2 * STATE_CHUNK_SWEEP_PAGE_RECORDS as u64;
    let reused = bare_address(1);
    let ledger = UploadDuringSweep {
        added: bare_address(count + 5),
        ledger: bare_ledger(count, count, now),
        now,
        reused,
    };

    let sweep = bare_sweep(&ledger, &clock);

    let expected: Vec<[u8; 32]> = (0..count).filter(|&i| i != 1).map(bare_address).collect();
    assert_eq!(sweep.deleted, expected);
    let mut left = Vec::new();
    ledger.records_after(None, usize::MAX, &mut left).unwrap();
    let left: Vec<[u8; 32]> = left.iter().map(|record| record.address).collect();
    assert_eq!(left, vec![reused, bare_address(count + 5)]);
}
