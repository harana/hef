//! Builds what a file keeps of each event's original form next to its canonical one: the exact payload bytes it
//! arrived as, its external protocol id, and its multi-signer provenance, plus the two indexes that find events by
//! those ids - the external-id index and the per-granule reference filters.
//!
//! Every column here is optional and absent from files whose rows never carry it, so a stream that opts into none of
//! this pays nothing. The columns ride the ordinary block machinery, so they encode, page, and compress like any other
//! string column.
//!
//! See: hef-write-path/spec.md

use super::build::BuildRow;
use super::constant::REFERENCE_FILTER_BITS_PER_KEY;
use crate::columns::{ColumnSpec, SOURCE_FORM_COLUMNS, column_ids};
use crate::encoding::{ColumnData, StringColumn};
use crate::error::FormatError;
use crate::events::constant::EXTERNAL_ID_MAX_BYTES;
use crate::events::provenance::{SignerSignature, hex_lower};
use crate::events::relationships::RelationshipKind;
use crate::indexes::probabilistic::SplitBlockBloomFilter;
use crate::indexes::stable_hash;
use crate::layout::footer::{ExternalIdEntry, Footer, ReferenceFilterEntry};
use std::borrow::Cow;

/// One granule's block of one source-form column: the column id, a presence bitmap with one bit per row, and the
/// present rows' values in row order.
pub(crate) type SourceFormBlock = (u32, Vec<u8>, ColumnData);

/// The file-wide indexes over the events' original ids, ready for the footer: where each external id lives, and which
/// granules might hold a given relationship reference.
///
/// See: hef-query-metadata-and-indexes/spec.md
pub(crate) struct SourceFormIndexes {
    pub(crate) external_ids: Vec<ExternalIdEntry>,
    pub(crate) reference_filters: Option<Vec<ReferenceFilterEntry>>,
}

impl SourceFormIndexes {
    /// The indexes a sparse update keeps: its source's own, since a sparse update never changes an external id or a
    /// relationship reference.
    pub(crate) fn from_footer(footer: &Footer) -> Self {
        Self {
            external_ids: footer.external_ids.clone(),
            reference_filters: footer.reference_filters.clone(),
        }
    }
}

/// The source-form columns a file declares: for a fresh build, each column some row carries a value for; for a sparse
/// update, the ones its source declares, since the update borrows those blocks unchanged.
pub(crate) fn carried_columns(source: Option<&Footer>, rows: &[BuildRow]) -> Vec<&'static ColumnSpec> {
    SOURCE_FORM_COLUMNS
        .iter()
        .filter(|spec| match source {
            Some(footer) => footer.columns.iter().any(|column| column.column_id == spec.column_id),
            None => rows.iter().any(|row| carries(row, spec.column_id)),
        })
        .collect()
}

