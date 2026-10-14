//! Per-recipient message queues - to-device messages, device inboxes, an outgoing federation queue - stored as inbox
//! files and drained by acknowledgement.
//!
//! An inbox file is an ordinary file with the [`Profile::Inbox`](super::Profile::Inbox) profile whose rows are grouped
//! by recipient (the row's entity) and, within a recipient, kept in queue order (`(epoch, sequence)`). Its footer
//! carries a recipient directory saying where each recipient's rows are and the newest one among them. Each recipient
//! acknowledges its queue up to a point; reading the queue never returns a row at or below that point, nor a row the
//! application deleted, and a file every recipient has acknowledged in full can be retired.
//!
//! See: hef-file-layout/spec.md

use super::entity_scan::{DeletesAndCorrections, EntityEvent};
use super::event::read_u64s;
use super::footer::{GranuleEntry, InboxRecipientEntry};
use super::optional_features;
use super::reader::{HefFile, granules_overlapping_rows};
use crate::columns::column_ids;
use crate::deletes::DeletionVector;
use crate::error::{FormatError, InboxError};
use crate::events::{SequencePoint, TenantId};

/// Who an inbox row is addressed to: the row's entity, by the two identity hashes every row stores beside its entity
/// id.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Recipient {
    pub entity_id_hash_high: u64,
    pub entity_id_hash_low: u64,
}

/// How far each recipient has acknowledged its queue: everything at or below that point is done with and is never
/// served again.
///
/// Acknowledgements must survive restarts, so the application keeps them in its own durable storage and backs this
/// interface with it. A recipient's watermark only ever moves forward.
///
/// See: hef-file-lifecycle/spec.md
pub trait AckWatermarks {
    /// The newest queue point `recipient` has acknowledged, or `None` when it has acknowledged nothing yet.
    fn acked_through(&self, tenant_id: TenantId, recipient: Recipient) -> Result<Option<SequencePoint>, InboxError>;
}

/// Reads `recipient`'s queue from `files`, oldest first: every row addressed to it that it has not yet acknowledged
/// and the application has not deleted.
///
/// Hand it the inbox files of the snapshot being read; files of another tenant are ignored. The acknowledgement point
/// comes from `watermarks` and the deleted rows from `deletes`, so a row at or below the recipient's watermark, or one
/// deleted in its file, is never returned.
pub fn read_inbox(
    files: &[&HefFile],
    tenant_id: TenantId,
    recipient: Recipient,
    watermarks: &dyn AckWatermarks,
    deletes: &dyn DeletesAndCorrections,
) -> Result<Vec<EntityEvent>, InboxError> {
    let acked = watermarks.acked_through(tenant_id, recipient)?;
    let mut events = Vec::new();
    for file in files.iter().filter(|file| file.header().tenant_id == tenant_id) {
        let granules = recipient_granules(file, recipient, acked);
        if granules.is_empty() {
            continue;
        }
        let deleted = deletes.deletion_vector(tenant_id, file.header().file_id)?;
        read_rows(file, &granules, recipient, acked, deleted.as_ref(), &mut events)?;
    }
    events.sort_by_key(|event| event.point);
    Ok(events)
}

impl HefFile {
    /// This file's recipient directory, one entry per recipient in row order, or `None` when the file carries none (it
    /// is not an inbox file, or its directory was dropped).
    pub fn inbox_recipients(&self) -> Option<&[InboxRecipientEntry]> {
        let directory = &self.footer().inbox_recipients;
        (self.usable_optional_features() & optional_features::INBOX_RECIPIENT_DIRECTORY != 0 && !directory.is_empty())
            .then_some(directory.as_slice())
    }

