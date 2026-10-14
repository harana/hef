//! Small append-only write-ahead log objects (`log/{shard}/{seq}`): a run of records, each with its own checksum, read
//! front to back. They are kilobytes, not megabytes, so they skip the HEF container's 4 KiB header block and footer
//! index and carry only a few bytes of framing.
//!
//! Byte layout, every integer little-endian:
//!
//! ```text
//! header   "HEFL" | version u8 (1) | profile u8 (Profile::Log = 4)          6 bytes
//! record   len u32 | payload (len bytes) | CRC-64/NVME u64 of len+payload   12 + len bytes, repeated
//! trailer  u32::MAX | record count u32 | "HEFL"                             12 bytes
//! ```
//!
//! The root a reader verifies against is `BLAKE3` of those bytes. An object larger than one 1 MiB chunk group is
//! followed by the same outboard proof tree HEF stripes use (`required_features::STRIPE_VERIFIED_STREAMING`), so a
//! verified reader checks it one chunk group at a time; a KiB-range object carries no tree.
//!
//! The magic differs from HEF's `"HEF1"` at both ends, so `decode_header` and `HefFile::open` refuse a log object and
//! a log reader refuses a HEF file.
//!
//! See: hef-physical-artifacts/spec.md

use super::Profile;
use super::constant::{
    LOG_FORMAT_VERSION, LOG_HEADER_LEN, LOG_MAGIC, LOG_RECORD_FRAME_LEN, LOG_TRAILER_LEN, LOG_TRAILER_SENTINEL,
};
use crate::error::{FormatError, LogReadError};
use crate::file::bytes::{Reader, Writer};
use crate::file::constant::CHUNK_GROUP_BYTES;
use crate::file::integrity::{
    attach_outboard_tree, crc64_matches, crc64_nvme, hash_tree, parse_outboard_object, verify_object_range,
};

/// Builds one write-ahead log object in memory, one record at a time. [`finish`](Self::finish) seals it and hands back
/// the bytes to upload and the root to pin in the manifest or journal.
///
/// See: hef-physical-artifacts/spec.md
#[derive(Debug)]
pub struct LogObjectWriter {
    bytes: Writer,
    record_count: u32,
}

/// A finished log object, ready to upload.
///
/// See: hef-physical-artifacts/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedLogObject {
    pub bytes: Vec<u8>,
    /// `BLAKE3` of the object up to and including its trailer. Pin it where the object is committed and hand it to
    /// [`LogRecords::open_verified`].
    pub root: [u8; blake3::OUT_LEN],
}

/// Reads a log object's records front to back, yielding each payload borrowed from the object bytes.
///
/// The iteration ends with `None` after the trailer, or with one final error: [`LogReadError::TornTail`] when the
/// bytes stop short of the trailer (every record yielded before it is intact), or [`LogReadError::Corrupt`] when they
/// are damaged. Opened with a root, no byte is yielded before it has been checked against that root.
///
/// See: hef-physical-artifacts/spec.md
#[derive(Debug, Clone)]
pub struct LogRecords<'a> {
    content: &'a [u8],
    finished: bool,
    object: &'a [u8],
    position: usize,
    record_count: u32,
    root: Option<[u8; blake3::OUT_LEN]>,
    /// Leading content bytes already checked against `root`: zero, a whole number of chunk groups, or all of it.
    verified_len: usize,
}

impl LogObjectWriter {
    /// Starts an empty log object.
    pub fn new() -> Self {
        let mut bytes = Writer::new();
        bytes.put_slice(&header());
        Self { bytes, record_count: 0 }
    }

    /// Adds one record to the end of the object. Refuses, leaving the object unchanged, a payload of `u32::MAX` bytes
    /// or more, or a record beyond the `u32::MAX`th.
    pub fn append(&mut self, payload: &[u8]) -> Result<(), FormatError> {
        let len = u32::try_from(payload.len())
            .ok()
            .filter(|len| *len != LOG_TRAILER_SENTINEL)
            .ok_or(FormatError::Structural {
                rule: "a log record payload must be shorter than u32::MAX bytes",
            })?;
        let record_count = self.record_count.checked_add(1).ok_or(FormatError::Structural {
            rule: "a log object holds at most u32::MAX records",
        })?;
        let start = self.bytes.len();
        self.bytes.put_u32(len);
        self.bytes.put_slice(payload);
        let crc = crc64_nvme(self.bytes.bytes().get(start..).unwrap_or_default());
        self.bytes.put_u64(crc);
        self.record_count = record_count;
        Ok(())
    }

    /// Seals the object with its trailer and returns the bytes to upload together with the root to pin.
    pub fn finish(mut self) -> SealedLogObject {
        self.bytes.put_u32(LOG_TRAILER_SENTINEL);
        self.bytes.put_u32(self.record_count);
        self.bytes.put_slice(&LOG_MAGIC);
        let content = self.bytes.into_bytes();
        let root = *hash_tree(&content).as_bytes();
        SealedLogObject {
            bytes: attach_outboard_tree(content, CHUNK_GROUP_BYTES),
            root,
        }
    }
}

