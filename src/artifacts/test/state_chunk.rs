use super::*;
use crate::benchmarks::{StateChunkWriteMeasurement, state_chunk_write_gate_passes};
use crate::security::TenantDek;
use std::cell::Cell;
use std::collections::HashMap;

const CREATE: u32 = 1;
const MEMBER: u32 = 2;
const POWER_LEVELS: u32 = 3;

/// A chunk store that counts every read, so a test can prove which chunks a lookup or a diff fetched.
#[derive(Default)]
struct CountingChunks {
    chunks: HashMap<[u8; 32], Vec<u8>>,
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

fn key() -> StateChunkKey {
    StateChunkKey::derive(&TenantDek::new([7; 32]))
}

fn member(user: u32) -> StateKey {
    StateKey {
        state_key_id: user,
        type_id: MEMBER,
    }
}

/// A room's state: its create event, its power levels, and one membership per member.
fn room(members: u32) -> BTreeMap<StateKey, u64> {
    let mut state = BTreeMap::new();
    state.insert(
        StateKey {
            state_key_id: 0,
            type_id: CREATE,
        },
        0,
    );
    state.insert(
        StateKey {
            state_key_id: 0,
            type_id: POWER_LEVELS,
        },
        1,
    );
    for user in 0..members {
        state.insert(member(user), u64::from(user) + 2);
    }
    state
}

fn build(state: &BTreeMap<StateKey, u64>) -> StateTree {
    build_state_tree(&key(), state).unwrap()
}

/// A small deterministic random source, so every run sees the same workload.
fn xorshift(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}

#[test]
fn identical_maps_give_identical_roots() {
    let state = room(20_000);
    let first = build(&state);
    assert_eq!(build(&state.clone()), first);
    assert_eq!(first.root.entry_count, 20_002);

    let mut changed = state;
    changed.insert(member(5), 99);
    assert_ne!(build(&changed).root, first.root);

    let empty = build(&BTreeMap::new());
    assert_eq!(empty.chunks.len(), 1);
    assert_eq!(empty.root.entry_count, 0);
    assert_ne!(empty.root.address, first.root.address);
}

#[test]
fn chunks_hold_4_to_16_kib_of_entries() {
    let tree = build(&room(100_000));
    let mut small_per_level: HashMap<u8, usize> = HashMap::new();
    for chunk in &tree.chunks {
        let plaintext = key().open(&chunk.address, &chunk.bytes).unwrap();
        let entry_bytes = plaintext.len() - STATE_CHUNK_HEADER_LEN - STATE_CHUNK_CRC_LEN;
        assert!(entry_bytes <= STATE_CHUNK_MAX_BYTES);
        let level = decode_state_chunk(&plaintext).unwrap().level();
        if entry_bytes < STATE_CHUNK_MIN_BYTES {
            *small_per_level.entry(level).or_default() += 1;
        }
    }
    assert!(
        small_per_level.values().all(|&small| small <= 1),
        "only the last chunk of a level may be under the minimum: {small_per_level:?}"
    );
    let average = tree.chunks.iter().map(|chunk| chunk.bytes.len()).sum::<usize>() / tree.chunks.len();
    assert!(
        (4096..=16384).contains(&average),
        "average sealed chunk {average} bytes"
    );
}

#[test]
fn a_one_key_change_rewrites_one_to_three_chunks() {
    let state = room(100_000);
    let base = build(&state);
    let mut updated = state.clone();
    updated.insert(member(51_234), 7);
    let mut inserted = state.clone();
    inserted.insert(member(100_500), 7);
    let mut removed = state;
    removed.remove(&member(77));
    for changed in [updated, inserted, removed] {
        let new = build(&changed).new_chunks(&base).len();
        assert!((1..=3).contains(&new), "{new} chunks rewritten");
    }
}

#[test]
fn an_update_in_place_matches_a_full_build() {
    let mut random = xorshift(0x0DD5_EED5);
    let mut state = BTreeMap::new();
    let mut tree = build(&state);
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let set = |user: u32, sn: u64| (member(user), Some(sn));
    let mut rounds: Vec<BTreeMap<StateKey, Option<u64>>> = vec![
        // Grow from the empty leaf to one leaf, then two levels, then three.
        (0..20).map(|user| set(user, 1)).collect(),
        (20..2_000).map(|user| set(user, 2)).collect(),
        (2_000..120_000).map(|user| set(user, 3)).collect(),
    ];
    // Churn: values changed, keys inserted and removed (present or not), and keys before and after every other.
    for round in 0..12u64 {
        let mut changes: BTreeMap<StateKey, Option<u64>> = (0..1 + random() % 60)
            .map(|_| {
                let user = (random() % 125_000) as u32;
                (member(user), (!random().is_multiple_of(3)).then_some(round + 10))
            })
            .collect();
        changes.insert(StateKey::default(), (round % 2 == 0).then_some(round));
        changes.insert(
            StateKey {
                state_key_id: u32::MAX,
                type_id: u32::MAX,
            },
            (round % 3 == 0).then_some(round),
        );
        rounds.push(changes);
    }
    // Shrink back down a level, then to the empty map, then remove a key the empty map lacks.
    rounds.push((1_000..125_000).map(|user| (member(user), None)).collect());
    rounds.push(
        (0..1_000)
            .map(member)
            .chain([
                StateKey::default(),
                StateKey {
                    state_key_id: u32::MAX,
                    type_id: u32::MAX,
                },
            ])
            .map(|state_key| (state_key, None))
            .collect(),
    );
    rounds.push(BTreeMap::from([(member(5), None)]));

    for changes in rounds {
        let update = update_state_tree(&store, &key(), &tree.root, &changes).unwrap();
        for (state_key, sn) in &changes {
            match sn {
                Some(sn) => drop(state.insert(*state_key, *sn)),
                None => drop(state.remove(state_key)),
            }
        }
        let full = build(&state);
        assert_eq!(update.root, full.root);
        assert_eq!(update.new_chunks, full.new_chunks(&tree));
        store.store(&update.new_chunks);
        tree = full;
    }
    assert_eq!(tree.root.entry_count, 0);
}

#[test]
fn an_update_reads_every_interior_chunk_and_only_the_leaf_it_changes() {
    let tree = build(&room(100_000));
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let interior = tree
        .chunks
        .iter()
        .filter(|chunk| {
            let plaintext = key().open(&chunk.address, &chunk.bytes).unwrap();
            decode_state_chunk(&plaintext).unwrap().level() > 0
        })
        .count();
    assert!(interior < 5, "{interior} interior chunks");

    let update = update_state_tree(&store, &key(), &tree.root, &BTreeMap::from([(member(51_234), Some(7))])).unwrap();
    assert_eq!(store.take_reads(), interior + 1);
    assert!(
        (2..=3).contains(&update.new_chunks.len()),
        "{} chunks",
        update.new_chunks.len()
    );
}

#[test]
fn state_get_reads_one_chunk_per_level() {
    let state = room(100_000);
    let tree = build(&state);
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let depth = {
        state_get(&store, &key(), &tree.root, member(0)).unwrap();
        store.take_reads()
    };
    assert!(depth <= 3, "depth {depth}");
    for user in [0, 1, 4_095, 50_000, 99_999] {
        assert_eq!(
            state_get(&store, &key(), &tree.root, member(user)).unwrap(),
            Some(u64::from(user) + 2)
        );
        assert_eq!(store.take_reads(), depth);
    }
    for absent in [
        member(100_000),
        StateKey::default(),
        StateKey {
            state_key_id: 1,
            type_id: CREATE,
        },
    ] {
        assert_eq!(state_get(&store, &key(), &tree.root, absent).unwrap(), None);
    }
    let empty = build(&BTreeMap::new());
    store.store(&empty.chunks);
    assert_eq!(state_get(&store, &key(), &empty.root, member(0)).unwrap(), None);
}

#[test]
fn a_diff_skips_the_subtrees_both_states_share() {
    let state = room(100_000);
    let before = build(&state);
    let mut changed = state;
    changed.insert(member(42_000), 5);
    let after = build(&changed);
    let mut store = CountingChunks::default();
    store.store(&before.chunks);
    store.store(&after.chunks);

    let changes = state_diff(&store, &key(), &before.root, &after.root).unwrap();
    assert_eq!(changes, [change(member(42_000), Some(42_002), Some(5))]);
    let reads = store.take_reads();
    assert!(reads <= 6, "{reads} chunk reads");
    assert!(before.chunks.len() > 100, "the tree has {} chunks", before.chunks.len());

    assert!(state_diff(&store, &key(), &after.root, &after.root).unwrap().is_empty());
    assert_eq!(store.take_reads(), 0);
}

#[test]
fn a_diff_matches_comparing_the_maps_key_by_key() {
    let mut random = xorshift(0x5EED);
    let mut before = room(30_000);
    for round in 0..6 {
        let mut after = before.clone();
        for _ in 0..(1 + random() % 40) {
            let user = (random() % 32_000) as u32;
            match random() % 3 {
                0 => drop(after.remove(&member(user))),
                _ => drop(after.insert(member(user), random() % 1_000_000)),
            }
        }
        if round == 5 {
            after.clear();
        }
        let old = build(&before);
        let new = build(&after);
        let mut store = CountingChunks::default();
        store.store(&old.chunks);
        store.store(&new.chunks);

        let mut expected = Vec::new();
        for key in before
            .keys()
            .chain(after.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let (was, is) = (before.get(key).copied(), after.get(key).copied());
            if was != is {
                expected.push(change(*key, was, is));
            }
        }
        assert_eq!(state_diff(&store, &key(), &old.root, &new.root).unwrap(), expected);
        let reversed: Vec<StateChange> = expected.iter().map(|c| change(c.key, c.after, c.before)).collect();
        assert_eq!(state_diff(&store, &key(), &new.root, &old.root).unwrap(), reversed);
        before = after;
    }
}

#[test]
fn a_missing_altered_or_malformed_chunk_is_refused() {
    let tree = build(&room(10_000));
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let mut missing = CountingChunks::default();
    missing.store(tree.chunks.split_last().unwrap().1);
    assert!(matches!(
        state_get(&missing, &key(), &tree.root, member(1)),
        Err(StateChunkError::Missing { .. })
    ));

    let mut altered = CountingChunks::default();
    altered.store(&tree.chunks);
    altered.chunks.get_mut(&tree.root.address).unwrap()[0] ^= 1;
    assert!(matches!(
        state_get(&altered, &key(), &tree.root, member(1)),
        Err(StateChunkError::Unauthentic { .. })
    ));

    let other_tenant = StateChunkKey::derive(&TenantDek::new([8; 32]));
    assert!(matches!(
        state_get(&store, &other_tenant, &tree.root, member(1)),
        Err(StateChunkError::Unauthentic { .. })
    ));

    // Chunks that seal and open correctly but break the format: keys out of order, a wrong CRC, a nonzero reserved
    // byte.
    let leaf = |entries: &[(u32, u64)]| {
        let mut out = Writer::new();
        out.put_slice(&STATE_CHUNK_MAGIC);
        out.put_u8(0);
        out.put_slice(&[0; 3]);
        out.put_u32(entries.len() as u32);
        for (user, sn) in entries {
            out.put_u32(MEMBER);
            out.put_u32(*user);
            out.put_u64(*sn);
        }
        out.put_u64(crc64_nvme(out.bytes()));
        out.into_bytes()
    };
    let unsorted = leaf(&[(2, 1), (1, 1)]);
    let mut bad_crc = leaf(&[(1, 1)]);
    *bad_crc.last_mut().unwrap() ^= 1;
    let mut reserved = leaf(&[(1, 1)]);
    reserved[5] = 1;
    for plaintext in [unsorted, bad_crc, reserved] {
        let (address, bytes) = key().seal(&plaintext).unwrap();
        let mut store = CountingChunks::default();
        store.chunks.insert(address, bytes);
        let root = StateRoot {
            address,
            entry_count: 1,
        };
        assert!(matches!(
            state_get(&store, &key(), &root, member(1)),
            Err(StateChunkError::Format(_))
        ));
    }
}

#[test]
fn a_cache_answers_as_state_get_and_fetches_each_chunk_once() {
    let state = room(100_000);
    let tree = build(&state);
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let depth = {
        state_get(&store, &key(), &tree.root, member(0)).unwrap();
        store.take_reads()
    };
    assert!(depth >= 2, "depth {depth}");
    let cache = StateChunkCache::new(key(), u64::MAX);

    assert_eq!(cache.state_get(&store, &tree.root, member(0)).unwrap(), Some(2));
    assert_eq!(store.take_reads(), depth);
    assert_eq!(cache.state_get(&store, &tree.root, member(0)).unwrap(), Some(2));
    assert_eq!(store.take_reads(), 0);
    // Another leaf under the same root: the root is not fetched again.
    assert_eq!(
        cache.state_get(&store, &tree.root, member(99_999)).unwrap(),
        Some(100_001)
    );
    let reads = store.take_reads();
    assert!((1..depth).contains(&reads), "{reads} chunk reads");

    for user in [1, 4_095, 50_000, 99_999] {
        assert_eq!(
            cache.state_get(&store, &tree.root, member(user)).unwrap(),
            state_get(&store, &key(), &tree.root, member(user)).unwrap()
        );
    }
    for absent in [member(100_000), StateKey::default()] {
        assert_eq!(cache.state_get(&store, &tree.root, absent).unwrap(), None);
    }

    // A later state shares every chunk but the ones its change rewrote, so only those are fetched.
    let mut changed = state;
    changed.insert(member(42_000), 5);
    let after = build(&changed);
    let new_chunks = after.new_chunks(&tree);
    store.store(&new_chunks);
    store.take_reads();
    assert_eq!(cache.state_get(&store, &after.root, member(42_000)).unwrap(), Some(5));
    assert_eq!(store.take_reads(), new_chunks.len());
    assert_eq!(cache.state_get(&store, &after.root, member(0)).unwrap(), Some(2));
    assert_eq!(store.take_reads(), 0);
}

#[test]
fn a_cache_keeps_no_more_than_its_budget() {
    let tree = build(&room(100_000));
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let depth = {
        state_get(&store, &key(), &tree.root, member(0)).unwrap();
        store.take_reads()
    };
    assert!(depth >= 2, "depth {depth}");
    // With no budget it keeps only the chunk read last, so each lookup fetches every level again.
    let cache = StateChunkCache::new(key(), 0);
    for _ in 0..3 {
        assert_eq!(cache.state_get(&store, &tree.root, member(7)).unwrap(), Some(9));
        assert_eq!(store.take_reads(), depth);
    }
}

#[test]
fn a_cache_refuses_what_state_get_refuses() {
    let tree = build(&room(10_000));
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let mut missing = CountingChunks::default();
    missing.store(tree.chunks.split_last().unwrap().1);
    assert!(matches!(
        StateChunkCache::new(key(), u64::MAX).state_get(&missing, &tree.root, member(1)),
        Err(StateChunkError::Missing { .. })
    ));
    let other_tenant = StateChunkKey::derive(&TenantDek::new([8; 32]));
    assert!(matches!(
        StateChunkCache::new(other_tenant, u64::MAX).state_get(&store, &tree.root, member(1)),
        Err(StateChunkError::Unauthentic { .. })
    ));

    // A kept leaf named as a child one level above a leaf is refused, as an unkept one is.
    let seal = |level: u8, entry: &[u8]| {
        let mut out = Writer::new();
        out.put_slice(&STATE_CHUNK_MAGIC);
        out.put_u8(level);
        out.put_slice(&[0; 3]);
        out.put_u32(1);
        out.put_u32(MEMBER);
        out.put_u32(1);
        out.put_slice(entry);
        out.put_u64(crc64_nvme(out.bytes()));
        key().seal(out.bytes()).unwrap()
    };
    let (leaf, leaf_bytes) = seal(0, &1u64.to_le_bytes());
    let (interior, interior_bytes) = seal(2, &leaf);
    let mut store = CountingChunks::default();
    store.chunks.insert(leaf, leaf_bytes);
    store.chunks.insert(interior, interior_bytes);
    let root = |address| StateRoot {
        address,
        entry_count: 1,
    };
    let cache = StateChunkCache::new(key(), u64::MAX);
    assert_eq!(cache.state_get(&store, &root(leaf), member(1)).unwrap(), Some(1));
    for answer in [
        cache.state_get(&store, &root(interior), member(1)),
        state_get(&store, &key(), &root(interior), member(1)),
    ] {
        assert!(matches!(answer, Err(StateChunkError::Format(_))));
    }
}

/// Checkpointed delta chains, the alternative the prolly tree is measured against: frame `k` stores the full map when
/// `k` is a multiple of 64 and otherwise only the keys that changed since frame `k - 1`. Each frame is one object,
/// framed and sealed like a leaf chunk so both designs carry the same per-object overhead.
#[derive(Default)]
struct DeltaChain {
    frames: Vec<SealedStateChunk>,
}

impl DeltaChain {
    const CHECKPOINT_EVERY: usize = 64;

