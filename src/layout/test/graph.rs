use super::*;
use crate::artifacts::state_chunk::{StateKey, StateTree, build_state_tree};
use crate::benchmarks::{
    GraphChainCoverMeasurement, GraphEdgeBytesMeasurement, GraphEventIdIndexMeasurement, RoomReplayMeasurement,
    graph_chain_cover_gate_passes, graph_edge_bytes_gate_passes, graph_event_id_index_gate_passes,
    room_replay_gate_passes,
};
use crate::layout::{Profile, required_features};
use crate::security::{StateChunkKey, TenantDek};
use crate::writer::build::{HefBuildConfig, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};
use std::collections::HashMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// One way to break a valid graph input.
type GraphBreak = Box<dyn Fn(&mut GraphInput)>;

fn granule(granule_id: u32, first_row_ordinal: u64, row_count: u32) -> GranuleEntry {
    GranuleEntry {
        compressed_bytes_estimate: 0,
        first_epoch: 1,
        first_row_ordinal,
        first_sequence: 0,
        granule_id,
        last_epoch: 1,
        last_sequence: 0,
        max_ingested_at_physical: 0,
        max_occurred_at_physical: 0,
        min_ingested_at_physical: 0,
        min_occurred_at_physical: 0,
        row_count,
        stripe_id: 0,
    }
}

/// Six events from sequence number 100 in two granules of three: a prev chain, every event authorized by the first,
/// event 104 also naming 102, one parent (90) in an earlier file, and event 103 naming a prev not yet numbered.
fn graph() -> GraphInput {
    let row = |prev: &[u64], auth: &[u64]| RowEdges {
        auth: auth.to_vec(),
        prev: prev.to_vec(),
    };
    GraphInput {
        chain_cover: None,
        depth: None,
        first_sn: 100,
        inbound: vec![
            GraphEdge {
                child: 200,
                kind: EdgeKind::Prev,
                parent: 105,
            },
            GraphEdge {
                child: 150,
                kind: EdgeKind::Auth,
                parent: 100,
            },
        ],
        rows: vec![
            row(&[90], &[]),
            row(&[100], &[100]),
            row(&[101], &[100]),
            row(&[102], &[100]),
            row(&[103, 102], &[100]),
            row(&[104], &[100, 103]),
        ],
        state_roots: BTreeMap::new(),
        subject: b"!room:example.org".to_vec(),
        unresolved: vec![UnresolvedEdge {
            child: 103,
            parent: RelationshipRef::to_external(RelationshipKind::Prev, b"$late:example.org").unwrap(),
        }],
    }
}

fn decoded(graph: &GraphInput) -> Vec<GranuleEdges> {
    let granules = [granule(0, 0, 3), granule(1, 3, 3)];
    let blocks = encode_graph_blocks(graph, 6, &granules).unwrap();
    granules
        .iter()
        .map(|granule| {
            let first_sn = graph.first_sn + granule.first_row_ordinal;
            decode_granule_edges(&blocks[&granule.granule_id], first_sn, granule.row_count, 106, false).unwrap()
        })
        .collect()
}

#[test]
fn edges_round_trip_through_granule_blocks() {
    let granules = decoded(&graph());
    assert_eq!(granules[0].parents(100, EdgeKind::Prev), [90]);
    assert_eq!(granules[1].parents(104, EdgeKind::Prev), [103, 102]);
    assert_eq!(granules[1].parents(105, EdgeKind::Auth), [100, 103]);
    assert!(granules[0].parents(100, EdgeKind::Auth).is_empty());
    assert_eq!(
        granules[0].children(100, EdgeKind::Auth),
        [101, 102, 103, 104, 105, 150]
    );
    assert_eq!(granules[0].children(102, EdgeKind::Prev), [103, 104]);
    assert_eq!(granules[1].children(103, EdgeKind::Auth), [105]);
    assert_eq!(granules[1].children(105, EdgeKind::Prev), [200]);
    assert!(
        granules[0].children(103, EdgeKind::Prev).is_empty(),
        "event 103 lives in the other granule"
    );
}

#[test]
fn borrowed_children_match_the_collected_children() {
    let granules = decoded(&graph());
    for granule in &granules {
        for sn in 95..210 {
            for kind in [EdgeKind::Auth, EdgeKind::Prev] {
                assert_eq!(
                    granule.children_iter(sn, kind).collect::<Vec<_>>(),
                    granule.children(sn, kind),
                    "event {sn} {kind:?}"
                );
            }
        }
    }
}

#[test]
fn a_graph_that_breaks_a_rule_is_refused() {
    let cases: Vec<(&str, GraphBreak)> = vec![
        ("parent not earlier", Box::new(|g| g.rows[2].prev = vec![102])),
        ("later parent", Box::new(|g| g.rows[2].auth = vec![104])),
        ("repeated parent", Box::new(|g| g.rows[3].prev = vec![102, 102])),
        ("missing row", Box::new(|g| drop(g.rows.pop()))),
        ("empty subject", Box::new(|g| g.subject.clear())),
        ("long subject", Box::new(|g| g.subject = vec![b'r'; 256])),
        ("overflow", Box::new(|g| g.first_sn = u64::MAX - 2)),
        (
            "reversed depth",
            Box::new(|g| g.depth = Some(DepthRange { max: 4, min: 5 })),
        ),
        ("inbound child inside", Box::new(|g| g.inbound[0].child = 105)),
        ("inbound parent outside", Box::new(|g| g.inbound[0].parent = 99)),
        (
            "repeated inbound",
            Box::new(|g| {
                let edge = g.inbound[0];
                g.inbound.push(edge);
            }),
        ),
        ("unresolved child outside", Box::new(|g| g.unresolved[0].child = 106)),
        (
            "unresolved link",
            Box::new(|g| g.unresolved[0].parent.kind = RelationshipKind::Link),
        ),
        (
            "unresolved by sequence number",
            Box::new(|g| g.unresolved[0].parent = RelationshipRef::to_subject_sequence(RelationshipKind::Prev, 1)),
        ),
    ];
    let granules = [granule(0, 0, 3), granule(1, 3, 3)];
    for (case, break_rule) in cases {
        let mut graph = graph();
        break_rule(&mut graph);
        assert!(
            matches!(
                encode_graph_blocks(&graph, 6, &granules),
                Err(FormatError::Structural { .. })
            ),
            "{case}"
        );
    }
}

