//! How one stored column file is laid out on disk: a fixed header up front, the data in the middle, and a
//! self-describing footer at the tail.
//!
//! The data is organised into stripes, then granules, then pages — the units that pruning and random access work on.
//! The header is a small fixed, aligned block for fast rejection; the footer (in the `footer` submodule) holds the
//! authoritative directories and the feature flags that govern what a reader is allowed to do with the file.

use crate::error::FormatError;
use crate::events::TenantId;
use crate::file::bytes::{Reader, Writer, slice};
use constant::HEF_MAGIC;
use std::borrow::Cow;
use uuid::Uuid;

pub mod clustering;
pub mod constant;
pub mod entity_scan;
pub mod event;
pub mod footer;
pub mod mirror;
pub mod reader;

/// The fixed file header occupies the first 4 KiB block.
pub const HEADER_BLOCK_LEN: usize = 4096;

/// Maximum page/chunk size (compressed). A reader rejects a file whose page exceeds this unless a recognized declared
/// required feature lifts it.
pub const MAX_PAGE_BYTES: u64 = 1024 * 1024;

/// Decode-bomb guard on an elided constant block's declared row count: matches the block encoder's own value-count
/// ceiling, so a forged zero-length mark cannot make the reader materialize an unbounded constant vector.
pub const MAX_ELIDED_BLOCK_ROWS: usize = 1 << 24;

/// The encoded forms a block's presence side stream may take, recorded as the frame's leading tag byte under the
/// `compressed_presence` required feature. Chosen deterministically per block: the zero-byte forms when they apply,
/// otherwise whichever of raw, set-runs, or set-positions stores smallest.
pub mod presence_forms {
    /// A block where every row is absent: zero stored bytes; the reader rebuilds an all-zero bitmap over the row
    /// count.
    pub const ALL_ABSENT: u8 = 3;
    /// A presence-gated block where every row is present: zero stored bytes; the reader rebuilds an all-set bitmap
    /// over the row count.
    pub const ALL_PRESENT: u8 = 2;
    /// A plain column with no presence stream at all — dense by construction, distinct from [`ALL_PRESENT`] because
    /// downstream rank/select never runs on it.
    pub const EMPTY: u8 = 1;
    /// The one-bit-per-row bitmap, kept as the fallback and the deterministic reference: `u32` byte length, then the
    /// bytes.
    pub const RAW: u8 = 0;
    /// Sparse present rows as individual positions: a `u32` count, then one `u32` per set bit.
    pub const SET_POSITIONS: u8 = 5;
    /// Clustered present rows as `(start, len)` runs of set bits: a `u32` run count, then two `u32`s per run.
    pub const SET_RUNS: u8 = 4;
}

/// Encodes a block's presence bitmap as its smallest deterministic side-stream form (see [`presence_forms`]).
/// `presence` uses the in-memory convention: empty means a plain dense column with no presence stream.
pub fn encode_presence(presence: &[u8], row_count: u32, out: &mut Writer) {
    if presence.is_empty() {
        out.put_u8(presence_forms::EMPTY);
        return;
    }
    let rows = row_count as usize;
    let set: Vec<usize> = (0..rows)
        .filter(|row| presence.get(row / 8).is_some_and(|byte| byte & (1 << (row % 8)) != 0))
        .collect();
    if set.len() == rows {
        out.put_u8(presence_forms::ALL_PRESENT);
        return;
    }
    if set.is_empty() {
        out.put_u8(presence_forms::ALL_ABSENT);
        return;
    }
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &row in &set {
        match runs.last_mut() {
            Some((start, len)) if *start as usize + *len as usize == row => *len += 1,
            _ => runs.push((row as u32, 1)),
        }
    }
    let raw_size = 4 + presence.len();
    let runs_size = 4 + runs.len() * 8;
    let positions_size = 4 + set.len() * 4;
    if raw_size <= runs_size && raw_size <= positions_size {
        out.put_u8(presence_forms::RAW);
        out.put_u32(presence.len() as u32);
        out.put_slice(presence);
    } else if runs_size <= positions_size {
        out.put_u8(presence_forms::SET_RUNS);
        out.put_u32(runs.len() as u32);
        for (start, len) in runs {
            out.put_u32(start);
            out.put_u32(len);
        }
    } else {
        out.put_u8(presence_forms::SET_POSITIONS);
        out.put_u32(set.len() as u32);
        for row in set {
            out.put_u32(row as u32);
        }
    }
}

