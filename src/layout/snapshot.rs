//! Looks up one key's state in a snapshot file: a file holding an epoch's whole keyed state (Matrix users, devices,
//! or end-to-end key shards, say), one row per key, sorted by key.
//!
//! The file's footer carries a small key index: a learned model of which row each key sits at, and the first key of
//! every granule. A lookup predicts the key's row window from the footer alone, picks the one granule that can hold
//! the key, fetches that granule, and confirms the key there. Opened from object storage, that is one request for the
//! file's tail and one for the granule.
//!
//! See: hef-file-layout/spec.md

use super::Profile;
use super::constant::{SNAPSHOT_KEY_ERROR_BOUND, SNAPSHOT_KEY_INDEX_FRAMING_BYTES, SNAPSHOT_KEY_LENGTH_BYTES};
use super::footer::GranuleEntry;
use super::optional_features;
use super::reader::{HefFile, PayloadRead};
use crate::columns::column_ids;
use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::indexes::learned_position::LearnedPositionIndex;

/// Finds which granule of a snapshot file can hold a key, from the footer alone.
///
/// A snapshot file's rows are sorted by key (the row's `entity_id`), one row per key. The index keeps a learned model
/// mapping a key to its approximate row and the first key of each granule; together they name the one granule a key
/// can be in without reading any column data.
///
/// The model reads the eight bytes of a key that follow the prefix every key in the file shares, so keys that differ
/// only past those bytes share a model key; `longest_run` widens the predicted window to cover such a run.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotKeyIndex {
    /// Bytes every key in the file starts with.
    pub common_prefix: Vec<u8>,
    /// One per granule, in granule order.
    pub granule_first_keys: Vec<String>,
    /// The most rows that share one model key; at least one.
    pub longest_run: u64,
    pub model: LearnedPositionIndex,
}

impl SnapshotKeyIndex {
    /// Builds the index over a snapshot file's keys, one per row in row order, cut into granules starting at
    /// `granule_first_rows`. Refuses keys that are not strictly ascending: a snapshot holds each key once, in order.
    pub fn build(keys: &[&str], granule_first_rows: &[usize]) -> Result<Self, FormatError> {
        if !keys
            .iter()
            .zip(keys.iter().skip(1))
            .all(|(before, after)| before < after)
        {
            return Err(FormatError::Structural {
                rule: "a snapshot file's rows must be strictly ascending by entity_id",
            });
        }
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
            return Err(FormatError::Structural {
                rule: "a HEF file must carry at least one row",
            });
        };
        let prefix_len = first.bytes().zip(last.bytes()).take_while(|(a, b)| a == b).count();
        let mut model_keys: Vec<i64> = Vec::new();
        let mut first_rows: Vec<u64> = Vec::new();
        let mut longest_run = 1u64;
        for (row, key) in keys.iter().enumerate() {
            let model_key = model_key(key.as_bytes(), prefix_len);
            match (model_keys.last(), first_rows.last()) {
                (Some(&previous), Some(&run_start)) if previous == model_key => {
                    longest_run = longest_run.max(row as u64 - run_start + 1);
                }
                _ => {
                    model_keys.push(model_key);
                    first_rows.push(row as u64);
                }
            }
        }
        let granule_first_keys = granule_first_rows
            .iter()
            .map(|&row| keys.get(row).map(|key| (*key).to_owned()))
            .collect::<Option<Vec<_>>>()
            .ok_or(FormatError::Structural {
                rule: "a granule starts past the snapshot file's last row",
            })?;
        Ok(Self {
            common_prefix: first.as_bytes().get(..prefix_len).unwrap_or_default().to_vec(),
            granule_first_keys,
            longest_run,
            model: LearnedPositionIndex::build_at_rows(
                column_ids::ENTITY_ID,
                0,
                &model_keys,
                &first_rows,
                keys.len() as u64,
                SNAPSHOT_KEY_ERROR_BOUND,
            ),
        })
    }

    /// Whether every granule's first key sorts at or after the one before it, as a writer always stores them. Checked
    /// once per open file so [`Self::locate`] can binary-search the first keys instead of walking them.
    pub(crate) fn first_keys_in_order(&self) -> bool {
        self.granule_first_keys
            .iter()
            .zip(self.granule_first_keys.iter().skip(1))
            .all(|(before, after)| before <= after)
    }

    /// The position in `granules` of the one granule that can hold `key`, with the file rows (inclusive) the key can
    /// sit in, or `None` when no granule can. `first_keys_in_order` is [`Self::first_keys_in_order`]: when set, the
    /// candidate granules' first keys are binary-searched; otherwise they are walked from the last one back.
    fn locate(&self, key: &str, granules: &[GranuleEntry], first_keys_in_order: bool) -> Option<(usize, u64, u64)> {
        if !key.as_bytes().starts_with(&self.common_prefix) {
            return None;
        }
        let window = self
            .model
            .estimate_position(model_key(key.as_bytes(), self.common_prefix.len()));
        let from = window.search_from;
        let to = window.search_to.saturating_add(self.longest_run.saturating_sub(1));
        let first =
            granules.partition_point(|granule| granule.first_row_ordinal + u64::from(granule.row_count) <= from);
        let end = granules
            .partition_point(|granule| granule.first_row_ordinal <= to)
            .min(self.granule_first_keys.len());
        let candidates = self.granule_first_keys.get(first..end)?;
        let at_or_before = |first_key: &String| first_key.as_str() <= key;
        let found = if first_keys_in_order {
            candidates.partition_point(at_or_before).checked_sub(1)
        } else {
            candidates.iter().rposition(at_or_before)
        };
        found.map(|offset| (first + offset, from, to))
    }

    /// Serializes the index for the footer's snapshot-keys section. Pairs with [`Self::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let model_len = self.model.encoded_len();
        let keys_len: usize = self
            .granule_first_keys
            .iter()
            .map(|key| SNAPSHOT_KEY_LENGTH_BYTES + key.len())
            .sum();
        let mut out =
            Writer::with_capacity(SNAPSHOT_KEY_INDEX_FRAMING_BYTES + self.common_prefix.len() + keys_len + model_len);
        out.put_u32(self.common_prefix.len() as u32);
        out.put_slice(&self.common_prefix);
        out.put_u32(self.granule_first_keys.len() as u32);
        for key in &self.granule_first_keys {
            out.put_u32(key.len() as u32);
            out.put_slice(key.as_bytes());
        }
        out.put_u64(self.longest_run);
        out.put_u32(model_len as u32);
        self.model.encode_into(&mut out);
        out.into_bytes()
    }

    /// Rebuilds an index from [`Self::encode`] output. Refuses truncated or trailing bytes, a zero run length, and keys
    /// that are not UTF-8.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        let prefix_len = reader.u32("snapshot key prefix length")? as usize;
        let common_prefix = reader.take(prefix_len, "snapshot key prefix")?.to_vec();
        let count = reader.u32("snapshot granule key count")? as usize;
        let mut granule_first_keys = Vec::with_capacity(reader.capacity_hint(count, 4));
        for _ in 0..count {
            let len = reader.u32("snapshot granule key length")? as usize;
            let key = reader.take(len, "snapshot granule key")?;
            granule_first_keys.push(String::from_utf8(key.to_vec()).map_err(|_| FormatError::InvalidUtf8 {
                what: "snapshot granule key",
            })?);
        }
        let longest_run = reader.u64("snapshot longest run")?;
        let model_len = reader.u32("snapshot key model length")? as usize;
        let model = LearnedPositionIndex::decode(reader.take(model_len, "snapshot key model")?)?;
        if longest_run == 0 || reader.remaining() != 0 {
            return Err(FormatError::Structural {
                rule: "malformed snapshot key index",
            });
        }
        Ok(Self {
            common_prefix,
            granule_first_keys,
            longest_run,
            model,
        })
    }
}

