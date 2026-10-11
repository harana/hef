//! Throws junk and mangled bytes at every decoder to prove none of them ever crash on bad input.
//!
//! It is a deterministic smoke corpus: seeded random buffers plus byte-level mutations of valid artifacts, run on the
//! pinned stable toolchain in CI. The decoders must return `Ok`/`Err` without panicking, reading out of bounds, or
//! allocating unboundedly. The coverage-guided soak lives in `fuzz/` (nightly cargo-fuzz) and runs with the
//! verification change.

#![cfg(test)]

use super::artifacts::batch::{decode_batch, decode_variant_dictionary};
use super::artifacts::frame;
use super::artifacts::state_chunk::{StateKey, build_state_tree, decode_state_chunk};
use super::encoding::{
    CascadeStrategy, ColumnData, Compression, PipelineId, Transform, ValueKind, decode_block,
    encode_block_forced_for_conformance,
};
use super::events::variant::VariantRef;
#[cfg(feature = "write")]
use super::events::{RelationshipKind, RelationshipRef};
use super::file::integrity::crc64_nvme;
#[cfg(feature = "write")]
use super::file::integrity::hash_tree;
#[cfg(feature = "write")]
use super::layout::Profile;
use super::layout::footer::{GranuleEntry, decode_footer};
#[cfg(feature = "write")]
use super::layout::footer::{encode_footer, sections};
use super::layout::graph::{
    ChainCoverInput, ChainLabel, GraphInput, RowEdges, decode_granule_edges, encode_graph_blocks,
};
#[cfg(feature = "write")]
use super::layout::graph::{ChainLink, DepthRange, UnresolvedEdge};
use super::layout::time_series::TimeSeriesRollups;
use super::layout::{decode_header, reader::HefFile};
use super::security::{StateChunkKey, TenantDek};
#[cfg(feature = "write")]
use super::writer::build::build_hef_file;
#[cfg(feature = "write")]
use super::writer::source_form::tests::{plain_row, source_form_config};
use std::collections::BTreeMap;

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn exercise_all(bytes: &[u8]) {
    let _ = frame::precheck_header(bytes);
    let _ = frame::decode_frame(bytes);
    if let Some((count, payload)) = bytes.split_first() {
        let _ = decode_batch(payload, u32::from(*count));
    }
    let (dict_bytes, value_bytes) = bytes.split_at(bytes.len() / 2);
    if let Ok(dictionary) = decode_variant_dictionary(dict_bytes) {
        let _ = VariantRef::new(value_bytes).validate(&dictionary);
    }
    let _ = decode_footer(bytes);
    let _ = decode_header(bytes);
    let _ = HefFile::open(bytes.to_vec(), None);
    let _ = decode_granule_edges(bytes, 1_000, 64, 2_000, false);
    let _ = decode_block(xor_float_pipeline(), bytes);
    let _ = TimeSeriesRollups::decode(bytes);
    if bytes.len() >= 4 {
        let id = u32::from_le_bytes(bytes.get(..4).unwrap_or(&[0; 4]).try_into().unwrap());
        let _ = decode_block(PipelineId(id), bytes.get(4..).unwrap_or_default());
    }
}

#[test]
fn random_buffers_never_panic_any_decoder() {
    let mut rng = XorShift(0xDEAD_BEEF_0BAD_F00D);
    for len in [0usize, 1, 7, 64, 191, 192, 193, 4096, 9000] {
        for _ in 0..16 {
            exercise_all(&rng.bytes(len));
        }
    }
}

#[test]
fn mutated_valid_frame_never_panics() {
    let frame_bytes = super::artifacts::batch::tests::sample_frame(3, 1, 1);
    let mut rng = XorShift(7);
    // Flip one byte at a spread of positions across the frame.
    let mut position = 0usize;
    while position < frame_bytes.len() {
        let mut mutated = frame_bytes.clone();
        mutated[position] ^= (rng.next() as u8) | 1;
        let _ = frame::precheck_header(&mutated);
        let _ = frame::decode_frame(&mutated);
        position += 37;
    }
    // Truncations at every boundary class.
    for cut in [0usize, 1, 191, 192, 256, frame_bytes.len() - 1] {
        let _ = frame::decode_frame(&frame_bytes[..cut]);
    }
}