/// Decodes a block's presence stream under either framing: the tagged side-stream forms when the file declares the
/// `compressed_presence` required feature, or the legacy `u32 length | bitmap` frame written before that feature
/// existed — so files written by older writers stay readable.
///
/// Borrows the file's own bytes whenever the block already stores the bitmap verbatim (the legacy frame, and the
/// tagged raw form), so a caller that only reads the bitmap pays no copy; the forms that store runs, positions, or
/// nothing at all are rebuilt into an owned bitmap as before.
pub fn decode_presence_frame<'a>(
    reader: &mut Reader<'a>,
    row_count: u32,
    compressed_presence: bool,
) -> Result<Cow<'a, [u8]>, FormatError> {
    if compressed_presence {
        decode_presence(reader, row_count)
    } else {
        let len = reader.u32("presence length")? as usize;
        Ok(Cow::Borrowed(reader.take(len, "presence bitmap")?))
    }
}

/// Decodes a block's presence stream straight into its compressed run form — the substrate rank/select builds on —
/// under either framing, exactly as [`decode_presence_frame`] does for the bitmap convention. `None` means a plain
/// dense column with no presence stream at all. The raw-bitmap form (tagged or legacy) is converted by the byte-run
/// scan; a per-row bitmap is never materialized here. Every answer equals what expanding the stream to a bitmap and
/// collecting its set rows would produce — the equivalence the rank_select tests pin.
pub fn decode_presence_ranges(
    reader: &mut Reader<'_>,
    row_count: u32,
    compressed_presence: bool,
) -> Result<Option<crate::indexes::bitmap::RoaringRangeBitmap>, FormatError> {
    use crate::indexes::bitmap::{RoaringRangeBitmap, RowRange};
    if !compressed_presence {
        let len = reader.u32("presence length")? as usize;
        let bitmap = reader.take(len, "presence bitmap")?;
        if bitmap.is_empty() {
            return Ok(None);
        }
        return Ok(Some(RoaringRangeBitmap::from_packed_bits(bitmap)));
    }
    let form = reader.u8("presence form")?;
    let rows = u64::from(row_count);
    Ok(match form {
        presence_forms::EMPTY => None,
        presence_forms::ALL_PRESENT => Some(RoaringRangeBitmap::from_ranges([RowRange { start: 0, end: rows }])),
        presence_forms::ALL_ABSENT => Some(RoaringRangeBitmap::default()),
        presence_forms::RAW => {
            let len = reader.u32("presence length")? as usize;
            if len != (rows as usize).div_ceil(8) {
                return Err(FormatError::Structural {
                    rule: "raw presence length must cover exactly the block's row count",
                });
            }
            Some(RoaringRangeBitmap::from_packed_bits(
                reader.take(len, "presence bitmap")?,
            ))
        }
        presence_forms::SET_RUNS => {
            let count = reader.u32("presence run count")? as usize;
            if count > rows as usize {
                return Err(FormatError::Structural {
                    rule: "presence run count beyond the block's row count",
                });
            }
            let mut ranges = Vec::with_capacity(count);
            for _ in 0..count {
                let start = u64::from(reader.u32("presence run start")?);
                let len = u64::from(reader.u32("presence run length")?);
                let end = start.saturating_add(len);
                if end > rows {
                    return Err(FormatError::RefOutOfRange {
                        what: "presence row beyond the block's row count",
                    });
                }
                ranges.push(RowRange { start, end });
            }
            Some(RoaringRangeBitmap::from_ranges(ranges))
        }
        presence_forms::SET_POSITIONS => {
            let count = reader.u32("presence position count")? as usize;
            if count > rows as usize {
                return Err(FormatError::Structural {
                    rule: "presence position count beyond the block's row count",
                });
            }
            let mut positions = Vec::with_capacity(count);
            for _ in 0..count {
                let row = u64::from(reader.u32("presence position")?);
                if row >= rows {
                    return Err(FormatError::RefOutOfRange {
                        what: "presence row beyond the block's row count",
                    });
                }
                positions.push(row);
            }
            Some(RoaringRangeBitmap::from_rows(positions))
        }
        _ => {
            return Err(FormatError::Structural {
                rule: "unknown presence form",
            });
        }
    })
}

