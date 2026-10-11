//! Stores a subject's event graph beside its rows: for every event, the earlier events it points back to (its `prev`
//! and `auth` parents) and the events that point at it (its children).
//!
//! A graph-profile file holds one subject's events with dense, gap-free sequence numbers in row order, so row `i` is
//! event `first_sn + i`. Each granule carries one edge block in the data area, right after the granule's payload
//! arenas, so one range read returns a granule's rows and their edges together. The block holds three compressed
//! sparse row (CSR) tables, each restarting its offsets at the granule's first row:
//!
//! - forward: each event's parents, as the caller supplied them;
//! - reverse: each event's children inside the file, built at seal from the forward edges;
//! - inbound: each event's children that live in later files, supplied by the compaction that rewrites the file, so
//!   an old file is never rewritten per event.
//!
//! A parent the writer could not give a sequence number (an event not yet received, say) is not an edge: it stays as
//! the reference its event declared, in the footer's list of unresolved parents, and the event reads as having
//! unresolved edges.
//!
//! A table is `edge_count u32`, its offsets block, then (when it has edges) its targets block. Offsets are one entry
//! per `(row, kind)` slot plus a final total, `prev` before `auth` within each row. A target is stored as
//! `source_sn - target_sn`, positive for a parent and negative for a child. Both arrays go through the ordinary column
//! block encoder (zigzag, frame-of-reference or delta bit-packing, and its trailing compression stage), each framed
//! as `pipeline u32 | byte length u32 | bytes`.
//!
//! See: hef-file-layout/spec.md

use super::constant::{GRAPH_EDGE_KINDS, MAX_GRAPH_SUBJECT_BYTES, UNRESOLVED_ENTRY_MIN_BYTES};
use super::footer::GranuleEntry;
use super::reader::HefFile;
use crate::encoding::{ColumnData, PipelineId, decode_block, encode_block};
use crate::error::FormatError;
use crate::events::{RelationshipKind, RelationshipRef, TargetIdSpace};
use crate::file::bytes::{Reader, Writer};
use hashbrown::HashSet;
use std::collections::BTreeMap;

/// Which relationship an edge records between two events of one subject.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// The earlier events that authorize this one.
    Auth,
    /// The events that came directly before this one in the subject's history.
    Prev,
}

impl EdgeKind {
    /// This kind's position within a row's pair of offset slots. Fixed by the format, never by variant order.
    fn slot(self) -> usize {
        match self {
            EdgeKind::Prev => 0,
            EdgeKind::Auth => 1,
        }
    }

    fn from_slot(slot: u8) -> Option<Self> {
        match slot {
            0 => Some(EdgeKind::Prev),
            1 => Some(EdgeKind::Auth),
            _ => None,
        }
    }

    /// The edge kind a reference of relationship `kind` records, or `None` for a relationship a graph does not store.
    pub fn of(kind: RelationshipKind) -> Option<Self> {
        match kind {
            RelationshipKind::Auth => Some(EdgeKind::Auth),
            RelationshipKind::Prev => Some(EdgeKind::Prev),
            _ => None,
        }
    }

    /// The relationship an edge of this kind records.
    pub fn relationship(self) -> RelationshipKind {
        match self {
            EdgeKind::Auth => RelationshipKind::Auth,
            EdgeKind::Prev => RelationshipKind::Prev,
        }
    }
}

/// The edges a build stores beside a graph-profile file's rows.
///
/// The rows must be one subject's events with dense, gap-free sequence numbers in row order: row `i` is event
/// `first_sn + i`. Every parent must be strictly earlier than its event, and may live in an earlier file.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphInput {
    /// The sequence number of the file's first row.
    pub first_sn: u64,
    /// Edges from events in later files to parents in this one, supplied when compaction rewrites the file. Each
    /// names a parent inside the file and a child past its last event.
    pub inbound: Vec<GraphEdge>,
    /// Exactly one entry per row, in row order.
    pub rows: Vec<RowEdges>,
    /// Whose graph this is, for example a room id: 1 to 255 bytes.
    pub subject: Vec<u8>,
    /// Parents no sequence number is known for yet (an event not yet received, say), kept as the references their
    /// events declared. Each names a child inside the file and a `prev` or `auth` reference in any space but
    /// `subject_sequence`.
    pub unresolved: Vec<UnresolvedEdge>,
}