/// The eight bytes of `key` after its first `prefix_len`, zero-padded, as an order-preserving signed integer: a key
/// that sorts after another never maps lower.
fn model_key(key: &[u8], prefix_len: usize) -> i64 {
    let mut bytes = [0u8; 8];
    for (slot, byte) in bytes.iter_mut().zip(key.iter().skip(prefix_len)) {
        *slot = *byte;
    }
    (u64::from_be_bytes(bytes) ^ (1 << 63)) as i64
}

impl HefFile {
    /// The value a snapshot file holds for `key` (a row's `entity_id`), or `None` when the file holds no such key.
    ///
    /// Reads the one granule the footer's key index names, so a file opened with [`Self::open_remote`] answers with
    /// a single request beyond the tail fetched at open. Without a usable key index the granules are searched in
    /// turn, with the same answer. Refuses a file that is not a snapshot file.
    pub fn snapshot_get(&self, key: &str) -> Result<Option<PayloadRead>, FormatError> {
        let footer = self.footer();
        if footer.profile != Profile::Snapshot {
            return Err(FormatError::Structural {
                rule: "a snapshot lookup needs a snapshot file",
            });
        }
        let index = footer
            .snapshot_keys
            .as_ref()
            .filter(|_| self.usable_optional_features() & optional_features::SNAPSHOT_KEY_INDEX != 0);
        // The file rows (inclusive) the key can sit in, and the granules that can hold them.
        let (candidates, from, to) = match index {
            Some(index) => match index.locate(key, &footer.granules, self.snapshot_first_keys_in_order) {
                Some((granule, from, to)) => (granule..granule + 1, from, to),
                None => return Ok(None),
            },
            None => (0..footer.granules.len(), 0, u64::MAX),
        };
        for granule in footer.granules.get(candidates).unwrap_or_default() {
            // Fetches the granule's whole stripe in one request; every read below is then served from it.
            if let Some(stripe) = self.stripe_by_id(granule.stripe_id) {
                self.data(stripe.file_offset as usize, stripe.byte_len as usize, "snapshot stripe")?;
            }
            let first_row = granule.first_row_ordinal;
            let window = (
                usize::try_from(from.saturating_sub(first_row)).unwrap_or(usize::MAX),
                usize::try_from(to.saturating_sub(first_row)).unwrap_or(usize::MAX),
            );
            if let Some(row) = self.sorted_string_row(
                column_ids::ENTITY_ID,
                granule.granule_id,
                granule.row_count,
                window,
                key,
            )? {
                return self.payload(first_row + row as u64).map(Some);
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
#[path = "test/snapshot.rs"]
mod tests;