    /// True when every recipient with rows in this file has acknowledged its newest one, so nothing in the file can be
    /// served again and it may be retired. False for a file without a recipient directory, which cannot tell.
    pub fn fully_acked(&self, watermarks: &dyn AckWatermarks) -> Result<bool, InboxError> {
        let Some(directory) = self.inbox_recipients() else {
            return Ok(false);
        };
        let tenant_id = self.header().tenant_id;
        for entry in directory {
            let acked = watermarks.acked_through(tenant_id, recipient_of(entry))?;
            if acked.is_none_or(|acked| acked < last_point(entry)) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The granules of `file` that can hold rows of `recipient` above `acked`. With a recipient directory these are the
/// granules overlapping the recipient's row range, and none when it has no rows or has acknowledged all of them;
/// without one, the granules its entity hash bounds admit.
fn recipient_granules(file: &HefFile, recipient: Recipient, acked: Option<SequencePoint>) -> Vec<&GranuleEntry> {
    let Some(directory) = file.inbox_recipients() else {
        let from = acked.unwrap_or(SequencePoint { epoch: 0, sequence: 0 });
        let to = SequencePoint {
            epoch: u64::MAX,
            sequence: u64::MAX,
        };
        return file.entity_granules(recipient.entity_id_hash_low, recipient.entity_id_hash_high, from, to);
    };
    let key = (recipient.entity_id_hash_low, recipient.entity_id_hash_high);
    let found = directory.binary_search_by_key(&key, |entry| (entry.entity_id_hash_low, entry.entity_id_hash_high));
    let Some(entry) = found.ok().and_then(|index| directory.get(index)) else {
        return Vec::new();
    };
    if acked.is_some_and(|acked| last_point(entry) <= acked) {
        return Vec::new();
    }
    let end = entry.first_row_ordinal.saturating_add(entry.row_count);
    granules_overlapping_rows(&file.footer().granules, &(entry.first_row_ordinal..end))
        .iter()
        .filter(|granule| {
            granule.first_row_ordinal < end
                && entry.first_row_ordinal < granule.first_row_ordinal + u64::from(granule.row_count)
        })
        .collect()
}

/// Adds the rows of `granules` that belong to `recipient`, sit above `acked`, and are not deleted.
fn read_rows(
    file: &HefFile,
    granules: &[&GranuleEntry],
    recipient: Recipient,
    acked: Option<SequencePoint>,
    deleted: Option<&DeletionVector>,
    out: &mut Vec<EntityEvent>,
) -> Result<(), FormatError> {
    for granule in granules {
        let rows = granule.row_count as usize;
        let epochs = read_u64s(file, column_ids::EPOCH, granule.granule_id, rows)?;
        let sequences = read_u64s(file, column_ids::SEQUENCE, granule.granule_id, rows)?;
        let lows = read_u64s(file, column_ids::ENTITY_ID_HASH_LOW, granule.granule_id, rows)?;
        let highs = read_u64s(file, column_ids::ENTITY_ID_HASH_HIGH, granule.granule_id, rows)?;
        let mut envelopes = None;
        let keys = epochs.iter().zip(&sequences).zip(lows.iter().zip(&highs));
        for (row_in_granule, ((&epoch, &sequence), (&low, &high))) in keys.enumerate() {
            let point = SequencePoint { epoch, sequence };
            let row_ordinal = granule.first_row_ordinal + row_in_granule as u64;
            if low != recipient.entity_id_hash_low
                || high != recipient.entity_id_hash_high
                || acked.is_some_and(|acked| point <= acked)
                || deleted.is_some_and(|vector| vector.is_deleted(row_ordinal))
            {
                continue;
            }
            if envelopes.is_none() {
                envelopes = Some(file.granule_envelopes(granule.granule_id)?);
            }
            let envelope = envelopes
                .as_ref()
                .and_then(|envelopes| envelopes.get(row_in_granule))
                .cloned()
                .ok_or(FormatError::RefOutOfRange { what: "row in granule" })?;
            out.push(EntityEvent {
                envelope,
                payload: file.payload(row_ordinal)?,
                point,
            });
        }
    }
    Ok(())
}

fn recipient_of(entry: &InboxRecipientEntry) -> Recipient {
    Recipient {
        entity_id_hash_high: entry.entity_id_hash_high,
        entity_id_hash_low: entry.entity_id_hash_low,
    }
}

fn last_point(entry: &InboxRecipientEntry) -> SequencePoint {
    SequencePoint {
        epoch: entry.last_epoch,
        sequence: entry.last_sequence,
    }
}

#[cfg(test)]
#[path = "test/inbox.rs"]
mod tests;