/// One event's parents, as sequence numbers, in the order the caller wants them returned. No list may repeat a
/// parent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowEdges {
    pub auth: Vec<u64>,
    pub prev: Vec<u64>,
}

/// One edge between two events, named by sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GraphEdge {
    pub child: u64,
    pub kind: EdgeKind,
    pub parent: u64,
}

/// A parent an event named that has no sequence number yet: the child's sequence number and the `prev` or `auth`
/// reference exactly as the event declared it.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnresolvedEdge {
    pub child: u64,
    pub parent: RelationshipRef,
}

/// One granule's decoded edges: parents and children for each of its events, as sequence numbers.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GranuleEdges {
    first_sn: u64,
    forward: Csr,
    inbound: Csr,
    reverse: Csr,
}

/// One compressed sparse row table, its targets resolved to sequence numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Csr {
    /// One entry per `(row, kind)` slot plus a final total; non-decreasing from zero.
    offsets: Vec<u64>,
    targets: Vec<u64>,
}

impl GranuleEdges {
    /// The parents of event `sn` along `kind` edges, in the order the writer was given them. Empty for an event this
    /// granule does not hold.
    pub fn parents(&self, sn: u64, kind: EdgeKind) -> &[u64] {
        self.forward.edges(self.first_sn, sn, kind)
    }

    /// The children of event `sn` along `kind` edges: those in the same file in ascending order, then those from
    /// later files that compaction folded in, in ascending order. Empty for an event this granule does not hold.
    pub fn children(&self, sn: u64, kind: EdgeKind) -> Vec<u64> {
        let mut children = self.reverse.edges(self.first_sn, sn, kind).to_vec();
        children.extend_from_slice(self.inbound.edges(self.first_sn, sn, kind));
        children
    }
}

impl Csr {
    fn edges(&self, first_sn: u64, sn: u64, kind: EdgeKind) -> &[u64] {
        let Some(row) = sn.checked_sub(first_sn).and_then(|row| usize::try_from(row).ok()) else {
            return &[];
        };
        let slot = row.saturating_mul(GRAPH_EDGE_KINDS).saturating_add(kind.slot());
        let (Some(&start), Some(&end)) = (self.offsets.get(slot), self.offsets.get(slot.saturating_add(1))) else {
            return &[];
        };
        self.targets.get(start as usize..end as usize).unwrap_or_default()
    }
}

/// Checks `graph` against a file of `row_count` rows cut into `granules`, then builds each granule's edge block,
/// keyed by granule id. Refuses a graph whose row list does not match the file, whose sequence numbers would
/// overflow, or that names a parent not strictly earlier than its event, a repeated edge, an inbound edge that does
/// not run from a later file into this one, or an unresolved parent that is not a `prev` or `auth` reference by
/// protocol id from an event in the file.
pub fn encode_graph_blocks(
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
        encode_csr(
            &mut out,
            first_sn,
            &slot_lists(reverse.get(range.clone()).unwrap_or_default()),
        )?;
        encode_csr(&mut out, first_sn, &slot_lists(inbound.get(range).unwrap_or_default()))?;
        blocks.insert(granule.granule_id, out.into_bytes());
    }
    Ok(blocks)
}

fn slot_lists(lists: &[[Vec<u64>; GRAPH_EDGE_KINDS]]) -> Vec<[&[u64]; GRAPH_EDGE_KINDS]> {
    lists.iter().map(|[prev, auth]| [&prev[..], &auth[..]]).collect()
}

