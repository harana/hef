use super::*;
use crate::benchmarks::{GraphEdgeBytesMeasurement, graph_edge_bytes_gate_passes};
use crate::layout::{Profile, required_features};
use crate::writer::build::{HefBuildConfig, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};
use std::collections::HashMap;

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
fn a_graph_that_breaks_a_rule_is_refused() {
    let cases: Vec<(&str, GraphBreak)> = vec![
        ("parent not earlier", Box::new(|g| g.rows[2].prev = vec![102])),
        ("later parent", Box::new(|g| g.rows[2].auth = vec![104])),
        ("repeated parent", Box::new(|g| g.rows[3].prev = vec![102, 102])),
        ("missing row", Box::new(|g| drop(g.rows.pop()))),
        ("empty subject", Box::new(|g| g.subject.clear())),
        ("long subject", Box::new(|g| g.subject = vec![b'r'; 256])),
        ("overflow", Box::new(|g| g.first_sn = u64::MAX - 2)),
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

fn graph_config(rows_per_granule: usize, graph: Option<GraphInput>) -> HefBuildConfig {
    let mut config = source_form_config(rows_per_granule);
    config.graph = graph;
    config.profile = Profile::Graph;
    config
}

fn chain(first_sn: u64, rows: u64) -> GraphInput {
    GraphInput {
        chain_cover: None,
        first_sn,
        inbound: Vec::new(),
        rows: (first_sn..first_sn + rows)
            .map(|sn| RowEdges {
                auth: if sn > first_sn { vec![first_sn] } else { Vec::new() },
                prev: sn.checked_sub(1).into_iter().collect(),
            })
            .collect(),
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

/// A deterministic stand-in for a chat room's history: mostly a linear prev chain with an occasional fork, each event
/// authorized by the create event, the latest power-levels event, and its sender's latest membership.
fn room_graph(events: u64) -> GraphInput {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut power_levels = 0;
    let mut membership = [0u64; 32];
    let rows = (0..events)
        .map(|sn| {
            if sn == 0 {
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
            RowEdges { auth, prev }
        })
        .collect();
    GraphInput {
        chain_cover: None,
        first_sn: 0,
        inbound: Vec::new(),
        rows,
        subject: b"!bench:example.org".to_vec(),
        unresolved: Vec::new(),
    }
}

/// Stored bytes per edge for a room-shaped graph in 8192-row granules, counting the forward and reverse copies both
/// against the one edge they describe.
#[test]
fn a_room_graph_stores_few_bytes_per_edge() {
    let events = 100_000u64;
    let graph = room_graph(events);
    let granules: Vec<GranuleEntry> = (0..events.div_ceil(8192))
        .map(|index| {
            let first = index * 8192;
            granule(index as u32, first, (events - first).min(8192) as u32)
        })
        .collect();
    let blocks = encode_graph_blocks(&graph, events, &granules).unwrap();
    let measurement = GraphEdgeBytesMeasurement {
        edges: graph
            .rows
            .iter()
            .map(|row| (row.auth.len() + row.prev.len()) as u64)
            .sum(),
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

fn event_id(sn: u64) -> Vec<u8> {
    format!("$event{sn}:example.org").into_bytes()
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
    assert!(section_bytes / (events as f64) < 12.0);
    assert!(false_positive_rate < 0.02);
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