/// [`encode_graph_blocks`] as first written, with a fresh duplicate-check set per parent list and a child vector per
/// reverse slot: the reference the current builder must match byte for byte and refusal for refusal.
fn encode_graph_blocks_as_first_written(
    graph: &GraphInput,
    row_count: u64,
    granules: &[GranuleEntry],
) -> Result<BTreeMap<u32, Vec<u8>>, FormatError> {
    let refuse = |rule: &'static str| FormatError::Structural { rule };
    if graph.subject.is_empty() || graph.subject.len() > MAX_GRAPH_SUBJECT_BYTES {
        return Err(refuse("a graph subject is 1 to 255 bytes"));
    }
    if graph.rows.len() as u64 != row_count {
        return Err(refuse("a graph carries one edge entry per row"));
    }
    let end_sn = graph
        .first_sn
        .checked_add(row_count)
        .ok_or(refuse("graph sequence numbers overflow"))?;
    let mut reverse: Vec<[Vec<u64>; GRAPH_EDGE_KINDS]> = vec![Default::default(); graph.rows.len()];
    for (sn, row) in (graph.first_sn..).zip(&graph.rows) {
        for (kind, targets) in [(EdgeKind::Prev, &row.prev), (EdgeKind::Auth, &row.auth)] {
            let mut seen = HashSet::with_capacity(targets.len());
            for &target in targets {
                if target >= sn {
                    return Err(refuse("a graph parent must be strictly earlier than its event"));
                }
                if !seen.insert(target) {
                    return Err(refuse("a graph event names the same parent twice"));
                }
                if let Some(children) = target
                    .checked_sub(graph.first_sn)
                    .and_then(|row| reverse.get_mut(row as usize))
                    .and_then(|slots| slots.get_mut(kind.slot()))
                {
                    children.push(sn);
                }
            }
        }
    }
    for edge in &graph.unresolved {
        if !(graph.first_sn..end_sn).contains(&edge.child) {
            return Err(refuse("an unresolved graph edge's child lives in the file"));
        }
        if EdgeKind::of(edge.parent.kind).is_none() || edge.parent.space == TargetIdSpace::SubjectSequence {
            return Err(refuse(
                "an unresolved graph edge names a prev or auth parent by protocol id",
            ));
        }
    }
    if graph.depth.is_some_and(|depth| depth.min > depth.max) {
        return Err(refuse("a graph depth range's minimum does not pass its maximum"));
    }
    if let Some(cover) = &graph.chain_cover {
        if cover.labels.len() != graph.rows.len() {
            return Err(refuse("a graph chain cover carries one label per row"));
        }
        let mut last_seq: BTreeMap<u32, u32> = BTreeMap::new();
        for label in &cover.labels {
            if last_seq
                .insert(label.chain_id, label.seq)
                .is_some_and(|last| last >= label.seq)
            {
                return Err(refuse("a chain's seq must strictly increase in row order"));
            }
        }
    }
    let mut inbound: Vec<[Vec<u64>; GRAPH_EDGE_KINDS]> = vec![Default::default(); graph.rows.len()];
    for edge in &graph.inbound {
        if edge.child < end_sn {
            return Err(refuse("an inbound graph edge's child lives past the file's last event"));
        }
        let children = edge
            .parent
            .checked_sub(graph.first_sn)
            .and_then(|row| inbound.get_mut(row as usize))
            .and_then(|slots| slots.get_mut(edge.kind.slot()))
            .ok_or(refuse("an inbound graph edge's parent lives in the file"))?;
        children.push(edge.child);
    }
    for children in inbound.iter_mut().flatten() {
        children.sort_unstable();
        if children.windows(2).any(|pair| pair.first() == pair.last()) {
            return Err(refuse("a graph inbound edge is repeated"));
        }
    }

    let mut blocks = BTreeMap::new();
    for granule in granules {
        let start = granule.first_row_ordinal as usize;
        let range = start..start.saturating_add(granule.row_count as usize);
        let rows = graph
            .rows
            .get(range.clone())
            .ok_or(refuse("granule rows beyond the graph"))?;
        let first_sn = graph.first_sn + granule.first_row_ordinal;
        let forward: Vec<[&[u64]; GRAPH_EDGE_KINDS]> = rows.iter().map(|row| [&row.prev[..], &row.auth[..]]).collect();
        let mut out = Writer::new();
        encode_csr(&mut out, first_sn, &forward)?;
        for lists in [&reverse, &inbound] {
            let lists: Vec<[&[u64]; GRAPH_EDGE_KINDS]> = lists
                .get(range.clone())
                .unwrap_or_default()
                .iter()
                .map(|[prev, auth]| [&prev[..], &auth[..]])
                .collect();
            encode_csr(&mut out, first_sn, &lists)?;
        }
        if let Some(cover) = &graph.chain_cover {
            let labels = cover.labels.get(range).unwrap_or_default();
            put_block(
                &mut out,
                &ColumnData::U64(labels.iter().map(|label| u64::from(label.chain_id)).collect()),
            );
            put_block(
                &mut out,
                &ColumnData::U64(labels.iter().map(|label| u64::from(label.seq)).collect()),
            );
        }
        blocks.insert(granule.granule_id, out.into_bytes());
    }
    Ok(blocks)
}

/// A seeded pseudo-random source: each call returns a value below `bound`.
fn seeded(seed: u64) -> impl FnMut(u64) -> u64 {
    let mut state = seed;
    move |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    }
}

/// A pseudo-random graph, its row count, and granules over it: unsorted parent lists from empty to high fan-out,
/// parents before the file, sequence numbers near zero, `2^40` and `u64::MAX`, inbound edges, chain labels, and
/// sometimes an extra granule that repeats or overlaps another. About half the cases also break a rule or two.
fn random_graph(next: &mut impl FnMut(u64) -> u64) -> (GraphInput, u64, Vec<GranuleEntry>) {
    let rows = next(40) + 1;
    let first_sn = [0, 7, 1 << 40, u64::MAX - 64][next(4) as usize];
    let end_sn = first_sn + rows;
    let mut parents = |sn: u64| {
        let count = if next(10) == 0 { next(30) } else { next(4) };
        let mut list = Vec::new();
        for _ in 0..count {
            let back = 1 + if next(4) == 0 { next(1 << 20) } else { next(rows + 4) };
            if let Some(target) = sn.checked_sub(back)
                && !list.contains(&target)
            {
                list.push(target);
            }
        }
        // A parent at zero spans more sequence numbers than a delta stores when the event sits near `u64::MAX`.
        if sn > 0 && next(30) == 0 && !list.contains(&0) {
            list.push(0);
        }
        list
    };
    let mut graph = GraphInput {
        chain_cover: None,
        depth: None,
        first_sn,
        inbound: Vec::new(),
        rows: (first_sn..end_sn)
            .map(|sn| RowEdges {
                auth: parents(sn),
                prev: parents(sn),
            })
            .collect(),
        state_roots: BTreeMap::new(),
        subject: b"!room:example.org".to_vec(),
        unresolved: Vec::new(),
    };
    for _ in 0..next(5) {
        let edge = GraphEdge {
            child: end_sn + next(6),
            kind: if next(2) == 0 { EdgeKind::Prev } else { EdgeKind::Auth },
            parent: first_sn + next(rows),
        };
        if !graph.inbound.contains(&edge) {
            graph.inbound.push(edge);
        }
    }
    if next(3) == 0 {
        graph.chain_cover = Some(ChainCoverInput {
            labels: (0..rows as u32)
                .map(|row| ChainLabel {
                    chain_id: next(3) as u32,
                    seq: row,
                })
                .collect(),
            links: Vec::new(),
        });
    }
    if next(3) == 0 {
        graph.depth = Some(DepthRange { max: 9, min: 2 });
    }
    if next(3) == 0 {
        graph.unresolved.push(UnresolvedEdge {
            child: first_sn + next(rows),
            parent: RelationshipRef::to_external(RelationshipKind::Prev, b"$late:example.org").unwrap(),
        });
    }
    let mut granules = Vec::new();
    let mut start = 0;
    while start < rows {
        let len = 1 + next((rows - start).min(8));
        granules.push(granule(granules.len() as u32, start, len as u32));
        start += len;
    }
    if next(5) == 0 {
        let id = next(granules.len() as u64 + 1) as u32;
        granules.push(granule(id, next(rows + 1), 1 + next(4) as u32));
    }
    for _ in 0..next(3) {
        break_rule(&mut graph, &mut *next);
    }
    (graph, rows, granules)
}