/// Everything a fresh build stores for the events' original form: each granule's blocks of the `carried` columns, in
/// `carried` order, and the file indexes. `granule_row_ranges` are the granules' `[start, end)` row ranges, and every
/// row's index in `rows` is its row ordinal.
///
/// Refuses an external id outside 1 to 255 bytes and a raw payload that is not UTF-8 text - the raw column keeps text
/// bodies such as JSON; a binary body is already byte-exact in the canonical payload.
pub(crate) fn build(
    rows: &[BuildRow],
    granule_row_ranges: &[(usize, usize)],
    carried: &[&ColumnSpec],
) -> Result<(Vec<Vec<SourceFormBlock>>, SourceFormIndexes), FormatError> {
    let blocks = granule_row_ranges
        .iter()
        .map(|&(start, end)| {
            let granule = rows.get(start..end).unwrap_or_default();
            carried
                .iter()
                .map(|spec| column_block(granule, spec.column_id))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let indexes = SourceFormIndexes {
        external_ids: external_id_index(rows),
        reference_filters: reference_filters(rows, granule_row_ranges),
    };
    Ok((blocks, indexes))
}

fn carries(row: &BuildRow, column_id: u32) -> bool {
    match column_id {
        column_ids::EXTERNAL_ID => row.external_id.is_some(),
        column_ids::MATRIX_ROOM_VERSION | column_ids::SIGNER_SIGNATURES => row.matrix_provenance.is_some(),
        column_ids::RAW_PAYLOAD => row.raw_payload.is_some(),
        _ => false,
    }
}

/// One row's stored text for a source-form column, or `None` when the row carries no value for it.
fn stored_text(row: &BuildRow, column_id: u32) -> Result<Option<Cow<'_, str>>, FormatError> {
    Ok(match column_id {
        column_ids::EXTERNAL_ID => match &row.external_id {
            Some(id) if id.is_empty() || id.len() > EXTERNAL_ID_MAX_BYTES => {
                return Err(FormatError::Structural {
                    rule: "an external id is 1 to 255 bytes",
                });
            }
            Some(id) => Some(Cow::Owned(hex_lower(id))),
            None => None,
        },
        column_ids::MATRIX_ROOM_VERSION => row
            .matrix_provenance
            .as_ref()
            .map(|provenance| Cow::Borrowed(provenance.room_version())),
        column_ids::SIGNER_SIGNATURES => row
            .matrix_provenance
            .as_ref()
            .map(|provenance| Cow::Owned(SignerSignature::join(provenance.signatures()))),
        column_ids::RAW_PAYLOAD => match &row.raw_payload {
            Some(bytes) => Some(Cow::Borrowed(
                simdutf8::basic::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 { what: "raw payload" })?,
            )),
            None => None,
        },
        _ => None,
    })
}

fn column_block(rows: &[BuildRow], column_id: u32) -> Result<SourceFormBlock, FormatError> {
    let mut presence = vec![0u8; rows.len().div_ceil(8)];
    let mut values = StringColumn::new();
    for (index, row) in rows.iter().enumerate() {
        if let Some(text) = stored_text(row, column_id)? {
            if let Some(byte) = presence.get_mut(index / 8) {
                *byte |= 1 << (index % 8);
            }
            values.push(Some(&*text));
        }
    }
    Ok((column_id, presence, ColumnData::Strings(values)))
}

/// Every row's external id as a sorted `(id hash, row ordinal)` index.
fn external_id_index(rows: &[BuildRow]) -> Vec<ExternalIdEntry> {
    let mut entries: Vec<ExternalIdEntry> = rows
        .iter()
        .enumerate()
        .filter_map(|(ordinal, row)| {
            row.external_id.as_ref().map(|id| ExternalIdEntry {
                id_hash: stable_hash(id),
                row_ordinal: ordinal as u64,
            })
        })
        .collect();
    entries.sort_unstable_by_key(|entry| (entry.id_hash, entry.row_ordinal));
    entries
}

/// One filter per (relationship column, granule) that holds any reference of that kind, over the references' stored
/// text, or `None` for a file whose rows declare no relationships and so carries no relationship columns.
fn reference_filters(rows: &[BuildRow], granule_row_ranges: &[(usize, usize)]) -> Option<Vec<ReferenceFilterEntry>> {
    if !rows.iter().any(|row| row.relationships.is_some()) {
        return None;
    }
    let mut filters = Vec::new();
    let mut text = String::new();
    let mut hashes = Vec::new();
    for kind in RelationshipKind::ALL {
        for (granule_id, &(start, end)) in granule_row_ranges.iter().enumerate() {
            hashes.clear();
            for relationships in rows
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .filter_map(|row| row.relationships.as_ref())
            {
                for reference in relationships.refs().iter().filter(|reference| reference.kind == kind) {
                    text.clear();
                    reference.column_value_into(&mut text);
                    hashes.push(stable_hash(text.as_bytes()));
                }
            }
            if hashes.is_empty() {
                continue;
            }
            hashes.sort_unstable();
            hashes.dedup();
            filters.push(ReferenceFilterEntry {
                column_id: kind.column_id(),
                filter: SplitBlockBloomFilter::build(&hashes, REFERENCE_FILTER_BITS_PER_KEY).encode(),
                granule_id: granule_id as u32,
            });
        }
    }
    filters.sort_by_key(|entry| (entry.column_id, entry.granule_id));
    Some(filters)
}

#[cfg(test)]
#[path = "test/source_form.rs"]
pub(crate) mod tests;
