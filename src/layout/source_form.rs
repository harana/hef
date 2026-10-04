//! Reads back what a file keeps of each event's original form: the exact payload bytes it arrived as, its external
//! protocol id, and its multi-signer provenance - and finds events by external id through the file's own index
//! instead of scanning every granule.
//!
//! See: hef-file-layout/spec.md

use super::footer::{Footer, GranuleEntry};
use super::reader::{ColumnRead, HefFile};
use crate::columns::column_ids;
use crate::encoding::ColumnData;
use crate::error::{FormatError, ProvenanceError};
use crate::events::matrix::MatrixProvenance;
use crate::events::provenance::SignerSignature;
use crate::indexes::probabilistic::SplitBlockBloomFilter;
use crate::indexes::stable_hash;
use std::borrow::Cow;

impl HefFile {
    /// The event's payload exactly as it arrived, byte for byte - for a signed JSON event, the bytes whose signature
    /// re-verifies. `None` when the row's stream did not keep raw payloads, or the file carries none at all.
    ///
    /// A deletion or correction applies to this the same way it applies to [`Self::payload`]: a caller drops deleted
    /// rows before reading either, and a field-level redaction withholds the whole raw payload (see
    /// [`FieldDeletionVector::withholds_raw_payload`](crate::deletes::FieldDeletionVector::withholds_raw_payload)).
    pub fn raw_payload(&self, row_ordinal: u64) -> Result<Option<Cow<'_, [u8]>>, FormatError> {
        Ok(self
            .source_form_text(column_ids::RAW_PAYLOAD, row_ordinal)?
            .map(|text| Cow::Owned(text.into_bytes())))
    }

    /// The event's external protocol id (a Matrix `$...` id, say), or `None` when the row carries none.
    pub fn external_id(&self, row_ordinal: u64) -> Result<Option<Vec<u8>>, FormatError> {
        self.source_form_text(column_ids::EXTERNAL_ID, row_ordinal)?
            .map(|hex| {
                hex_simd::decode_to_vec(hex.as_bytes()).map_err(|_| FormatError::Structural {
                    rule: "a stored external id is lowercase hex",
                })
            })
            .transpose()
    }

    /// The row that holds the event with `external_id`, found through the file's external-id index without scanning
    /// a granule. When the file holds the id more than once, the last row - the newest event - answers. `None` when the
    /// file does not hold the id.
    pub fn row_by_external_id(&self, external_id: &[u8]) -> Result<Option<u64>, FormatError> {
        Ok(self.rows_with_external_id(external_id)?.last().copied())
    }

    /// Every row that holds the event with `external_id`, in row order. Usually one row, or none.
    pub fn rows_with_external_id(&self, external_id: &[u8]) -> Result<Vec<u64>, FormatError> {
        let id_hash = stable_hash(external_id);
        let entries = &self.footer().external_ids;
        let first = entries.partition_point(|entry| entry.id_hash < id_hash);
        let mut rows = Vec::new();
        for entry in entries
            .get(first..)
            .unwrap_or_default()
            .iter()
            .take_while(|entry| entry.id_hash == id_hash)
        {
            // Two ids can share a hash, so every candidate is confirmed against the stored id.
            if self.external_id(entry.row_ordinal)?.as_deref() == Some(external_id) {
                rows.push(entry.row_ordinal);
            }
        }
        Ok(rows)
    }

    /// Every row's external id, in row order - what a cross-file index records for this file.
    pub fn external_ids(&self) -> Result<Vec<(u64, Vec<u8>)>, FormatError> {
        let mut ids = Vec::with_capacity(self.footer().external_ids.len());
        if !has_column(self.footer(), column_ids::EXTERNAL_ID) {
            return Ok(ids);
        }
        for granule in &self.footer().granules {
            let read = self.read_column(column_ids::EXTERNAL_ID, granule.granule_id)?;
            for (row, hex) in present_strings(&read, granule.row_count as usize) {
                let id = hex_simd::decode_to_vec(hex.as_bytes()).map_err(|_| FormatError::Structural {
                    rule: "a stored external id is lowercase hex",
                })?;
                ids.push((granule.first_row_ordinal + row as u64, id));
            }
        }
        Ok(ids)
    }

    /// The room version and server signatures stored for a Matrix event, ready to
    /// [`verify`](MatrixProvenance::verify) against [`Self::raw_payload`] and [`Self::external_id`]. `None` when the
    /// row carries none.
    pub fn matrix_provenance(&self, row_ordinal: u64) -> Result<Option<MatrixProvenance>, FormatError> {
        let (Some(room_version), Some(signatures)) = (
            self.source_form_text(column_ids::MATRIX_ROOM_VERSION, row_ordinal)?,
            self.source_form_text(column_ids::SIGNER_SIGNATURES, row_ordinal)?,
        ) else {
            return Ok(None);
        };
        let malformed = |_: ProvenanceError| FormatError::Structural {
            rule: "stored Matrix provenance does not parse",
        };
        let signatures = SignerSignature::split(&signatures).map_err(malformed)?;
        MatrixProvenance::new(&room_version, signatures)
            .map(Some)
            .map_err(malformed)
    }

    /// One row's text in a source-form column, or `None` when the file has no such column or the row has no value.
    fn source_form_text(&self, column_id: u32, row_ordinal: u64) -> Result<Option<String>, FormatError> {
        if !has_column(self.footer(), column_id) {
            return Ok(None);
        }
        let granule = granule_holding(self.footer(), row_ordinal)?;
        let read = self.read_column(column_id, granule.granule_id)?;
        let row = (row_ordinal - granule.first_row_ordinal) as usize;
        Ok(string_at(&read, granule.row_count as usize, row).map(str::to_owned))
    }
}