fn encode_csr(out: &mut Writer, first_sn: u64, rows: &[[&[u64]; GRAPH_EDGE_KINDS]]) -> Result<(), FormatError> {
    let mut offsets = Vec::with_capacity(rows.len() * GRAPH_EDGE_KINDS + 1);
    let mut deltas = Vec::new();
    offsets.push(0);
    for (source, lists) in (first_sn..).zip(rows) {
        for targets in lists {
            for &target in *targets {
                let delta =
                    i64::try_from(i128::from(source) - i128::from(target)).map_err(|_| FormatError::Structural {
                        rule: "a graph edge spans more sequence numbers than it can store",
                    })?;
                deltas.push(delta);
            }
            offsets.push(deltas.len() as u64);
        }
    }
    let edge_count = u32::try_from(deltas.len()).map_err(|_| FormatError::Structural {
        rule: "a granule holds more graph edges than it can store",
    })?;
    out.put_u32(edge_count);
    put_block(out, &ColumnData::U64(offsets));
    if edge_count > 0 {
        put_block(out, &ColumnData::I64(deltas));
    }
    Ok(())
}

fn put_block(out: &mut Writer, data: &ColumnData) {
    let encoded = encode_block(data, false);
    out.put_u32(encoded.pipeline.0);
    out.put_u32(encoded.bytes.len() as u32);
    out.put_slice(&encoded.bytes);
}

/// Writes the unresolved-parent list of footer section `GRAPH_UNRESOLVED`: `count u32`, then per entry the child's
/// sequence number `u64`, the edge kind `u8` (0 `prev`, 1 `auth`), and the reference's column value (see
/// [`RelationshipRef::column_value`]) as `length u16 | bytes`. `edges` must already be sorted by child.
pub(super) fn encode_unresolved(out: &mut Writer, edges: &[UnresolvedEdge]) {
    out.put_u32(edges.len() as u32);
    for edge in edges {
        let value = edge.parent.column_value();
        out.put_u64(edge.child);
        out.put_u8(EdgeKind::of(edge.parent.kind).map_or(u8::MAX, |kind| kind.slot() as u8));
        out.put_u16(value.len() as u16);
        out.put_slice(value.as_bytes());
    }
}

/// Reads [`encode_unresolved`]'s bytes for a file whose events run from `first_sn` up to just before `end_sn`.
/// Refuses an unknown kind, a reference that does not parse or is named by sequence number, a child outside the
/// file, and a list not sorted by child.
pub(super) fn decode_unresolved(bytes: &[u8], first_sn: u64, end_sn: u64) -> Result<Vec<UnresolvedEdge>, FormatError> {
    let malformed = FormatError::Structural {
        rule: "graph unresolved parents must be prev or auth references by protocol id, sorted by child",
    };
    let mut reader = Reader::new(bytes);
    let count = reader.u32("graph unresolved count")? as usize;
    let mut edges: Vec<UnresolvedEdge> = Vec::with_capacity(count.min(reader.remaining() / UNRESOLVED_ENTRY_MIN_BYTES));
    for _ in 0..count {
        let child = reader.u64("graph unresolved child")?;
        let kind = EdgeKind::from_slot(reader.u8("graph unresolved kind")?).ok_or(malformed.clone())?;
        let len = usize::from(reader.u16("graph unresolved reference length")?);
        let value = std::str::from_utf8(reader.take(len, "graph unresolved reference")?).map_err(|_| {
            FormatError::InvalidUtf8 {
                what: "graph unresolved reference",
            }
        })?;
        let parent = RelationshipRef::parse_column_value(kind.relationship(), value).map_err(|_| malformed.clone())?;
        if parent.space == TargetIdSpace::SubjectSequence
            || !(first_sn..end_sn).contains(&child)
            || edges.last().is_some_and(|last| last.child > child)
        {
            return Err(malformed);
        }
        edges.push(UnresolvedEdge { child, parent });
    }
    if reader.remaining() != 0 {
        return Err(FormatError::Structural {
            rule: "graph unresolved section has trailing bytes",
        });
    }
    Ok(edges)
}