/// Decodes a block's presence side stream back into the in-memory bitmap convention: an empty bitmap for a plain
/// dense column, otherwise one bit per row. Counts and positions are bounded by `row_count`, so forged bytes cannot
/// amplify allocation. The raw form is handed back borrowed from the block's own bytes — it is already the bitmap —
/// while the run, position, and all-set/all-absent forms are rebuilt into an owned one. The inverse of
/// [`encode_presence`].
pub fn decode_presence<'a>(reader: &mut Reader<'a>, row_count: u32) -> Result<Cow<'a, [u8]>, FormatError> {
    let form = reader.u8("presence form")?;
    let rows = row_count as usize;
    match form {
        // EMPTY and RAW hand back a borrow (nothing, or the block's own bytes), so neither needs the owned bitmap the
        // other forms rebuild — allocating one here just to leave it unused would cost a calloc/free on every block
        // this decode's hottest forms cover.
        presence_forms::EMPTY => Ok(Cow::Borrowed(&[])),
        presence_forms::ALL_PRESENT => {
            // Written whole rather than a bit at a time: this runs once per raw read on the point-probe path, where
            // a per-row loop over a full granule costs more than the bitmap it produces. The last byte keeps only
            // the bits the row count reaches, exactly as setting rows `0..rows` one by one would leave it.
            let mut bitmap = vec![0xFFu8; rows.div_ceil(8)];
            if rows % 8 != 0
                && let Some(last) = bitmap.last_mut()
            {
                *last = (1u8 << (rows % 8)) - 1;
            }
            Ok(Cow::Owned(bitmap))
        }
        presence_forms::ALL_ABSENT => Ok(Cow::Owned(vec![0u8; rows.div_ceil(8)])),
        presence_forms::RAW => {
            let len = reader.u32("presence length")? as usize;
            if len != rows.div_ceil(8) {
                return Err(FormatError::Structural {
                    rule: "raw presence length must cover exactly the block's row count",
                });
            }
            Ok(Cow::Borrowed(reader.take(len, "presence bitmap")?))
        }
        presence_forms::SET_RUNS => {
            let count = reader.u32("presence run count")? as usize;
            if count > rows {
                return Err(FormatError::Structural {
                    rule: "presence run count beyond the block's row count",
                });
            }
            let mut bitmap = vec![0u8; rows.div_ceil(8)];
            for _ in 0..count {
                let start = reader.u32("presence run start")? as usize;
                let len = reader.u32("presence run length")? as usize;
                let end = start.saturating_add(len);
                if end > rows {
                    return Err(FormatError::RefOutOfRange {
                        what: "presence row beyond the block's row count",
                    });
                }
                // Fills whole interior bytes in one write and masks only the two partial edge bytes, rather than
                // setting each row's bit one at a time.
                fill_bit_range(&mut bitmap, start, end);
            }
            Ok(Cow::Owned(bitmap))
        }
        presence_forms::SET_POSITIONS => {
            let count = reader.u32("presence position count")? as usize;
            if count > rows {
                return Err(FormatError::Structural {
                    rule: "presence position count beyond the block's row count",
                });
            }
            let mut bitmap = vec![0u8; rows.div_ceil(8)];
            for _ in 0..count {
                let row = reader.u32("presence position")? as usize;
                if row >= rows {
                    return Err(FormatError::RefOutOfRange {
                        what: "presence row beyond the block's row count",
                    });
                }
                bitmap[row / 8] |= 1 << (row % 8);
            }
            Ok(Cow::Owned(bitmap))
        }
        _ => Err(FormatError::Structural {
            rule: "unknown presence form",
        }),
    }
}