/// Whether granule `granule_id` might hold a reference whose stored text is `reference_text` in relationship column
/// `column_id`. Always `true` for a file written before the reference filters existed; `false` for a granule the
/// filters show holds no reference of that kind at all.
pub(crate) fn reference_may_be_in(
    footer: &Footer,
    column_id: u32,
    granule_id: u32,
    reference_text: &str,
) -> Result<bool, FormatError> {
    let Some(filters) = &footer.reference_filters else {
        return Ok(true);
    };
    match filters.binary_search_by_key(&(column_id, granule_id), |entry| (entry.column_id, entry.granule_id)) {
        Ok(index) => match filters.get(index) {
            Some(entry) => {
                SplitBlockBloomFilter::contains_encoded(&entry.filter, stable_hash(reference_text.as_bytes()))
            }
            None => Ok(true),
        },
        Err(_) => Ok(false),
    }
}

fn has_column(footer: &Footer, column_id: u32) -> bool {
    footer.columns.iter().any(|column| column.column_id == column_id)
}

fn granule_holding(footer: &Footer, row_ordinal: u64) -> Result<&GranuleEntry, FormatError> {
    let index = footer
        .granules
        .partition_point(|granule| granule.first_row_ordinal + u64::from(granule.row_count) <= row_ordinal);
    footer
        .granules
        .get(index)
        .filter(|granule| granule.first_row_ordinal <= row_ordinal)
        .ok_or(FormatError::RefOutOfRange {
            what: "row ordinal past the file's last row",
        })
}

/// The text at granule row `row` of a string block, whether the block stores a value per row or only the present
/// rows' values behind a presence bitmap.
fn string_at(read: &ColumnRead, row_count: usize, row: usize) -> Option<&str> {
    let ColumnData::Strings(values) = &read.data else {
        return None;
    };
    if values.len() == row_count {
        return values.get(row).flatten();
    }
    let present = |row: usize| {
        read.presence
            .get(row / 8)
            .is_some_and(|byte| byte & (1 << (row % 8)) != 0)
    };
    if !present(row) {
        return None;
    }
    values.get((0..row).filter(|&before| present(before)).count()).flatten()
}

/// Every present value of a string block with its granule row, in row order, in one walk over the block.
fn present_strings(read: &ColumnRead, row_count: usize) -> Vec<(usize, &str)> {
    let ColumnData::Strings(values) = &read.data else {
        return Vec::new();
    };
    if values.len() == row_count {
        return values
            .iter()
            .enumerate()
            .filter_map(|(row, value)| value.map(|text| (row, text)))
            .collect();
    }
    let mut dense = values.iter();
    (0..row_count)
        .filter(|&row| {
            read.presence
                .get(row / 8)
                .is_some_and(|byte| byte & (1 << (row % 8)) != 0)
        })
        .filter_map(|row| dense.next().flatten().map(|text| (row, text)))
        .collect()
}

#[cfg(test)]
#[path = "test/source_form.rs"]
mod tests;