fn take_block(reader: &mut Reader<'_>) -> Result<ColumnData, FormatError> {
    let pipeline = PipelineId(reader.u32("graph block pipeline id")?);
    let len = reader.u32("graph block byte length")? as usize;
    decode_block(pipeline, reader.take(len, "graph block bytes")?)
}

/// Decodes one granule's edge block, written for `rows` events starting at `first_sn` in a file whose events end
/// just before `file_end_sn`. Refuses bytes whose offsets do not run from zero to the edge count in order, or whose
/// targets break the direction rules: a parent strictly earlier than its event, an in-file child later than its event
/// and inside the file, an inbound child past the file's last event.
pub fn decode_granule_edges(
    bytes: &[u8],
    first_sn: u64,
    rows: u32,
    file_end_sn: u64,
) -> Result<GranuleEdges, FormatError> {
    let mut reader = Reader::new(bytes);
    let forward = decode_csr(&mut reader, first_sn, rows, |source, target| target < source)?;
    let reverse = decode_csr(&mut reader, first_sn, rows, |source, target| {
        source < target && target < file_end_sn
    })?;
    let inbound = decode_csr(&mut reader, first_sn, rows, |_, target| target >= file_end_sn)?;
    if reader.remaining() != 0 {
        return Err(FormatError::Structural {
            rule: "graph edge block has trailing bytes",
        });
    }
    Ok(GranuleEdges {
        first_sn,
        forward,
        inbound,
        reverse,
    })
}

fn decode_csr(
    reader: &mut Reader<'_>,
    first_sn: u64,
    rows: u32,
    allowed: impl Fn(u64, u64) -> bool,
) -> Result<Csr, FormatError> {
    let edge_count = u64::from(reader.u32("graph edge count")?);
    let ColumnData::U64(offsets) = take_block(reader)? else {
        return Err(FormatError::Structural {
            rule: "graph offsets must decode to a u64 column",
        });
    };
    let slots = rows as usize * GRAPH_EDGE_KINDS;
    if offsets.len() != slots + 1
        || offsets.first() != Some(&0)
        || offsets.last() != Some(&edge_count)
        || offsets.windows(2).any(|pair| pair.first() > pair.last())
    {
        return Err(FormatError::Structural {
            rule: "graph offsets must run in order from zero to the edge count, one per slot",
        });
    }
    if edge_count == 0 {
        return Ok(Csr {
            offsets,
            targets: Vec::new(),
        });
    }
    let ColumnData::I64(deltas) = take_block(reader)? else {
        return Err(FormatError::Structural {
            rule: "graph targets must decode to an i64 column",
        });
    };
    if deltas.len() as u64 != edge_count {
        return Err(FormatError::Structural {
            rule: "graph targets must hold exactly the edge count",
        });
    }
    let mut targets = Vec::with_capacity(deltas.len());
    for (slot, pair) in offsets.windows(2).enumerate() {
        let (Some(&start), Some(&end)) = (pair.first(), pair.last()) else {
            continue;
        };
        let source = first_sn
            .checked_add((slot / GRAPH_EDGE_KINDS) as u64)
            .ok_or(FormatError::RefOutOfRange {
                what: "graph event sequence number",
            })?;
        for &delta in deltas.get(start as usize..end as usize).unwrap_or_default() {
            let target = u64::try_from(i128::from(source) - i128::from(delta))
                .ok()
                .filter(|&target| allowed(source, target))
                .ok_or(FormatError::RefOutOfRange {
                    what: "graph edge target",
                })?;
            targets.push(target);
        }
    }
    Ok(Csr { offsets, targets })
}