/// Sets bits `[start, end)` of a packed LSB-0 bitmap: whole interior bytes are written in one store, and only the
/// (at most two) partial bytes at the edges are built with a mask, instead of setting each bit in the range one at a
/// time.
fn fill_bit_range(bitmap: &mut [u8], start: usize, end: usize) {
    if start >= end {
        return;
    }
    let start_byte = start / 8;
    let end_byte = (end - 1) / 8;
    let low_mask = 0xFFu8 << (start % 8);
    if start_byte == end_byte {
        let high_mask = 0xFFu8 >> (7 - (end - 1) % 8);
        bitmap[start_byte] |= low_mask & high_mask;
        return;
    }
    bitmap[start_byte] |= low_mask;
    for byte in &mut bitmap[start_byte + 1..end_byte] {
        *byte = 0xFF;
    }
    bitmap[end_byte] |= 0xFFu8 >> (7 - (end - 1) % 8);
}

/// The offset a per-row `(offset, len)` slot stores for a value that is present but empty.
///
/// A row's slot is `(0, 0)` when it carries no value at all, so a stored empty string — length zero, like an absent one
/// — needs something else to say "present". It is written at this offset instead, which no real value can occupy: the
/// arenas these slots index are addressed by `u32`, so an offset of `u32::MAX` is past the end of any arena a writer
/// can produce. Reading such a slot yields the empty value, never `None`.
pub const EMPTY_VALUE_ROW_OFFSET: u32 = u32::MAX;

/// The size targets that shape how rows are grouped into pages, granules, and stripes. Defaults are the spec values;
/// tests shrink them to exercise stripe/granule formation without gigabytes of data.
#[derive(Debug, Clone, Copy)]
pub struct LayoutTargets {
    /// Granule row target.
    pub index_granularity: usize,
    /// Granule compressed-byte target.
    pub index_granularity_bytes: usize,
    /// Hard stripe clamp: a writer must close the file before any stripe exceeds this (pure safety backstop, not the
    /// roll trigger).
    pub max_stripe_bytes: usize,
    /// Compact/wide crossover.
    pub min_bytes_for_wide: usize,
    /// Stripe target (uncompressed estimate).
    pub stripe_target_bytes: usize,
}

impl Default for LayoutTargets {
    fn default() -> Self {
        Self {
            index_granularity: 8192,
            index_granularity_bytes: 10 * 1024 * 1024,
            stripe_target_bytes: 256 * 1024 * 1024,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        }
    }
}

/// The two physical layout classes within the one HEF format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutClass {
    /// Interleaved sections with a single index directory: avoids the per-column overhead of many small files.
    Compact = 0,
    /// One section per column: direct range reads for analytical scans.
    Wide = 1,
}

impl LayoutClass {
    /// Reads the layout class from its stored byte code, or an error for an unknown value.
    pub fn from_u8(value: u8) -> Result<Self, FormatError> {
        match value {
            0 => Ok(LayoutClass::Compact),
            1 => Ok(LayoutClass::Wide),
            _ => Err(FormatError::Structural {
                rule: "unknown layout class",
            }),
        }
    }
}

