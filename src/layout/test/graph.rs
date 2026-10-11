use super::*;
use crate::benchmarks::{GraphEdgeBytesMeasurement, graph_edge_bytes_gate_passes};
use crate::layout::{Profile, required_features};
use crate::writer::build::{HefBuildConfig, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};
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
/// event 104 also naming 102, and one parent (90) in an earlier file.
fn graph() -> GraphInput {
    let row = |prev: &[u64], auth: &[u64]| RowEdges {
        auth: auth.to_vec(),
        prev: prev.to_vec(),
    };
    GraphInput {
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
    }
}

fn decoded(graph: &GraphInput) -> Vec<GranuleEdges> {
    let granules = [granule(0, 0, 3), granule(1, 3, 3)];
    let blocks = encode_graph_blocks(graph, 6, &granules).unwrap();
    granules
        .iter()
        .map(|granule| {
            let first_sn = graph.first_sn + granule.first_row_ordinal;
            decode_granule_edges(&blocks[&granule.granule_id], first_sn, granule.row_count, 106).unwrap()
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
                decode_granule_edges(&block, 10, 1, 11),
                Err(FormatError::Structural { .. })
            ),
            "{offsets:?}"
        );
    }
    let mut block = raw_block([(&[0, 1, 1], &[1]), empty, empty]);
    block.push(0);
    assert!(decode_granule_edges(&block, 10, 1, 11).is_err(), "trailing bytes");
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
                decode_granule_edges(&block, 10, 1, 11),
                Err(FormatError::RefOutOfRange { .. })
            ),
            "{tables:?}"
        );
    }
    let block = raw_block([(one, &[10]), empty, (one, &[-1])]);
    let edges = decode_granule_edges(&block, 10, 1, 11).unwrap();
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
        first_sn,
        inbound: Vec::new(),
        rows: (first_sn..first_sn + rows)
            .map(|sn| RowEdges {
                auth: if sn > first_sn { vec![first_sn] } else { Vec::new() },
                prev: sn.checked_sub(1).into_iter().collect(),
            })
            .collect(),
        subject: b"room".to_vec(),
    }
}

#[test]
fn a_built_graph_file_declares_its_edges_and_reads_them_back() {
    let rows: Vec<_> = (0..20).map(plain_row).collect();
    let built = build_hef_file(rows, &graph_config(4, Some(chain(50, 20)))).unwrap();
    let edge_flags = required_features::GRAPH_CSR_FORWARD | required_features::GRAPH_CSR_REVERSE;
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
        first_sn: 0,
        inbound: Vec::new(),
        rows,
        subject: b"!bench:example.org".to_vec(),
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
    let graph = room_graph(events);
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

/// Times building and decoding the edge blocks of the 100,000-event room in 8192-row granules, the median of five
/// samples each, and prints edges per second, MB/s of edge-block bytes, and stored bytes per edge. Run by hand:
/// `cargo test --release --all-features --lib benchmark_graph_edge_blocks -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode graph edge block benchmark"]
fn benchmark_graph_edge_blocks() {
    let events = 100_000u64;
    let graph = room_graph(events);
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