impl HefFile {
    /// The edges stored for the events of granule `granule_id` of a graph-profile file, read and checked. Fails when
    /// the file carries no graph or no edge block for that granule.
    pub fn granule_edges(&self, granule_id: u32) -> Result<GranuleEdges, FormatError> {
        let (position, len) = self.graph_block_range(granule_id)?;
        let footer = self.footer();
        let first_sn = footer.graph.as_ref().map_or(0, |directory| directory.first_sn);
        let granule = footer
            .granules
            .iter()
            .find(|granule| granule.granule_id == granule_id)
            .ok_or(FormatError::RefOutOfRange { what: "granule id" })?;
        let bytes = self.data(position as usize, len as usize, "graph edge block")?;
        decode_granule_edges(
            bytes,
            first_sn.saturating_add(granule.first_row_ordinal),
            granule.row_count,
            first_sn.saturating_add(footer.exact_counts.row_count),
        )
    }

    /// The sequence number of the event at row `row_ordinal` of a graph-profile file, or `None` when the file carries
    /// no graph or has no such row. Rows are numbered densely: row `i` is event `first_sn + i`.
    pub fn graph_sn_of_row(&self, row_ordinal: u64) -> Option<u64> {
        let footer = self.footer();
        if row_ordinal >= footer.exact_counts.row_count {
            return None;
        }
        footer.graph.as_ref()?.first_sn.checked_add(row_ordinal)
    }

    /// The row of a graph-profile file that holds event `sn`, or `None` when the file does not hold it.
    pub fn graph_row_of(&self, sn: u64) -> Option<u64> {
        let footer = self.footer();
        let row = sn.checked_sub(footer.graph.as_ref()?.first_sn)?;
        (row < footer.exact_counts.row_count).then_some(row)
    }

    /// Whether event `sn` named a parent that had no sequence number when the file was written, so some of its
    /// edges are missing from [`Self::granule_edges`]. `false` for an event the file does not hold.
    pub fn graph_has_unresolved_edges(&self, sn: u64) -> bool {
        !self.graph_unresolved_parents(sn).is_empty()
    }

    /// The parents event `sn` named that had no sequence number when the file was written, as the references the
    /// event declared, in the order it declared them. Empty for an event with none or one the file does not hold.
    pub fn graph_unresolved_parents(&self, sn: u64) -> &[UnresolvedEdge] {
        let Some(graph) = self.footer().graph.as_ref() else {
            return &[];
        };
        let start = graph.unresolved.partition_point(|edge| edge.child < sn);
        let end = graph.unresolved.partition_point(|edge| edge.child <= sn);
        graph.unresolved.get(start..end).unwrap_or_default()
    }

    /// The granule of a graph-profile file that holds event `sn`, or `None` when the file does not hold it.
    pub fn graph_granule_of(&self, sn: u64) -> Option<u32> {
        let footer = self.footer();
        let row = sn.checked_sub(footer.graph.as_ref()?.first_sn)?;
        let after = footer
            .granules
            .partition_point(|granule| granule.first_row_ordinal <= row);
        let granule = footer.granules.get(after.checked_sub(1)?)?;
        (row < granule.first_row_ordinal + u64::from(granule.row_count)).then_some(granule.granule_id)
    }

    /// The file-absolute position and length of granule `granule_id`'s edge block.
    pub(super) fn graph_block_range(&self, granule_id: u32) -> Result<(u64, u64), FormatError> {
        let footer = self.footer();
        let missing = FormatError::RefOutOfRange {
            what: "no graph edge block for the granule",
        };
        let entry = footer
            .graph
            .as_ref()
            .and_then(|directory| directory.blocks.iter().find(|entry| entry.granule_id == granule_id))
            .ok_or(missing)?;
        let position = self.file_position(granule_id, entry.offset)?;
        Ok((position as u64, entry.len))
    }
}

#[cfg(test)]
#[path = "test/graph.rs"]
mod tests;