/// Required feature flags: every HEF file must declare all of these, and a reader that does not understand a declared
/// required feature refuses.
pub mod required_features {
    pub const ENVELOPE_COLUMNS: u64 = 1 << 0;
    pub const PAYLOAD_ARENA: u64 = 1 << 1;
    pub const STRIPE_DIRECTORY: u64 = 1 << 2;
    pub const GRANULE_DIRECTORY: u64 = 1 << 3;
    pub const MARKS_PER_COLUMN: u64 = 1 << 4;
    pub const PAGE_METADATA: u64 = 1 << 5;
    pub const MINMAX_SKIP_INDEXES: u64 = 1 << 6;
    pub const SEQUENCE_SKIP_INDEX: u64 = 1 << 7;
    pub const TIME_SKIP_INDEX: u64 = 1 << 8;
    pub const EXACT_FILE_COUNTS: u64 = 1 << 9;
    pub const EXACT_SOURCE_TYPE_ENTITY_COUNTS: u64 = 1 << 10;
    pub const CHECKSUM_DIRECTORY: u64 = 1 << 11;
    pub const FOOTER_DIRECTORY: u64 = 1 << 12;
    pub const LAYOUT_CLASS: u64 = 1 << 13;
    /// Every intra-stripe mark and page offset is measured from the base offset of its own stripe
    /// (`StripeEntry.file_offset`), not from the start of the file. A reader adds the stripe base back before slicing.
    /// Required and refusing on purpose: a reader that did not understand the flag would read a stripe-relative
    /// offset as file-absolute and return the wrong bytes, so it must reject the file instead.
    pub const STRIPE_RELATIVE_MARKS: u64 = 1 << 14;
    /// Marks are stored columnar per `(projection, column)` as FastLanes/DELTA-encoded parallel arrays, addressed
    /// through a two-level, per-stripe marks directory instead of one row-oriented struct per mark. Required and
    /// refusing on purpose: a reader that did not understand the flag would misread the columnar arrays as the
    /// row-oriented form and return the wrong bytes, so it must reject the file instead. A writer MAY dual-emit the
    /// row-oriented form alongside it during migration.
    pub const COLUMNAR_MARKS: u64 = 1 << 15;

    /// The file's footer — column directory, dictionaries, and layout metadata — is sealed under the caller-supplied
    /// file DEK with the pinned AEAD, so the schema is opaque to a reader without the key. It rides the fixed header's
    /// `feature_flags`, where a reader gates required features before the footer is decoded, precisely because the
    /// footer bytes are ciphertext when it is set: a reader that does not understand this bit refuses rather than
    /// decoding ciphertext as a plaintext footer.
    pub const FOOTER_ENCRYPTED: u64 = 1 << 16;

    /// A dense single-page integer block whose exact stats prove one value for every row may store no bytes at all:
    /// its mark carries a zero-length extent and readers rebuild the block from the stats. Required and refusing on
    /// purpose: a reader that did not understand the flag would treat the zero-length extent as a truncated or empty
    /// block and fail — or worse, decode garbage — so it must reject the file instead.
    pub const ELIDED_CONSTANT_BLOCKS: u64 = 1 << 17;

    /// A block's presence bitmap is an encoded side stream (see [`super::presence_forms`]) instead of a fixed raw
    /// `length | bitmap` prefix, so an all-present block stores zero presence bytes. Required and refusing on
    /// purpose: a reader that did not understand the flag would parse the form tag as a length byte and misframe
    /// every block, so it must reject the file instead.
    pub const COMPRESSED_PRESENCE: u64 = 1 << 18;

    /// Hot-but-sparse payload paths may store as sparse shredded columns (presence bitmap + dense value block),
    /// declared in the footer's sparse key set — and their values are removed from the residual arena. Required and
    /// refusing on purpose: a reader that did not understand the flag would read the residual as if those paths still
    /// lived there and silently return missing values, so it must reject the file instead.
    pub const SPARSE_SHREDDED_COLUMNS: u64 = 1 << 19;

    /// A dictionary block may store only its code stream, resolving the alphabet from the footer's file-scope shared
    /// dictionary for the column. Required and refusing on purpose: a reader that cannot resolve the shared alphabet
    /// cannot decode a shared-scope block at all, so it must reject the file instead. The external (cross-file) scope
    /// stays reserved and undefined.
    pub const SHARED_DICTIONARIES: u64 = 1 << 20;