/// Flips bytes across a valid graph edge block, and its offset, target, and chain label arrays in particular, and
/// truncates it: the decoder refuses or answers, never panics.
#[test]
fn mutated_graph_edge_block_never_panics() {
    let graph = GraphInput {
        chain_cover: Some(ChainCoverInput {
            labels: (0..64u32)
                .map(|row| ChainLabel {
                    chain_id: row % 3,
                    seq: row,
                })
                .collect(),
            links: Vec::new(),
        }),
        depth: None,
        first_sn: 10,
        inbound: Vec::new(),
        rows: (10..74u64)
            .map(|sn| RowEdges {
                auth: vec![10.min(sn - 1)],
                prev: vec![sn - 1],
            })
            .collect(),
        state_roots: BTreeMap::new(),
        subject: b"room".to_vec(),
        unresolved: Vec::new(),
    };
    let granule = GranuleEntry {
        compressed_bytes_estimate: 0,
        first_epoch: 1,
        first_row_ordinal: 0,
        first_sequence: 1,
        granule_id: 0,
        last_epoch: 1,
        last_sequence: 64,
        max_ingested_at_physical: 0,
        max_occurred_at_physical: 0,
        min_ingested_at_physical: 0,
        min_occurred_at_physical: 0,
        row_count: 64,
        stripe_id: 0,
    };
    let block = encode_graph_blocks(&graph, 64, &[granule]).unwrap().remove(&0).unwrap();
    assert!(decode_granule_edges(&block, 10, 64, 74, true).is_ok());
    let mut rng = XorShift(23);
    // A spread of positions rather than every byte: a flipped bit width can make one block decode millions of values.
    for position in (0..block.len()).step_by(29) {
        let mut mutated = block.clone();
        mutated[position] ^= (rng.next() as u8) | 1;
        let _ = decode_granule_edges(&mutated, 10, 64, 74, true);
    }
    for cut in 0..block.len() {
        assert!(decode_granule_edges(&block[..cut], 10, 64, 74, true).is_err());
    }
}

fn xor_float_pipeline() -> PipelineId {
    PipelineId::new(Transform::Chimp, Compression::None, ValueKind::F64)
}

/// Flips bytes across a valid Chimp float block and truncates it: the decoder refuses or answers, never panics, and
/// every truncation is refused.
#[test]
fn mutated_xor_float_block_never_panics() {
    let mut rng = XorShift(31);
    let values: Vec<f64> = (0..300u64)
        .map(|i| match i % 7 {
            0 => f64::from_bits(rng.next()),
            1 => f64::NAN,
            2 => -0.0,
            _ => 40.0 + (i as f64 / 13.0).sin(),
        })
        .collect();
    let block = encode_block_forced_for_conformance(
        &ColumnData::F64(values),
        true,
        CascadeStrategy::DecodeOptimized,
        Transform::Chimp,
    )
    .unwrap();
    assert_eq!(block.pipeline, xor_float_pipeline());
    assert!(decode_block(block.pipeline, &block.bytes).is_ok());
    for position in 0..block.bytes.len() {
        let mut mutated = block.bytes.clone();
        mutated[position] ^= (rng.next() as u8) | 1;
        let _ = decode_block(block.pipeline, &mutated);
    }
    for cut in 0..block.bytes.len() {
        assert!(decode_block(block.pipeline, &block.bytes[..cut]).is_err());
    }
}

/// Flips bytes across real state chunks, leaf and interior, and truncates them, each both with its CRC left as it was
/// and with a CRC that matches the damaged bytes, so the checks behind the CRC see the damage too: the decoder refuses
/// or answers, never panics, and every truncation is refused.
#[test]
fn mutated_state_chunk_never_panics() {
    let key = StateChunkKey::derive(&TenantDek::new([5; 32]));
    let entries: BTreeMap<StateKey, u64> = (0..2_000u32)
        .map(|user| {
            let state_key = StateKey {
                state_key_id: user,
                type_id: 2,
            };
            (state_key, u64::from(user) + 3)
        })
        .collect();
    let tree = build_state_tree(&key, &entries).unwrap();
    assert!(tree.chunks.len() > 2, "leaves and an interior chunk");
    let restamped = |bytes: &[u8]| {
        let body = bytes.get(..bytes.len().saturating_sub(8)).unwrap_or_default();
        let mut out = body.to_vec();
        out.extend_from_slice(&crc64_nvme(body).to_le_bytes());
        out
    };
    let mut rng = XorShift(41);
    for chunk in &tree.chunks {
        let plaintext = key.open(&chunk.address, &chunk.bytes).unwrap();
        assert!(decode_state_chunk(&plaintext).is_ok());
        for position in (0..plaintext.len()).step_by(7) {
            let mut mutated = plaintext.clone();
            mutated[position] ^= (rng.next() as u8) | 1;
            assert!(decode_state_chunk(&mutated).is_err(), "the CRC catches a flipped byte");
            let _ = decode_state_chunk(&restamped(&mutated));
        }
        for cut in 0..plaintext.len() {
            assert!(decode_state_chunk(&plaintext[..cut]).is_err());
            assert!(decode_state_chunk(&restamped(&plaintext[..cut])).is_err());
        }
    }
}

