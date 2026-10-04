//! Finds which file holds an event, given the event's external protocol id (a Matrix `$...` id, say), without
//! opening every file.
//!
//! Each file already indexes its own external ids; this is the layer above it. As files are published, the ids they
//! hold are recorded in a durable store the embedding application backs, each pointing at the file and row that hold
//! the event. When the same id appears in several files - a compaction rewrote it, or a producer resent it - the file
//! of the newest generation wins.
//!
//! See: hef-query-metadata-and-indexes/spec.md

use crate::error::{ExternalIdIndexError, StorageError};
use crate::events::TenantId;
use crate::layout::reader::HefFile;

/// Where one event lives: the file that holds it, the generation that published that file, and the event's row in
/// it.
///
/// See: hef-query-metadata-and-indexes/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalIdLocation {
    pub file_id: u128,
    /// The publishing generation; a newer one supersedes an older one for the same id.
    pub generation: u64,
    pub row_ordinal: u64,
}

/// The durable home of the cross-file external-id index: one record per (tenant, external id). The embedding
/// application backs it with durable storage (a keyed table, say); tests use an in-memory map.
///
/// See: hef-query-metadata-and-indexes/spec.md
pub trait ExternalIdStore {
    /// The location recorded for `external_id` in `tenant_id`, or `None` when nothing is recorded.
    fn get(&self, tenant_id: TenantId, external_id: &[u8]) -> Result<Option<ExternalIdLocation>, StorageError>;

    /// Records `location` for `external_id` in `tenant_id`, replacing whatever was recorded before.
    fn put(
        &mut self,
        tenant_id: TenantId,
        external_id: &[u8],
        location: ExternalIdLocation,
    ) -> Result<(), StorageError>;
}

/// Records every external id `file` holds, pointing each at this file's generation and row, and returns how many
/// records it wrote. An id already recorded from a newer generation keeps that record, so files may be recorded in
/// any order and the newest still wins. Call it once a file is published.
pub fn record_file(store: &mut impl ExternalIdStore, file: &HefFile) -> Result<usize, ExternalIdIndexError> {
    let header = file.header();
    let mut written = 0;
    for (row_ordinal, external_id) in file.external_ids()? {
        let newer_recorded = store
            .get(header.tenant_id, &external_id)?
            .is_some_and(|recorded| recorded.generation > header.generation_id);
        if newer_recorded {
            continue;
        }
        store.put(
            header.tenant_id,
            &external_id,
            ExternalIdLocation {
                file_id: header.file_id,
                generation: header.generation_id,
                row_ordinal,
            },
        )?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
#[path = "test/external_id.rs"]
mod tests;