    /// Each stripe's columnar marks pages live in the data area immediately beside that stripe's filter bytes —
    /// one contiguous extent a single ranged IO fetches — with the footer's marks section carrying only the
    /// per-stripe directory (stripe-relative offsets). Required and refusing on purpose: a reader that did not
    /// understand the placement would look for the pages in the footer's (empty) pages area and could resolve no
    /// mark at all, so it must reject the file instead. Files from before the placement keep their pages in the
    /// footer and decode as before.
    pub const STRIPE_MARKS_PAGES: u64 = 1 << 21;

    /// Each stripe checksum is also the root of a Bao-style outboard proof tree whose authenticated geometry lives in
    /// the footer. A range reader that understands this bit may return bytes only after verifying their chunk-group
    /// proof against that stripe root; a reader that does not understand the proof placement must refuse range-native
    /// reads rather than silently treating unverified bytes as trusted.
    pub const STRIPE_VERIFIED_STREAMING: u64 = 1 << 22;

    /// Every required feature this reader implements.
    pub const KNOWN: u64 = (1 << 23) - 1;

    /// The full required set every v1 file declares, `COLUMNAR_MARKS` included — the writer emits the columnar,
    /// per-stripe marks form. Excludes only `FOOTER_ENCRYPTED` (opt-in per build, set only when the caller requests
    /// footer encryption); declaring that bit before it applies would misrepresent the file.
    pub const ALL: u64 = KNOWN & !FOOTER_ENCRYPTED;
}

/// Optional feature flags: unknown optional features are ignored; known ones are validated and used.
pub mod optional_features {
    pub const TRAINED_ZSTD_RESIDUAL_DICTIONARIES: u64 = 1 << 0;
    pub const VARIANT_SHREDDED_FIELD_BLOCKS: u64 = 1 << 1;
    pub const FREETEXT_COLUMNS: u64 = 1 << 2;
    pub const HEF_NATIVE_DELETION_VECTORS: u64 = 1 << 4;
    /// The file carries at least one late event — a row whose `occurred_at` is older than the `occurred_at` of a row
    /// ingested before it (an inversion against `(epoch, sequence)` order). A planner copies this into the file's
    /// manifest summary so it can tell, without opening the file, that the file's `occurred_at` coverage is not implied
    /// by its ingest order. Informational: a reader that ignores the bit still reads every row correctly.
    pub const HEF_LATE_EVENTS: u64 = 1 << 8;
    /// Every column block starts at a multiple of the IO granularity recorded in the footer's `io_alignment_bytes`, so
    /// a reader can fetch one page with an aligned direct read. Informational: the alignment is applied whether or not
    /// this bit is set, but declaring it lets a reader see the alignment is in effect without inspecting offsets.
    pub const PAGE_IO_ALIGNMENT: u64 = 1 << 6;
    /// Per-page byte-range directory inside each column mark, enabling single-page reads without reading the rest of
    /// the granule block.
    pub const PER_PAGE_MARKS: u64 = 1 << 5;
    pub const PROJECTIONS: u64 = 1 << 3;
    /// Per-page text-token filter bytes live in the data area, with only their byte ranges recorded in the footer, so
    /// a cold open never fetches them. A reader that declares this feature resolves a filter lazily by its recorded
    /// range; one that does not simply never prunes by token filter, which is always safe.
    pub const TEXT_TOKEN_FILTER_OFFSETS: u64 = 1 << 9;
    /// A per-row `(offset, len)` byte-offset index for wide typed columns (declared free-text fields and internal
    /// embedding/vector columns), in the same shape as the residual variant arena's per-row slot: a reader that
    /// declares this feature resolves one row's byte range by direct indexed lookup instead of decoding the whole
    /// granule column block. Droppable acceleration state — a reader that does not declare it falls back to the
    /// whole-granule decode and gets byte-identical values.
    pub const TYPED_COLUMN_ROW_OFFSETS: u64 = 1 << 7;

    /// One point-membership filter per granule over the identity-hash column, carried in the footer, so a lookup for
    /// a single entity id reads only the granules whose filter admits its hash. Droppable acceleration state: a
    /// reader that does not declare the feature scans every granule and returns the identical answer, and a filter
    /// never rules out a granule that holds the id, so a confirmed match is never lost.
    pub const ENTITY_HASH_POINT_FILTERS: u64 = 1 << 10;