/// The footer of a graph file carrying every graph footer section: edges with one unresolved parent, an event-id
/// index, a chain cover, and a state root per event.
#[cfg(feature = "write")]
fn full_graph_footer() -> Vec<u8> {
    let key = StateChunkKey::derive(&TenantDek::new([6; 32]));
    let root = build_state_tree(&key, &BTreeMap::new()).unwrap().root;
    let mut config = source_form_config(8);
    config.profile = Profile::Graph;
    config.graph = Some(GraphInput {
        chain_cover: Some(ChainCoverInput {
            labels: (0..32u32)
                .map(|row| ChainLabel {
                    chain_id: row % 3,
                    seq: row,
                })
                .collect(),
            links: vec![ChainLink {
                from_chain: 1,
                from_seq: 4,
                to_chain: 0,
                to_seq: 3,
            }],
        }),
        depth: Some(DepthRange { max: 140, min: 108 }),
        first_sn: 100,
        inbound: Vec::new(),
        rows: (100..132u64)
            .map(|sn| RowEdges {
                auth: if sn > 100 { vec![100] } else { Vec::new() },
                prev: if sn > 100 { vec![sn - 1] } else { Vec::new() },
            })
            .collect(),
        state_roots: (100..132u64).map(|sn| (sn, root)).collect(),
        subject: b"!room:example.org".to_vec(),
        unresolved: vec![UnresolvedEdge {
            child: 105,
            parent: RelationshipRef::to_external(RelationshipKind::Prev, b"$late:example.org").unwrap(),
        }],
    });
    let rows: Vec<_> = (0..32u64)
        .map(|i| {
            let mut row = plain_row(i);
            row.external_id = Some(format!("$event{i}:example.org").into_bytes());
            row
        })
        .collect();
    encode_footer(&build_hef_file(rows, &config).unwrap().footer)
}

/// Each section of the footer `blob` as `(id, offset, len)`, the offset counted from the start of the section area,
/// and that area. The preamble is the format version (4 bytes), both feature words (16), the schema fingerprint (32)
/// and the section count (4); each directory entry is the id (4), offset (8), length (8) and checksum (32).
#[cfg(feature = "write")]
fn footer_sections(blob: &[u8]) -> (Vec<(u32, usize, usize)>, &[u8]) {
    let count = u32::from_le_bytes(blob[52..56].try_into().unwrap()) as usize;
    let (directory, area) = blob[56..].split_at(count * 52);
    let entries = directory
        .chunks_exact(52)
        .map(|entry| {
            (
                u32::from_le_bytes(entry[..4].try_into().unwrap()),
                u64::from_le_bytes(entry[4..12].try_into().unwrap()) as usize,
                u64::from_le_bytes(entry[12..20].try_into().unwrap()) as usize,
            )
        })
        .collect();
    (entries, area)
}

/// The footer `blob` with section `id` replaced by `body`, the sections laid out again and every checksum recomputed,
/// so the footer decoder gets past the checksum and reads the replaced bytes.
#[cfg(feature = "write")]
fn with_section(blob: &[u8], id: u32, body: &[u8]) -> Vec<u8> {
    let (entries, area) = footer_sections(blob);
    let mut out = blob[..56].to_vec();
    let mut new_area = Vec::new();
    for (section_id, offset, len) in entries {
        let bytes = if section_id == id {
            body
        } else {
            &area[offset..offset + len]
        };
        out.extend_from_slice(&section_id.to_le_bytes());
        out.extend_from_slice(&(new_area.len() as u64).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(hash_tree(bytes).as_bytes());
        new_area.extend_from_slice(bytes);
    }
    out.extend_from_slice(&new_area);
    out
}

/// Flips bytes across each graph footer section (event-id index, chain cover, unresolved parents, state roots) and
/// truncates it, with the checksum recomputed so the damage reaches the section's own decoder: the footer decoder
/// refuses or answers, never panics, and every truncation is refused.
#[cfg(feature = "write")]
#[test]
fn mutated_graph_footer_sections_never_panic() {
    let blob = full_graph_footer();
    let (entries, area) = footer_sections(&blob);
    let mut rng = XorShift(53);
    for id in [
        sections::GRAPH_EVENT_IDS,
        sections::GRAPH_CHAIN_COVER,
        sections::GRAPH_UNRESOLVED,
        sections::GRAPH_STATE_ROOTS,
        sections::GRAPH_DEPTH,
    ] {
        let (_, offset, len) = entries.iter().copied().find(|entry| entry.0 == id).unwrap();
        let body = &area[offset..offset + len];
        assert!(decode_footer(&with_section(&blob, id, body)).is_ok(), "section {id}");
        for position in (0..body.len()).step_by(3) {
            let mut mutated = body.to_vec();
            mutated[position] ^= (rng.next() as u8) | 1;
            let _ = decode_footer(&with_section(&blob, id, &mutated));
        }
        for cut in 0..body.len() {
            assert!(
                decode_footer(&with_section(&blob, id, &body[..cut])).is_err(),
                "section {id} cut at {cut}"
            );
        }
    }
}