    /// Stores the next frame and returns its size in bytes.
    fn push(&mut self, state: &BTreeMap<StateKey, u64>, changed: &BTreeMap<StateKey, u64>) -> usize {
        let source = if self.frames.len().is_multiple_of(Self::CHECKPOINT_EVERY) {
            state
        } else {
            changed
        };
        let entries: Vec<(StateKey, [u8; 8])> = source.iter().map(|(key, sn)| (*key, sn.to_le_bytes())).collect();
        seal_chunk(&key(), 0, &entries, &mut self.frames).unwrap();
        self.frames.last().unwrap().bytes.len()
    }

    /// The objects, and their bytes, a cold reader fetches to look one key up at frame `frame`: the deltas back to
    /// the checkpoint, newest first, until one holds the key, else the checkpoint too.
    fn lookup_fetches(&self, frame: usize, state_key: StateKey) -> (usize, usize) {
        let checkpoint = frame - frame % Self::CHECKPOINT_EVERY;
        let (mut fetches, mut bytes) = (0, 0);
        for object in self.frames[checkpoint..=frame].iter().rev() {
            fetches += 1;
            bytes += object.bytes.len();
            let plaintext = key().open(&object.address, &object.bytes).unwrap();
            let StateChunk::Leaf(entries) = decode_state_chunk(&plaintext).unwrap() else {
                panic!("a frame is one leaf");
            };
            if entries.iter().any(|(entry_key, _)| *entry_key == state_key) {
                break;
            }
        }
        (fetches, bytes)
    }
}

/// Prototype comparison for issue #24: a 100,000-member room takes 1,024 state events after its initial state (70%
/// membership changes of existing members, 20% new joins, 10% power-level changes), each one a new state root.
/// Measures the bytes each design writes for those events and what a cold reader fetches to look one member up at
/// every frame, and to rebuild the whole state at one frame.
#[test]
#[ignore = "prototype measurement; run with --release -- --ignored --nocapture"]
fn prolly_trees_against_checkpointed_delta_chains() {
    const MEMBERS: u32 = 100_000;
    const EVENTS: u64 = 1_024;
    let mut random = xorshift(0xC0FF_EE00);
    let mut state = room(MEMBERS);
    let mut users = MEMBERS;
    let mut tree = build(&state);
    let mut store = CountingChunks::default();
    store.store(&tree.chunks);
    let mut chain = DeltaChain::default();
    let initial_prolly: usize = tree.chunks.iter().map(|chunk| chunk.bytes.len()).sum();
    let initial_chain = chain.push(&state, &BTreeMap::new());

    let mut roots = vec![tree.root];
    let (mut prolly_bytes, mut prolly_objects, mut chain_bytes) = (0, 0, 0);
    for event in 1..=EVENTS {
        let sn = u64::from(MEMBERS) + 2 + event;
        let changed_key = match random() % 10 {
            0..=6 => member((random() % u64::from(users)) as u32),
            7 | 8 => {
                users += 1;
                member(users - 1)
            }
            _ => StateKey {
                state_key_id: 0,
                type_id: POWER_LEVELS,
            },
        };
        state.insert(changed_key, sn);
        let next = build(&state);
        let new = next.new_chunks(&tree);
        prolly_bytes += new.iter().map(|chunk| chunk.bytes.len()).sum::<usize>();
        prolly_objects += new.len();
        store.store(&new);
        tree = next;
        roots.push(tree.root);
        chain_bytes += chain.push(&state, &BTreeMap::from([(changed_key, sn)]));
    }

    let (mut prolly_fetches, mut prolly_read_bytes, mut chain_fetches, mut chain_read_bytes) = (0, 0, 0, 0);
    for (frame, root) in roots.iter().enumerate() {
        let looked_up = member((random() % u64::from(MEMBERS)) as u32);
        let before = store.reads.get();
        state_get(&store, &key(), root, looked_up).unwrap();
        prolly_fetches += store.reads.get() - before;
        prolly_read_bytes += descent_bytes(&store, root, looked_up);
        let (fetches, bytes) = chain.lookup_fetches(frame, looked_up);
        chain_fetches += fetches;
        chain_read_bytes += bytes;
    }
    let frames = roots.len();
    let full_state_chunks = tree.chunks.len();
    let full_state_chain: f64 = (0..frames)
        .map(|frame| (frame % DeltaChain::CHECKPOINT_EVERY + 1) as f64)
        .sum::<f64>()
        / frames as f64;

    println!("initial state: prolly {initial_prolly} B in {full_state_chunks} chunks, delta chain {initial_chain} B");
    println!(
        "{EVENTS} state events written: prolly {prolly_bytes} B in {prolly_objects} new chunks ({:.0} B/event, \
         {:.2} chunks/event); delta chain {chain_bytes} B in {EVENTS} frames ({:.0} B/event)",
        prolly_bytes as f64 / EVENTS as f64,
        prolly_objects as f64 / EVENTS as f64,
        chain_bytes as f64 / EVENTS as f64,
    );
    println!(
        "cold one-member lookup, mean over {frames} frames: prolly {:.2} fetches / {:.0} B; delta chain {:.2} \
         fetches / {:.0} B",
        prolly_fetches as f64 / frames as f64,
        prolly_read_bytes as f64 / frames as f64,
        chain_fetches as f64 / frames as f64,
        chain_read_bytes as f64 / frames as f64,
    );
    println!(
        "cold whole state at one frame: prolly {full_state_chunks} fetches; delta chain {full_state_chain:.1} \
         fetches on average"
    );
    assert!(prolly_fetches < chain_fetches);
    assert!(state_chunk_write_gate_passes(&StateChunkWriteMeasurement {
        bytes_written: prolly_bytes as u64,
        events: EVENTS,
    }));
}

/// The bytes of the chunks a lookup of `state_key` at `root` reads.
fn descent_bytes(store: &CountingChunks, root: &StateRoot, state_key: StateKey) -> usize {
    let mut address = root.address;
    let mut bytes = 0;
    loop {
        let sealed = &store.chunks[&address];
        bytes += sealed.len();
        match decode_state_chunk(&key().open(&address, sealed).unwrap()).unwrap() {
            StateChunk::Leaf(_) => return bytes,
            StateChunk::Interior { children, .. } => {
                let after = children.partition_point(|(first, _)| *first <= state_key);
                address = children[after.max(1) - 1].1;
            }
        }
    }
}