impl Default for LogObjectWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> LogRecords<'a> {
    /// Opens a log object checked only by each record's CRC, for an object whose root was never pinned, such as the
    /// newest one found after a crash. Fails with [`LogReadError::TornTail`] when even the header is incomplete, and
    /// with [`LogReadError::Corrupt`] when the bytes are not a log object (a HEF file among them).
    pub fn open(object: &'a [u8]) -> Result<Self, LogReadError> {
        Self::start(object, None)
    }

    /// Opens a log object whose root the caller pinned when it was committed. Every byte is checked against `root`
    /// before it is yielded, one 1 MiB chunk group at a time, so a mismatch anywhere fails as
    /// [`LogReadError::Corrupt`].
    pub fn open_verified(object: &'a [u8], root: &[u8; blake3::OUT_LEN]) -> Result<Self, LogReadError> {
        Self::start(object, Some(*root))
    }

    /// How many leading bytes of the object have been checked against the pinned root so far; always zero when opened
    /// without one. Reading forward extends it one 1 MiB chunk group at a time, so the first record arrives after at
    /// most one chunk group has been hashed and reading every record hashes each byte once.
    pub fn verified_len(&self) -> u64 {
        self.verified_len as u64
    }

    fn start(object: &'a [u8], root: Option<[u8; blake3::OUT_LEN]>) -> Result<Self, LogReadError> {
        let content = parse_outboard_object(object, &LOG_MAGIC).map_or(object, |parsed| parsed.content);
        let mut records = Self {
            content,
            finished: false,
            object,
            position: LOG_HEADER_LEN,
            record_count: 0,
            root,
            verified_len: 0,
        };
        records.verify_through(LOG_HEADER_LEN)?;
        if content.len() < LOG_HEADER_LEN && header().starts_with(content) {
            return Err(LogReadError::TornTail {
                intact_len: 0,
                intact_records: 0,
            });
        }
        let mut reader = Reader::new(content);
        if reader.take(LOG_MAGIC.len(), "log magic").map_err(FormatError::from)? != LOG_MAGIC {
            return Err(FormatError::BadMagic { expected: "HEFL" }.into());
        }
        let version = reader.u8("log format version").map_err(FormatError::from)?;
        if version != LOG_FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                field: "log format version",
                found: u32::from(version),
            }
            .into());
        }
        if Profile::from_u8(reader.u8("log profile").map_err(FormatError::from)?)? != Profile::Log {
            return Err(FormatError::Structural {
                rule: "a log object must declare the log profile",
            }
            .into());
        }
        Ok(records)
    }

    /// Reads the frame at the current position: a record, the trailer (`None`), or why neither could be read.
    fn next_record(&mut self) -> Result<Option<&'a [u8]>, LogReadError> {
        let rest = self.content.get(self.position..).unwrap_or_default();
        let mut reader = Reader::new(rest);
        let Ok(len) = reader.u32("log record length") else {
            return Err(self.torn());
        };
        if len == LOG_TRAILER_SENTINEL {
            let (Ok(count), Ok(magic)) = (
                reader.u32("log record count"),
                reader.take(LOG_MAGIC.len(), "log trailer magic"),
            ) else {
                return Err(self.torn());
            };
            self.verify_through(self.position.saturating_add(LOG_TRAILER_LEN))?;
            if magic != LOG_MAGIC || reader.remaining() != 0 {
                return Err(FormatError::Structural {
                    rule: "a log trailer must end the object",
                }
                .into());
            }
            if count != self.record_count {
                return Err(FormatError::Structural {
                    rule: "a log trailer's record count must match the records read",
                }
                .into());
            }
            return Ok(None);
        }
        let (Ok(payload), Ok(crc)) = (
            reader.take(len as usize, "log record payload"),
            reader.u64("log record crc"),
        ) else {
            return Err(self.torn());
        };
        let frame_len = LOG_RECORD_FRAME_LEN.saturating_add(payload.len());
        self.verify_through(self.position.saturating_add(frame_len))?;
        let covered = rest
            .get(..frame_len.saturating_sub(size_of::<u64>()))
            .unwrap_or_default();
        if !crc64_matches(covered, crc) {
            return Err(FormatError::Structural {
                rule: "a log record's CRC-64/NVME must match its length and payload",
            }
            .into());
        }
        self.position = self.position.saturating_add(frame_len);
        self.record_count = self.record_count.saturating_add(1);
        Ok(Some(payload))
    }

    /// The error for bytes that stop short of the trailer: a torn tail, unless the pinned root shows the bytes are
    /// damaged instead.
    fn torn(&mut self) -> LogReadError {
        if let Err(error) = self.verify_through(self.content.len()) {
            return error.into();
        }
        LogReadError::TornTail {
            intact_len: self.position as u64,
            intact_records: u64::from(self.record_count),
        }
    }

    /// Checks the content against the pinned root through at least `end` bytes, extending the checked prefix by whole
    /// chunk groups so no byte is hashed twice. Does nothing when opened without a root.
    fn verify_through(&mut self, end: usize) -> Result<(), FormatError> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let end = end.min(self.content.len());
        if self.verified_len > 0 && self.verified_len >= end {
            return Ok(());
        }
        let window_end = end
            .div_ceil(CHUNK_GROUP_BYTES)
            .saturating_mul(CHUNK_GROUP_BYTES)
            .min(self.content.len());
        verify_object_range(
            self.object,
            root,
            self.verified_len as u64,
            window_end.saturating_sub(self.verified_len) as u64,
            CHUNK_GROUP_BYTES,
            &LOG_MAGIC,
        )?;
        self.verified_len = window_end;
        Ok(())
    }
}

impl<'a> Iterator for LogRecords<'a> {
    type Item = Result<&'a [u8], LogReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let next = self.next_record().transpose();
        self.finished = !matches!(next, Some(Ok(_)));
        next
    }
}

/// The six header bytes every log object starts with, built on the stack.
fn header() -> [u8; LOG_HEADER_LEN] {
    let [m0, m1, m2, m3] = LOG_MAGIC;
    [m0, m1, m2, m3, LOG_FORMAT_VERSION, Profile::Log.code()]
}

#[cfg(test)]
#[path = "test/log.rs"]
mod tests;