/// Breaks one rule of `graph` at random, sometimes in a way a later check would also catch.
fn break_rule(graph: &mut GraphInput, next: &mut impl FnMut(u64) -> u64) {
    if graph.rows.is_empty() {
        return;
    }
    let rows = graph.rows.len() as u64;
    let row = next(rows) as usize;
    let sn = graph.first_sn + row as u64;
    let list = if next(2) == 0 {
        &mut graph.rows[row].prev
    } else {
        &mut graph.rows[row].auth
    };
    match next(10) {
        0 if !list.is_empty() => {
            let repeated = list[next(list.len() as u64) as usize];
            list.insert(next(list.len() as u64 + 1) as usize, repeated);
        }
        1 => list.insert(next(list.len() as u64 + 1) as usize, sn.saturating_add(next(3))),
        2 => graph.unresolved.push(UnresolvedEdge {
            child: graph.first_sn + rows,
            parent: RelationshipRef::to_external(RelationshipKind::Auth, b"$gone:example.org").unwrap(),
        }),
        3 => graph.depth = Some(DepthRange { max: 1, min: 2 }),
        4 => graph.chain_cover = Some(ChainCoverInput::default()),
        5 => graph.inbound.push(GraphEdge {
            child: graph.first_sn,
            kind: EdgeKind::Prev,
            parent: graph.first_sn,
        }),
        6 => graph.inbound.push(GraphEdge {
            child: u64::MAX,
            kind: EdgeKind::Auth,
            parent: graph.first_sn.wrapping_sub(1),
        }),
        7 => drop(graph.rows.pop()),
        8 => graph.subject.clear(),
        _ => {
            if let Some(&edge) = graph.inbound.first() {
                graph.inbound.push(edge);
            }
        }
    }
}

#[test]
fn graph_blocks_match_the_builder_as_first_written() {
    let mut next = seeded(0x2545_F491_4F6C_DD1D);
    let (mut built, mut refused) = (0, 0);
    for case in 0..3000 {
        let (graph, row_count, granules) = random_graph(&mut next);
        let expected = encode_graph_blocks_as_first_written(&graph, row_count, &granules);
        assert_eq!(
            encode_graph_blocks(&graph, row_count, &granules),
            expected,
            "case {case}"
        );
        if expected.is_ok() {
            built += 1;
        } else {
            refused += 1;
        }
    }
    assert!(built >= 200 && refused >= 200, "built {built}, refused {refused}");

    for graph in [graph(), room_graph(20_000)] {
        let rows = graph.rows.len() as u64;
        for granules in [
            room_granules(rows),
            vec![granule(0, 0, 3), granule(1, 3, 3)],
            vec![granule(4, 2, 1), granule(4, 0, 4), granule(9, 1, 2)],
        ] {
            assert_eq!(
                encode_graph_blocks(&graph, rows, &granules),
                encode_graph_blocks_as_first_written(&graph, rows, &granules)
            );
        }
    }
}

/// The flat reverse lists give the same blocks as a child vector per slot on graphs shaped to stress them: no edges,
/// every event naming every earlier one (and parents before the file), and one event every other names, each cut into
/// one-row granules, seven-row granules and one granule for the whole file.
#[test]
fn flat_reverse_lists_match_a_vector_per_slot() {
    let rows = 60u64;
    let shaped = |first_sn: u64, parents: &dyn Fn(u64) -> RowEdges| GraphInput {
        chain_cover: None,
        depth: None,
        first_sn,
        inbound: Vec::new(),
        rows: (first_sn..first_sn + rows).map(parents).collect(),
        state_roots: BTreeMap::new(),
        subject: b"room".to_vec(),
        unresolved: Vec::new(),
    };
    let unlinked = shaped(1 << 40, &|_| RowEdges::default());
    let dense = shaped(5, &|sn| RowEdges {
        auth: (0..sn).collect(),
        prev: (5..sn).rev().collect(),
    });
    let star = shaped(0, &|sn| RowEdges {
        auth: if sn > 0 { vec![0] } else { Vec::new() },
        prev: sn.checked_sub(1).into_iter().collect(),
    });
    let cuts = [
        (0..rows).map(|row| granule(row as u32, row, 1)).collect::<Vec<_>>(),
        (0..rows)
            .step_by(7)
            .map(|row| granule(row as u32, row, (rows - row).min(7) as u32))
            .collect(),
        vec![granule(0, 0, rows as u32)],
    ];
    for graph in [&unlinked, &dense, &star] {
        for granules in &cuts {
            assert_eq!(
                encode_graph_blocks(graph, rows, granules),
                encode_graph_blocks_as_first_written(graph, rows, granules)
            );
        }
    }
    let blocks = encode_graph_blocks(&star, rows, &cuts[2]).unwrap();
    let edges = decode_granule_edges(&blocks[&0], 0, rows as u32, rows, false).unwrap();
    assert_eq!(edges.children(0, EdgeKind::Auth), (1..rows).collect::<Vec<_>>());
    assert_eq!(edges.children(0, EdgeKind::Prev), [1]);
}

/// A repeated parent at the start, middle or end of a list, or competing with a parent that is not earlier in the
/// same list, is refused with the same rule as before, whichever comes first.
#[test]
fn a_repeated_or_later_parent_is_refused_as_first_written() {
    let granules = [granule(0, 0, 3), granule(1, 3, 3)];
    let lists: [&[u64]; 8] = [
        &[101, 101, 100],
        &[100, 99, 101, 99],
        &[99, 98, 97, 99],
        &[90, 90],
        &[105, 101, 101],
        &[101, 101, 105],
        &[103, 104],
        &[103, 102, 101, 100, 99, 90],
    ];
    for list in lists {
        for auth in [false, true] {
            let mut graph = graph();
            if auth {
                graph.rows[4].auth = list.to_vec();
            } else {
                graph.rows[4].prev = list.to_vec();
            }
            assert_eq!(
                encode_graph_blocks(&graph, 6, &granules),
                encode_graph_blocks_as_first_written(&graph, 6, &granules),
                "{list:?} auth {auth}"
            );
        }
    }
}

