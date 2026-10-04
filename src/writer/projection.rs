//! Rewrites a tenant's ingest files into a second copy sorted by entity, so one entity's history (a chat room, an
//! account, a deal) sits in one or a few neighbouring granules instead of being spread across every file.
//!
//! The copy is the entity projection: a read alternative over the same rows, never extra data. Ingest keeps writing
//! the primary `(epoch, sequence)` order unchanged; compaction builds this copy from the rows it already holds, and
//! the manifest publishes it as a [`FileType::EntityProjection`](crate::lifecycle::FileType::EntityProjection) entry.
//! Each granule's existing per-block min/max of the entity hash columns then brackets the entities it holds, so a
//! reader finds an entity's granules from the footer alone (see
//! [`HefFile::entity_granules`](crate::layout::reader::HefFile::entity_granules)).
//!
//! See: hef-layout-and-clustering/spec.md

use crate::error::FormatError;
use crate::invariants::EncodeExecutor;
use crate::layout::footer::{SortDirection, SortednessProof};
use crate::writer::build::{BuildInput, BuildRow, BuiltHef, HefBuildConfig, build_hef_file_in_order};
use rayon::prelude::*;

/// The order a file's rows are sorted in, which decides the order the build checks them against and the sortedness
/// proof it records.
///
/// See: hef-layout-and-clustering/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowOrder {
    /// `(entity_id_hash_low, entity_id_hash_high, epoch, sequence)`: the entity projection.
    Entity,
    /// `(epoch, sequence)`: the primary projection every ingest file is written in.
    Sequence,
}

/// A row's position in its file's sort order, compared as a tuple.
pub(crate) type RowKey = (u64, u64, u64, u64);

impl RowOrder {
    /// Where `row` sorts in this order.
    pub(crate) fn key(self, row: &BuildRow) -> RowKey {
        match self {
            RowOrder::Entity => (
                row.envelope.entity_id_hash_low,
                row.envelope.entity_id_hash_high,
                row.epoch,
                row.sequence,
            ),
            RowOrder::Sequence => (row.epoch, row.sequence, 0, 0),
        }
    }

    /// The rule a row out of this order breaks.
    pub(crate) fn rule(self) -> &'static str {
        match self {
            RowOrder::Entity => "rows must be strictly (entity_id_hash, epoch, sequence) ordered",
            RowOrder::Sequence => "rows must be strictly (epoch, sequence) ordered",
        }
    }

    /// The proof each granule records that its rows are sorted in this order.
    pub(crate) fn sortedness_proof(self) -> SortednessProof {
        let column_names: &[&str] = match self {
            RowOrder::Entity => &["entity_id_hash_low", "entity_id_hash_high", "epoch", "sequence"],
            RowOrder::Sequence => &["epoch", "sequence"],
        };
        SortednessProof {
            column_names: column_names.iter().map(|name| (*name).to_owned()).collect(),
            direction: SortDirection::Ascending,
        }
    }
}

/// Builds the entity projection of `rows`: one file holding exactly those rows, sorted by entity and then by
/// `(epoch, sequence)`, so each entity's events sit side by side.
///
/// Hand it every row of the ingest files being compacted, in any order; one tenant, as `config` names. Each
/// `(epoch, sequence)` point may appear once - a repeat is refused, since the copy must hold the same rows as its
/// sources and no more. Publish the result as a `FileType::EntityProjection` entry covering the same journal range as
/// those sources.
pub fn build_entity_projection<R: BuildInput>(
    rows: Vec<R>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<BuiltHef, FormatError> {
    let mut rows = R::into_build_rows(rows);
    let mut points: Vec<(u64, u64)> = rows.iter().map(|row| (row.epoch, row.sequence)).collect();
    points.par_sort_unstable();
    let total = points.len();
    points.dedup();
    if points.len() != total {
        return Err(FormatError::Structural {
            rule: "an entity projection holds each (epoch, sequence) point once",
        });
    }
    rows.par_sort_unstable_by_key(|row| RowOrder::Entity.key(row));
    build_hef_file_in_order(rows, config, encode, RowOrder::Entity)
}

#[cfg(test)]
#[path = "test/projection.rs"]
mod tests;