    /// Optional features this reader knows how to validate and use.
    pub const KNOWN: u64 = (1 << 11) - 1;
}

/// The decoded fixed file header. A coarse rejection accelerator, not the authoritative aggregate source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HefHeader {
    pub created_at_physical: i64,
    pub feature_flags: u64,
    pub file_id: u128,
    pub footer_pointer_hint: u64,
    pub generation_id: u64,
    pub layout_class: LayoutClass,
    pub max_epoch: u64,
    pub max_ingested_at_physical: i64,
    pub max_occurred_at_physical: i64,
    pub max_sequence: u64,
    pub min_epoch: u64,
    pub min_ingested_at_physical: i64,
    pub min_occurred_at_physical: i64,
    pub min_sequence: u64,
    pub projection_count: u32,
    pub row_count: u64,
    /// Internal only; never public output.
    pub tenant_id: TenantId,
    pub version_major: u16,
    pub version_minor: u16,
}

// magic + version pair + header_len + file_id + tenant_id + generation + layout_class + projection_count + (created_at,
// 2× occurred, 2× ingested, 2× epoch, 2× sequence, row_count, feature_flags, footer_pointer_hint = 12 u64s) + crc64 +
// blake3.
const HEADER_FIXED_LEN: usize = 4 + 2 + 2 + 4 + 16 + 16 + 8 + 1 + 4 + 8 * 12 + 8 + 32;

impl HefHeader {
    /// Coarse header-level rejection: true when the file's sequence coverage in `epoch` may intersect the queried
    /// `first..=last` range. False rejects the file before its footer is read.
    ///
    /// Sequences reset per epoch, and a file may span epochs, so the flat `[min_sequence, max_sequence]` interval is
    /// only meaningful at the boundary epochs. In the first covered epoch the file starts at `min_sequence`; in the
    /// last it ends at `max_sequence`; any epoch strictly between them is covered in full, so the file may hold any
    /// sequence there.
    pub fn may_contain_sequence(&self, epoch: u64, first: u64, last: u64) -> bool {
        if epoch < self.min_epoch || epoch > self.max_epoch {
            return false;
        }
        let lower_ok = epoch != self.min_epoch || self.min_sequence <= last;
        let upper_ok = epoch != self.max_epoch || first <= self.max_sequence;
        lower_ok && upper_ok
    }

    /// Coarse header-level rejection by occurred-at range.
    pub fn may_contain_occurred(&self, min_nanos: i64, max_nanos: i64) -> bool {
        self.min_occurred_at_physical <= max_nanos && min_nanos <= self.max_occurred_at_physical
    }
}

/// Encodes the header into its fixed aligned 4 KiB block with CRC-64/NVME and BLAKE3 over the header bytes.
pub fn encode_header(header: &HefHeader) -> Vec<u8> {
    let mut out = Writer::with_capacity(HEADER_BLOCK_LEN);
    out.put_slice(&HEF_MAGIC);
    out.put_u16(header.version_major);
    out.put_u16(header.version_minor);
    out.put_u32(HEADER_FIXED_LEN as u32);
    out.put_u128(header.file_id);
    out.put_u128(header.tenant_id.uuid().as_u128());
    out.put_u64(header.generation_id);
    out.put_u8(header.layout_class as u8);
    out.put_u32(header.projection_count);
    out.put_i64(header.created_at_physical);
    out.put_i64(header.min_occurred_at_physical);
    out.put_i64(header.max_occurred_at_physical);
    out.put_i64(header.min_ingested_at_physical);
    out.put_i64(header.max_ingested_at_physical);
    out.put_u64(header.min_epoch);
    out.put_u64(header.max_epoch);
    out.put_u64(header.min_sequence);
    out.put_u64(header.max_sequence);
    out.put_u64(header.row_count);
    out.put_u64(header.feature_flags);
    out.put_u64(header.footer_pointer_hint);
    let crc = crate::file::integrity::crc64_nvme(out.bytes());
    out.put_u64(crc);
    let digest = blake3::hash(out.bytes());
    out.put_slice(digest.as_bytes());
    let mut block = out.into_bytes();
    block.resize(HEADER_BLOCK_LEN, 0);
    block
}