/// A block of three CSR tables over `rows` rows, each from raw offsets and deltas, so a test can write what no
/// writer would.
fn raw_block(tables: [(&[u64], &[i64]); 3]) -> Vec<u8> {
    let mut out = Writer::new();
    for (offsets, deltas) in tables {
        out.put_u32(deltas.len() as u32);
        put_block(&mut out, &ColumnData::U64(offsets.to_vec()));
        if !deltas.is_empty() {
            put_block(&mut out, &ColumnData::I64(deltas.to_vec()));
        }
    }
    out.into_bytes()
}

#[test]
fn corrupt_offsets_are_refused() {
    let empty: (&[u64], &[i64]) = (&[0, 0, 0], &[]);
    let cases: [&[u64]; 4] = [&[1, 1, 1], &[0, 1, 0], &[0, 0], &[0, 0, 2]];
    for offsets in cases {
        let block = raw_block([(offsets, &[1]), empty, empty]);
        assert!(
            matches!(
                decode_granule_edges(&block, 10, 1, 11, false),
                Err(FormatError::Structural { .. })
            ),
            "{offsets:?}"
        );
    }
    let mut block = raw_block([(&[0, 1, 1], &[1]), empty, empty]);
    block.push(0);
    assert!(
        decode_granule_edges(&block, 10, 1, 11, false).is_err(),
        "trailing bytes"
    );
}

#[test]
fn out_of_range_targets_are_refused() {
    let empty: (&[u64], &[i64]) = (&[0, 0, 0], &[]);
    let one: &[u64] = &[0, 1, 1];
    let cases: [[(&[u64], &[i64]); 3]; 6] = [
        // A parent that is the event itself, a later event, or before sequence number zero.
        [(one, &[0]), empty, empty],
        [(one, &[-1]), empty, empty],
        [(one, &[11]), empty, empty],
        // An in-file child that is not later, or lies past the file.
        [empty, (one, &[1]), empty],
        [empty, (one, &[-1]), empty],
        // An inbound child inside the file.
        [empty, empty, (one, &[0])],
    ];
    for tables in cases {
        let block = raw_block(tables);
        assert!(
            matches!(
                decode_granule_edges(&block, 10, 1, 11, false),
                Err(FormatError::RefOutOfRange { .. })
            ),
            "{tables:?}"
        );
    }
    let block = raw_block([(one, &[10]), empty, (one, &[-1])]);
    let edges = decode_granule_edges(&block, 10, 1, 11, false).unwrap();
    assert_eq!(edges.parents(10, EdgeKind::Prev), [0]);
    assert_eq!(edges.children(10, EdgeKind::Prev), [11]);
}

/// The targets one table resolves to when they are collected into their own vector, as the decoder once did: no
/// source is counted for a table with no edges, every slot's source is counted for one with edges (an empty slot's
/// too), and each delta is resolved and checked in slot order. `None` where that decode refuses the table.
fn targets_in_a_separate_vector(
    offsets: &[u64],
    deltas: &[i64],
    first_sn: u64,
    allowed: &dyn Fn(u64, u64) -> bool,
) -> Option<Vec<u64>> {
    let mut targets = Vec::with_capacity(deltas.len());
    if deltas.is_empty() {
        return Some(targets);
    }
    for (slot, pair) in offsets.windows(2).enumerate() {
        let source = first_sn.checked_add((slot / GRAPH_EDGE_KINDS) as u64)?;
        for &delta in &deltas[pair[0] as usize..pair[1] as usize] {
            let target = u64::try_from(i128::from(source) - i128::from(delta))
                .ok()
                .filter(|&target| allowed(source, target))?;
            targets.push(target);
        }
    }
    Some(targets)
}

/// Rebuilding targets inside the decoded delta buffer gives exactly the tables, and the refusals, that collecting them
/// into a second vector gives, over pseudo-random blocks: empty and fanned-out slots, sources near zero and near
/// `u64::MAX` (where an empty slot's source can overflow), extreme deltas, and targets at 0 and `u64::MAX`.
#[test]
fn targets_rebuilt_in_place_match_a_separate_target_vector() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |bound: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % bound
    };
    let (mut accepted, mut refused) = (0, 0);
    for case in 0..4000 {
        // A clean case writes only deltas its table can accept, so most clean blocks decode; a noisy one mixes in
        // extremes and slots with no acceptable target, so most noisy blocks are refused.
        let noisy = next(2) == 0;
        let rows = next(6) as u32 + 1;
        let first_sn = [0, 10, 1 << 40, u64::MAX - 3][next(4) as usize];
        let file_end_sn = first_sn.saturating_add(u64::from(rows));
        let rules: [&dyn Fn(u64, u64) -> bool; 3] = [
            &|source: u64, target: u64| target < source,
            &|source: u64, target: u64| source < target && target < file_end_sn,
            &|_: u64, target: u64| target >= file_end_sn,
        ];
        let mut tables: Vec<(Vec<u64>, Vec<i64>)> = Vec::new();
        for table in 0..3 {
            let mut offsets = vec![0u64];
            let mut deltas = Vec::new();
            for slot in 0..rows as usize * GRAPH_EDGE_KINDS {
                let source = first_sn.wrapping_add((slot / GRAPH_EDGE_KINDS) as u64);
                let to = |target: u64| i64::try_from(i128::from(source) - i128::from(target)).unwrap_or(i64::MIN);
                let reachable = match table {
                    0 => source > 0,
                    1 => source.saturating_add(1) < file_end_sn,
                    _ => true,
                };
                let fan_out = if !noisy && !reachable {
                    0
                } else if next(8) == 0 {
                    6
                } else {
                    next(4).saturating_sub(1)
                };
                for _ in 0..fan_out {
                    let delta = match (noisy.then(|| next(10)), table) {
                        (Some(0), _) => i64::MIN,
                        (Some(1), _) => i64::MAX,
                        (Some(2), _) => to(0),
                        (Some(3), _) => to(u64::MAX),
                        (_, 0) => to(source.saturating_sub(1 + next(8))),
                        (_, 1) => to(source.saturating_add(1 + next(8)).min(file_end_sn.saturating_sub(1))),
                        _ => to(file_end_sn.saturating_add(next(8))),
                    };
                    deltas.push(delta);
                }
                offsets.push(deltas.len() as u64);
            }
            tables.push((offsets, deltas));
        }
        let block = raw_block([
            (tables[0].0.as_slice(), tables[0].1.as_slice()),
            (tables[1].0.as_slice(), tables[1].1.as_slice()),
            (tables[2].0.as_slice(), tables[2].1.as_slice()),
        ]);
        let expected: Option<Vec<Vec<u64>>> = tables
            .iter()
            .zip(rules)
            .map(|((offsets, deltas), allowed)| targets_in_a_separate_vector(offsets, deltas, first_sn, allowed))
            .collect();
        let decoded = decode_granule_edges(&block, first_sn, rows, file_end_sn, false);
        match expected {
            Some(targets) => {
                let edges = decoded.unwrap_or_else(|error| panic!("case {case}: {error:?}"));
                let csrs = [&edges.forward, &edges.reverse, &edges.inbound];
                for ((csr, (offsets, _)), targets) in csrs.into_iter().zip(&tables).zip(targets) {
                    assert_eq!(&csr.offsets, offsets, "case {case}");
                    assert_eq!(csr.targets, targets, "case {case}");
                }
                accepted += 1;
            }
            None => {
                assert!(
                    matches!(decoded, Err(FormatError::RefOutOfRange { .. })),
                    "case {case}: {decoded:?}"
                );
                refused += 1;
            }
        }
    }
    assert!(
        accepted >= 200 && refused >= 200,
        "accepted {accepted}, refused {refused}"
    );
}

