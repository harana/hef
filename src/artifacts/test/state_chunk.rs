use super::*;
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
        let source = if self.frames.len() % Self::CHECKPOINT_EVERY == 0 {
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