/// Decodes and validates the fixed header block.
pub fn decode_header(block: &[u8]) -> Result<HefHeader, FormatError> {
    let fixed = slice(block, 0, HEADER_FIXED_LEN, "hef header")?;
    let mut reader = Reader::new(fixed);
    let magic = reader.take(HEF_MAGIC.len(), "hef magic")?;
    if magic != HEF_MAGIC {
        return Err(FormatError::BadMagic { expected: "HEF1" });
    }
    let version_major = reader.u16("version_major")?;
    let version_minor = reader.u16("version_minor")?;
    if version_major != 1 {
        return Err(FormatError::UnsupportedVersion {
            field: "HEFHeader.version_major",
            found: u32::from(version_major),
        });
    }
    let header_len = reader.u32("header_len")?;
    if header_len as usize != HEADER_FIXED_LEN {
        return Err(FormatError::Structural {
            rule: "hef header_len mismatch",
        });
    }
    let file_id = reader.u128("file_id")?;
    let tenant_id = TenantId::from_uuid(Uuid::from_u128(reader.u128("tenant_id")?));
    let generation_id = reader.u64("generation_id")?;
    let layout_class = LayoutClass::from_u8(reader.u8("layout_class")?)?;
    let projection_count = reader.u32("projection_count")?;
    let created_at_physical = reader.i64("created_at")?;
    let min_occurred_at_physical = reader.i64("min_occurred_at")?;
    let max_occurred_at_physical = reader.i64("max_occurred_at")?;
    let min_ingested_at_physical = reader.i64("min_ingested_at")?;
    let max_ingested_at_physical = reader.i64("max_ingested_at")?;
    let min_epoch = reader.u64("min_epoch")?;
    let max_epoch = reader.u64("max_epoch")?;
    let min_sequence = reader.u64("min_sequence")?;
    let max_sequence = reader.u64("max_sequence")?;
    let row_count = reader.u64("row_count")?;
    let feature_flags = reader.u64("feature_flags")?;
    let footer_pointer_hint = reader.u64("footer_pointer_hint")?;
    let stored_crc = reader.u64("header_crc64")?;
    let stored_blake3 = reader.take(32, "header_blake3")?;

    let crc_scope = slice(block, 0, HEADER_FIXED_LEN - 8 - 32, "hef header crc scope")?;
    if crate::file::integrity::crc64_nvme(crc_scope) != stored_crc {
        return Err(FormatError::HeaderCrcMismatch);
    }
    let blake3_scope = slice(block, 0, HEADER_FIXED_LEN - 32, "hef header blake3 scope")?;
    if blake3::hash(blake3_scope).as_bytes() != stored_blake3 {
        return Err(FormatError::Blake3Mismatch { scope: "hef header" });
    }
    // Padding to the aligned block must be zero.
    let padding = slice(
        block,
        HEADER_FIXED_LEN,
        HEADER_BLOCK_LEN.saturating_sub(HEADER_FIXED_LEN),
        "hef header padding",
    )?;
    if padding.iter().any(|byte| *byte != 0) {
        return Err(FormatError::ReservedNotZero {
            field: "hef header padding",
        });
    }
    Ok(HefHeader {
        version_major,
        version_minor,
        file_id,
        tenant_id,
        generation_id,
        layout_class,
        projection_count,
        created_at_physical,
        min_occurred_at_physical,
        max_occurred_at_physical,
        min_ingested_at_physical,
        max_ingested_at_physical,
        min_epoch,
        max_epoch,
        min_sequence,
        max_sequence,
        row_count,
        feature_flags,
        footer_pointer_hint,
    })
}

#[cfg(test)]
#[path = "test/mod.rs"]
mod tests;