fn graph_config(rows_per_granule: usize, graph: Option<GraphInput>) -> HefBuildConfig {
    let mut config = source_form_config(rows_per_granule);
    config.graph = graph;
    config.profile = Profile::Graph;
    config
}

fn chain(first_sn: u64, rows: u64) -> GraphInput {
    GraphInput {
        chain_cover: None,
        depth: None,
        first_sn,
        inbound: Vec::new(),
        rows: (first_sn..first_sn + rows)
            .map(|sn| RowEdges {
                auth: if sn > first_sn { vec![first_sn] } else { Vec::new() },
                prev: sn.checked_sub(1).into_iter().collect(),
            })
            .collect(),
        state_roots: BTreeMap::new(),
        subject: b"room".to_vec(),
        unresolved: Vec::new(),
    }
}

#[test]
fn a_built_graph_file_declares_its_edges_and_reads_them_back() {
    let rows: Vec<_> = (0..20).map(plain_row).collect();
    let built = build_hef_file(rows, &graph_config(4, Some(chain(50, 20)))).unwrap();
    let edge_flags = required_features::GRAPH_DENSE_IDS
        | required_features::GRAPH_CSR_FORWARD
        | required_features::GRAPH_CSR_REVERSE;
    assert_eq!(built.footer.required_feature_flags & edge_flags, edge_flags);
    assert_eq!(built.header.feature_flags & edge_flags, edge_flags);
    let directory = built.footer.graph.as_ref().unwrap();
    assert_eq!(directory.blocks.len(), built.footer.granules.len());

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    assert_eq!(file.footer().graph, built.footer.graph);
    assert_eq!(file.graph_granule_of(49), None);
    assert_eq!(file.graph_granule_of(70), None);
    for sn in 50..70 {
        let granule = file.graph_granule_of(sn).unwrap();
        let edges = file.granule_edges(granule).unwrap();
        let prev: Vec<u64> = sn.checked_sub(1).into_iter().collect();
        assert_eq!(edges.parents(sn, EdgeKind::Prev), prev);
        let next: Vec<u64> = (sn + 1..70).take(1).collect();
        assert_eq!(edges.children(sn, EdgeKind::Prev), next);
    }
    let first = file.granule_edges(file.graph_granule_of(50).unwrap()).unwrap();
    assert_eq!(first.children(50, EdgeKind::Auth), (51..70).collect::<Vec<_>>());
}

#[test]
fn a_built_graph_file_records_the_state_root_after_each_state_event() {
    let rows: Vec<_> = (0..20).map(plain_row).collect();
    let root = |byte: u8| StateRoot {
        address: [byte; 32],
        entry_count: u32::from(byte),
    };
    let mut graph = chain(50, 20);
    graph.state_roots = BTreeMap::from([(50, root(1)), (57, root(2)), (69, root(3))]);
    let built = build_hef_file(rows.clone(), &graph_config(4, Some(graph.clone()))).unwrap();
    assert_ne!(
        built.footer.required_feature_flags & required_features::GRAPH_STATE_ROOTS,
        0
    );
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert_eq!(file.state_root(50), Some(root(1)));
    assert_eq!(file.state_root(57), Some(root(2)));
    assert_eq!(file.state_root(69), Some(root(3)));
    for not_a_state_event in [49, 58, 70] {
        assert_eq!(file.state_root(not_a_state_event), None);
    }

    let without = build_hef_file(rows.clone(), &graph_config(4, Some(chain(50, 20)))).unwrap();
    assert_eq!(
        without.footer.required_feature_flags & required_features::GRAPH_STATE_ROOTS,
        0
    );
    assert!(without.footer.state_roots.is_empty());

    for outside in [49, 70] {
        graph.state_roots = BTreeMap::from([(outside, root(1))]);
        assert!(
            matches!(
                build_hef_file(rows.clone(), &graph_config(4, Some(graph.clone()))),
                Err(FormatError::Structural { .. })
            ),
            "{outside}"
        );
    }
}

/// A deterministic stand-in for a chat room's history: mostly a linear prev chain with an occasional fork, each event
/// authorized by the create event, the latest power-levels event, and its sender's latest membership.
///
/// It carries a stand-in chain cover too: the create event alone on chain 0 and each of the 32 senders' events on a
/// chain of their own, with a link from an event to each auth parent on another chain unless an earlier event on its
/// chain already links to that parent or a later event of that chain.
pub(crate) fn room_graph(events: u64) -> GraphInput {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut power_levels = 0;
    let mut membership = [0u64; 32];
    let mut labels: Vec<ChainLabel> = Vec::new();
    let mut links = Vec::new();
    let mut chain_lengths = [0u32; 33];
    let mut reached: HashMap<(u32, u32), u32> = HashMap::new();
    let rows = (0..events)
        .map(|sn| {
            if sn == 0 {
                labels.push(ChainLabel { chain_id: 0, seq: 0 });
                return RowEdges::default();
            }
            let sender = (random() % 32) as usize;
            let mut prev = vec![sn - 1];
            if sn > 2 && random() % 10 == 0 {
                prev.push(sn - 2);
            }
            let mut auth = vec![0];
            if power_levels != 0 {
                auth.push(power_levels);
            }
            if membership[sender] != 0 && !auth.contains(&membership[sender]) {
                auth.push(membership[sender]);
            }
            match random() % 50 {
                0 => power_levels = sn,
                1..=3 => membership[sender] = sn,
                _ => {}
            }
            let label = ChainLabel {
                chain_id: sender as u32 + 1,
                seq: chain_lengths[sender + 1],
            };
            chain_lengths[sender + 1] += 1;
            for parent in &auth {
                let target = labels[*parent as usize];
                let linked = reached.get(&(label.chain_id, target.chain_id));
                if target.chain_id != label.chain_id && linked.is_none_or(|seq| *seq < target.seq) {
                    reached.insert((label.chain_id, target.chain_id), target.seq);
                    links.push(ChainLink {
                        from_chain: label.chain_id,
                        from_seq: label.seq,
                        to_chain: target.chain_id,
                        to_seq: target.seq,
                    });
                }
            }
            labels.push(label);
            RowEdges { auth, prev }
        })
        .collect();
    GraphInput {
        chain_cover: Some(ChainCoverInput { labels, links }),
        depth: None,
        first_sn: 0,
        inbound: Vec::new(),
        rows,
        state_roots: BTreeMap::new(),
        subject: b"!bench:example.org".to_vec(),
        unresolved: Vec::new(),
    }
}

/// `events` rows cut into 8192-row granules.
fn room_granules(events: u64) -> Vec<GranuleEntry> {
    (0..events.div_ceil(8192))
        .map(|index| {
            let first = index * 8192;
            granule(index as u32, first, (events - first).min(8192) as u32)
        })
        .collect()
}

fn edge_count(graph: &GraphInput) -> u64 {
    graph
        .rows
        .iter()
        .map(|row| (row.auth.len() + row.prev.len()) as u64)
        .sum()
}

/// Stored bytes per edge for a room-shaped graph in 8192-row granules, counting the forward and reverse copies both
/// against the one edge they describe.
#[test]
fn a_room_graph_stores_few_bytes_per_edge() {
    let events = 100_000u64;
    let graph = GraphInput {
        chain_cover: None,
        ..room_graph(events)
    };
    let granules = room_granules(events);
    let blocks = encode_graph_blocks(&graph, events, &granules).unwrap();
    let measurement = GraphEdgeBytesMeasurement {
        edges: edge_count(&graph),
        stored_bytes: blocks.values().map(|block| block.len() as u64).sum(),
    };
    println!(
        "graph edges: {} edges in {} bytes, {:.2} bytes per edge",
        measurement.edges,
        measurement.stored_bytes,
        measurement.stored_bytes as f64 / measurement.edges as f64
    );
    assert!(graph_edge_bytes_gate_passes(&measurement));
}

/// Bytes per chain-cover label and link for the same room-shaped graph: the labels as what they add to the edge
/// blocks, the links as the footer section that holds them and each chain's range.
#[test]
fn a_room_graph_stores_few_bytes_per_chain_label_and_link() {
    let events = 100_000u64;
    let graph = room_graph(events);
    let granules = room_granules(events);
    let stored = |graph: &GraphInput| -> u64 {
        let blocks = encode_graph_blocks(graph, events, &granules).unwrap();
        blocks.values().map(|block| block.len() as u64).sum()
    };
    let bare = GraphInput {
        chain_cover: None,
        ..graph.clone()
    };
    let cover = ChainCover::build(graph.chain_cover.as_ref().unwrap());
    let mut section = Writer::new();
    cover.encode(&mut section);
    let measurement = GraphChainCoverMeasurement {
        label_bytes: stored(&graph) - stored(&bare),
        labels: events,
        link_bytes: section.into_bytes().len() as u64,
        links: cover.links.len() as u64,
    };
    println!(
        "chain cover: {} labels in {} bytes ({:.2} per label), {} links over {} chains in {} bytes ({:.2} per link)",
        measurement.labels,
        measurement.label_bytes,
        measurement.label_bytes as f64 / measurement.labels as f64,
        measurement.links,
        cover.chains.len(),
        measurement.link_bytes,
        measurement.link_bytes as f64 / measurement.links as f64
    );
    assert!(graph_chain_cover_gate_passes(&measurement));
}

fn event_id(sn: u64) -> Vec<u8> {
    format!("$event{sn}:example.org").into_bytes()
}

/// Replays a 100,000-member room's join history: the create and power-levels events, then one join per member. Every
/// event is a state event: its state tree is built in full, as the state chunk API builds it, and the chunks new to
/// it counted as an upload would store them; the events are then written as graph files of 16,384 events in
/// 8192-row granules, each joining event pointing back at the event before it and authorized by the first two, with
/// its protocol id and state root. Prints wall time, events, and bytes written. Run by hand:
/// `cargo test --release --features write --lib replaying_a_100k_member_room -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode room replay benchmark"]
fn replaying_a_100k_member_room() {
    const MEMBERS: u64 = 100_000;
    const FILE_EVENTS: u64 = 16_384;
    let events = MEMBERS + 2;
    let key = StateChunkKey::derive(&TenantDek::new([9; 32]));
    let started = std::time::Instant::now();
    let mut state: BTreeMap<StateKey, u64> = BTreeMap::new();
    let mut previous: Option<StateTree> = None;
    let mut roots = BTreeMap::new();
    let mut state_chunk_bytes = 0u64;
    for sn in 0..events {
        let state_key = match sn {
            0 | 1 => StateKey {
                state_key_id: 0,
                type_id: sn as u32,
            },
            _ => StateKey {
                state_key_id: (sn - 2) as u32,
                type_id: 2,
            },
        };
        state.insert(state_key, sn);
        let tree = build_state_tree(&key, &state).unwrap();
        state_chunk_bytes += match &previous {
            Some(previous) => tree
                .new_chunks_iter(previous)
                .map(|chunk| chunk.bytes.len() as u64)
                .sum::<u64>(),
            None => tree.chunks.iter().map(|chunk| chunk.bytes.len() as u64).sum::<u64>(),
        };
        roots.insert(sn, tree.root);
        previous = Some(tree);
    }
    let mut graph_bytes = 0u64;
    for first_sn in (0..events).step_by(FILE_EVENTS as usize) {
        let end = (first_sn + FILE_EVENTS).min(events);
        let graph = GraphInput {
            chain_cover: None,
            depth: None,
            first_sn,
            inbound: Vec::new(),
            rows: (first_sn..end)
                .map(|sn| RowEdges {
                    auth: (0..sn.min(2)).collect(),
                    prev: sn.checked_sub(1).into_iter().collect(),
                })
                .collect(),
            state_roots: roots.range(first_sn..end).map(|(sn, root)| (*sn, *root)).collect(),
            subject: b"!replay:example.org".to_vec(),
            unresolved: Vec::new(),
        };
        let rows: Vec<_> = (first_sn..end)
            .map(|sn| {
                let mut row = plain_row(sn);
                row.external_id = Some(event_id(sn));
                row
            })
            .collect();
        graph_bytes += build_hef_file(rows, &graph_config(8192, Some(graph)))
            .unwrap()
            .bytes
            .len() as u64;
    }
    let measurement = RoomReplayMeasurement {
        events,
        graph_bytes,
        state_chunk_bytes,
        wall_millis: started.elapsed().as_secs_f64() * 1e3,
    };
    println!(
        "room replay: {} events in {:.0} ms; {} B of graph files, {} B of new state chunks ({:.0} B per event)",
        measurement.events,
        measurement.wall_millis,
        measurement.graph_bytes,
        measurement.state_chunk_bytes,
        measurement.state_chunk_bytes as f64 / measurement.events as f64
    );
    assert!(room_replay_gate_passes(&measurement));
}

#[test]
fn an_event_id_finds_its_sequence_number_and_a_missing_one_does_not() {
    let rows: Vec<_> = (0..20)
        .map(|i| {
            let mut row = plain_row(i);
            // Row 7 carries no protocol id, so the index leaves it out.
            row.external_id = (i != 7).then(|| event_id(50 + i));
            row
        })
        .collect();
    let built = build_hef_file(rows, &graph_config(4, Some(chain(50, 20)))).unwrap();
    assert_ne!(
        built.footer.required_feature_flags & required_features::GRAPH_EVENT_ID_INDEX,
        0
    );
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let index = file.footer().graph.as_ref().unwrap().event_ids.as_ref().unwrap();
    assert_eq!(index.sns.len(), 19);
    for sn in (50..70).filter(|&sn| sn != 57) {
        assert_eq!(file.graph_sn_of(&event_id(sn)).unwrap(), Some(sn));
        assert!(event_id_filter_may_hold(&index.filter, &event_id(sn)));
    }
    assert_eq!(file.graph_sn_of(&event_id(57)).unwrap(), None);
    assert_eq!(file.graph_sn_of(b"$elsewhere:example.org").unwrap(), None);

    let unindexed = build_hef_file(
        (0..20).map(plain_row).collect::<Vec<_>>(),
        &graph_config(4, Some(chain(50, 20))),
    )
    .unwrap();
    assert_eq!(
        unindexed.footer.required_feature_flags & required_features::GRAPH_EVENT_ID_INDEX,
        0,
        "rows without protocol ids get no index"
    );
}

#[test]
fn a_forced_hash_collision_resolves_to_the_event_holding_the_id() {
    // Events 5 and 9 are stored under the same hash; only event 9's row holds the id asked for.
    let hash = event_id_hash(b"$wanted");
    let index = EventIdIndex {
        filter: Vec::new(),
        hashes: vec![hash, hash],
        sns: vec![5, 9],
    };
    let stored: HashMap<u64, &[u8]> = HashMap::from([(5, &b"$collides"[..]), (9, &b"$wanted"[..])]);
    let stored_id = |sn: u64| Ok(stored.get(&sn).map(|id| id.to_vec()));
    assert_eq!(index.sn_of(b"$wanted", stored_id).unwrap(), Some(9));

    let only_collider = EventIdIndex {
        filter: Vec::new(),
        hashes: vec![hash],
        sns: vec![5],
    };
    assert_eq!(
        only_collider.sn_of(b"$wanted", stored_id).unwrap(),
        None,
        "a hash match whose stored id differs never answers"
    );
}

/// The event-id section written straight from the index's arrays is byte for byte the section written by copying them
/// into owned columns first, as the encoder once did; it reads back to the same index, and encoding it twice leaves
/// the index as it was. Hashes several events share are written the same way too.
#[test]
fn the_event_id_section_matches_the_one_written_from_copied_columns() {
    let copied_section = |index: &EventIdIndex| {
        let mut out = Writer::new();
        put_block(&mut out, &ColumnData::U64(index.hashes.clone()));
        put_block(&mut out, &ColumnData::U64(index.sns.clone()));
        out.put_u32(index.filter.len() as u32);
        out.put_slice(&index.filter);
        out.into_bytes()
    };
    let shared_hash = event_id_hash(b"$shared");
    let mut indexes = vec![EventIdIndex {
        filter: Vec::new(),
        hashes: vec![3, shared_hash, shared_hash, shared_hash, u64::MAX],
        sns: vec![9, 2, 5, 7, 1],
    }];
    for events in [1u64, 2, 1023, 1024, 1025, 5000] {
        let ids: Vec<Vec<u8>> = (0..events).map(event_id).collect();
        let index = EventIdIndex::build(ids.iter().map(Vec::as_slice).zip(0..events))
            .unwrap()
            .unwrap();
        assert_eq!(EventIdIndex::decode(&copied_section(&index), 0, events).unwrap(), index);
        indexes.push(index);
    }
    for index in indexes {
        let before = index.clone();
        let expected = copied_section(&index);
        for _ in 0..2 {
            let mut out = Writer::new();
            index.encode(&mut out);
            assert_eq!(out.into_bytes(), expected, "{} events", index.sns.len());
        }
        assert_eq!(index, before);
    }
}

/// The event-id index's stored bytes per event and its filter's false-positive rate, for 100,000 Matrix-shaped ids.
#[test]
fn the_event_id_index_is_small_and_its_filter_rarely_admits_an_absent_id() {
    let events = 100_000u64;
    let ids: Vec<Vec<u8>> = (0..events)
        .map(|sn| {
            format!(
                "${:043x}",
                u128::from(sn).wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835)
            )
            .into_bytes()
        })
        .collect();
    let index = EventIdIndex::build(ids.iter().map(Vec::as_slice).zip(0..))
        .unwrap()
        .unwrap();
    let mut section = Writer::new();
    index.encode(&mut section);
    let section_bytes = section.into_bytes().len() as f64;
    let filter_bytes = index.filter.len() as f64;
    let probes = 200_000u64;
    let admitted = (0..probes)
        .filter(|probe| event_id_filter_may_hold(&index.filter, format!("$absent{probe}").as_bytes()))
        .count();
    let false_positive_rate = admitted as f64 / probes as f64;
    println!(
        "event id index: {:.2} bytes per event in the footer section ({:.2} of them filter), false-positive rate {:.4}",
        section_bytes / events as f64,
        filter_bytes / events as f64,
        false_positive_rate
    );
    assert!(ids.iter().all(|id| event_id_filter_may_hold(&index.filter, id)));
    assert!(graph_event_id_index_gate_passes(&GraphEventIdIndexMeasurement {
        absent_admitted: admitted as u64,
        absent_probes: probes,
        events,
        section_bytes: section_bytes as u64,
    }));
}

/// Labels for `rows` rows starting at row 0: rows alternate between chains 1 and 2, each chain's seq counting up from
/// 10 along its own rows.
fn chain_cover(rows: u32) -> ChainCoverInput {
    ChainCoverInput {
        labels: (0..rows)
            .map(|row| ChainLabel {
                chain_id: 1 + row % 2,
                seq: 10 + row / 2,
            })
            .collect(),
        links: vec![
            ChainLink {
                from_chain: 2,
                from_seq: 12,
                to_chain: 1,
                to_seq: 11,
            },
            ChainLink {
                from_chain: 1,
                from_seq: 13,
                to_chain: 2,
                to_seq: 10,
            },
            ChainLink {
                from_chain: 2,
                from_seq: 11,
                to_chain: 1,
                to_seq: 10,
            },
            ChainLink {
                from_chain: 2,
                from_seq: 12,
                to_chain: 1,
                to_seq: 11,
            },
        ],
    }
}

fn covered_file(cover: ChainCoverInput) -> Result<HefFile, FormatError> {
    let mut graph = chain(50, 20);
    graph.chain_cover = Some(cover);
    let built = build_hef_file(
        (0..20).map(plain_row).collect::<Vec<_>>(),
        &graph_config(4, Some(graph)),
    )?;
    assert_ne!(
        built.footer.required_feature_flags & required_features::GRAPH_CHAIN_COVER,
        0
    );
    HefFile::open(built.bytes, Some(&built.file_seal))
}

#[test]
fn a_non_monotonic_chain_seq_is_refused_at_write() {
    let mut repeated = chain_cover(20);
    repeated.labels[4].seq = repeated.labels[2].seq;
    let mut backwards = chain_cover(20);
    backwards.labels[19].seq = 3;
    let mut short = chain_cover(20);
    short.labels.pop();
    for cover in [repeated, backwards, short] {
        assert!(matches!(covered_file(cover), Err(FormatError::Structural { .. })));
    }
    // Seqs may jump, and chains interleave freely, as long as each chain only climbs.
    let mut jumping = chain_cover(20);
    jumping.labels[19].seq = 1_000;
    assert!(covered_file(jumping).is_ok());
}

/// A labels lookup keeps the edges it decodes, so asking again for events in the same granules decodes nothing.
#[test]
fn a_second_labels_lookup_reuses_the_decoded_edges_of_the_first() {
    let file = covered_file(chain_cover(20)).unwrap();
    let sns: Vec<u64> = (50..70).collect();
    let first = file.graph_chain_labels(&sns).unwrap();
    let held = file.decoded_cache_bytes();
    assert!(held > 0, "the lookup holds its decoded edges");
    assert_eq!(file.graph_chain_labels(&sns).unwrap(), first);
    assert_eq!(
        file.decoded_cache_bytes(),
        held,
        "the second lookup decoded nothing new"
    );
}

#[test]
fn chain_labels_and_links_round_trip_through_a_built_file() {
    let cover = chain_cover(20);
    let file = covered_file(cover.clone()).unwrap();
    let sns: Vec<u64> = (45..75).collect();
    let expected: Vec<Option<ChainLabel>> = sns
        .iter()
        .map(|sn| {
            sn.checked_sub(50)
                .and_then(|row| cover.labels.get(row as usize).copied())
        })
        .collect();
    assert_eq!(file.graph_chain_labels(&sns).unwrap(), expected);

    let from_two = [
        ChainLink {
            from_chain: 2,
            from_seq: 11,
            to_chain: 1,
            to_seq: 10,
        },
        ChainLink {
            from_chain: 2,
            from_seq: 12,
            to_chain: 1,
            to_seq: 11,
        },
    ];
    assert_eq!(file.graph_chain_links(&[2]), from_two, "sorted, the repeated link once");
    assert_eq!(file.graph_chain_links(&[2, 1, 2]).len(), 3);
    assert!(file.graph_chain_links(&[7]).is_empty());

    let built = build_hef_file(
        (0..20).map(plain_row).collect::<Vec<_>>(),
        &graph_config(4, Some(chain(50, 20))),
    )
    .unwrap();
    let uncovered = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert_eq!(uncovered.graph_chain_labels(&[50]).unwrap(), [None]);
    assert!(uncovered.graph_chain_links(&[1, 2]).is_empty());
}

#[test]
fn a_file_is_pruned_by_chain_range() {
    // Chain 1 holds seqs 10 to 19 in this file, chain 2 the same.
    let file = covered_file(chain_cover(20)).unwrap();
    assert!(file.graph_may_hold_chain(1, 0..=10));
    assert!(file.graph_may_hold_chain(2, 15..=15));
    assert!(file.graph_may_hold_chain(1, 19..=u32::MAX));
    assert!(
        !file.graph_may_hold_chain(1, 0..=9),
        "every seq asked for is older than the file's"
    );
    assert!(
        !file.graph_may_hold_chain(2, 20..=30),
        "every seq asked for is newer than the file's"
    );
    assert!(
        !file.graph_may_hold_chain(3, 0..=u32::MAX),
        "the file holds no event on chain 3"
    );

    let built = build_hef_file(
        (0..20).map(plain_row).collect::<Vec<_>>(),
        &graph_config(4, Some(chain(50, 20))),
    )
    .unwrap();
    let uncovered = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert!(!uncovered.graph_may_hold_chain(1, 0..=u32::MAX));
}

#[test]
fn chain_labels_ride_the_edge_block_only_when_declared() {
    let mut graph = graph();
    graph.chain_cover = Some(chain_cover(6));
    let granules = [granule(0, 0, 3), granule(1, 3, 3)];
    let blocks = encode_graph_blocks(&graph, 6, &granules).unwrap();
    let second = decode_granule_edges(&blocks[&1], 103, 3, 106, true).unwrap();
    assert_eq!(second.label(104), Some(ChainLabel { chain_id: 1, seq: 12 }));
    assert_eq!(second.label(100), None, "event 100 lives in the other granule");
    assert!(
        decode_granule_edges(&blocks[&1], 103, 3, 106, false).is_err(),
        "labels read as trailing bytes"
    );
    let plain = encode_graph_blocks(&self::graph(), 6, &granules).unwrap();
    assert!(
        decode_granule_edges(&plain[&1], 103, 3, 106, true).is_err(),
        "a block without labels"
    );
}

/// Times building and decoding the edge blocks of the 100,000-event room in 8192-row granules, the median of five
/// samples each, and prints edges per second, MB/s of edge-block bytes, and stored bytes per edge. Run by hand:
/// `cargo test --release --all-features --lib benchmark_graph_edge_blocks -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode graph edge block benchmark"]
fn benchmark_graph_edge_blocks() {
    let events = 100_000u64;
    let graph = GraphInput {
        chain_cover: None,
        ..room_graph(events)
    };
    let granules = room_granules(events);
    let edges = edge_count(&graph);
    let median = |mut samples: Vec<Duration>| {
        samples.sort_unstable();
        samples[samples.len() / 2]
    };

    let encode = median(
        (0..5)
            .map(|_| {
                let started = Instant::now();
                black_box(encode_graph_blocks(&graph, events, &granules).unwrap());
                started.elapsed()
            })
            .collect(),
    );
    let blocks = encode_graph_blocks(&graph, events, &granules).unwrap();
    let decode = median(
        (0..5)
            .map(|_| {
                let started = Instant::now();
                for granule in &granules {
                    black_box(
                        decode_granule_edges(
                            &blocks[&granule.granule_id],
                            graph.first_sn + granule.first_row_ordinal,
                            granule.row_count,
                            events,
                            false,
                        )
                        .unwrap(),
                    );
                }
                started.elapsed()
            })
            .collect(),
    );

    let stored: u64 = blocks.values().map(|block| block.len() as u64).sum();
    for (step, time) in [("encode", encode), ("decode", decode)] {
        println!(
            "graph edge blocks {step}: {edges} edges in {time:?}, {:.1} M edges/s, {:.1} MB/s",
            edges as f64 / time.as_secs_f64() / 1e6,
            stored as f64 / time.as_secs_f64() / 1e6
        );
    }
    println!(
        "graph edge blocks: {stored} bytes, {:.2} bytes per edge",
        stored as f64 / edges as f64
    );
}
