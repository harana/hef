//! Reads many columns over many granules in one call, decoding their blocks side by side on the shared thread pool
//! and handing the results back one granule at a time, in order, under a fixed memory bound.
//!
//! A scan of every column of every granule has hundreds of independent block decodes. One after another they leave
//! all but one core idle; all at once they hold every decoded block in memory before the caller has seen the first.
//! The scan here walks the granules in the caller's order, gathers consecutive ones into a window of at most
//! [`SCAN_WINDOW_BYTES`] estimated decode bytes, decodes that window's blocks in parallel when there is enough work
//! in it to pay for the hand-off, and delivers the window's batches before starting the next. Marks are resolved on
//! the calling thread before a window fans out, so workers only slice and decode; a lazily opened file hashes each
//! stripe once, on whichever worker touches it first, and the rest wait on that one hash rather than repeating it.
//!
//! See: hef-apis/spec.md

use super::*;
use crate::encoding::predicate::scan_in_parallel;
use crate::layout::constant::SCAN_WINDOW_BYTES;
use crate::layout::footer::ColumnKind;

/// One granule's slice of a projected scan: its decoded columns in the order the scan asked for them.
#[derive(Debug)]
pub struct ScanBatch {
    /// File bytes the granule's blocks occupy — what a reader fetched to produce the batch.
    pub bytes: u64,
    pub columns: Vec<ScanColumn>,
    pub granule_id: u32,
    /// From the granule directory, not the decoded value count: a column with gaps stores only its present values.
    pub row_count: u32,
}

/// One column block of a [`ScanBatch`], in whichever of the reader's two decoded forms the block takes.
#[derive(Debug)]
pub enum ScanColumn {
    /// What [`HefFile::read_column`] returns: every non-string block, and a string block outside the view decoder's
    /// fast path.
    Materialized(ColumnRead),
    /// What [`HefFile::read_column_string_views`] returns: a string block as Arrow views over its own buffers,
    /// paired with its presence bitmap.
    Views { presence: Vec<u8>, views: StringViewArray },
}

/// One block a scan will decode, located ahead of the decode so no worker resolves a mark.
struct ScanBlock {
    /// The file records extents that more than one mark references, so a materializing read must go through the
    /// aliased-extent cache; a file without any takes the scan's direct decode.
    aliased: bool,
    column_id: u32,
    granule_id: u32,
    mark: ColumnMark,
    /// A footer-declared string column, tried through the view decoder first.
    string: bool,
}

/// One granule of a window: where its blocks sit in the window's block list, in projection order, and what the
/// granule directory says about it.
struct ScanGranule {
    blocks: std::ops::Range<usize>,
    /// Sum of the blocks' [`estimated_decode_bytes`].
    estimated_bytes: u64,
    granule_id: u32,
    row_count: u32,
}

/// A window's granule ranges and its block list are built together, so this never fires; it stands where a slice
/// would otherwise be indexed, so nothing in the scan can panic.
const OUT_OF_STEP: FormatError = FormatError::Structural {
    rule: "scan window out of step with its block list",
};

/// Decode memory one block is expected to hold at its peak: the inflated body plus sixteen bytes per row — the
/// widest fixed-width value, or one string view — so the estimate tracks the real decode to within a small factor
/// whichever kind the block is.
fn estimated_decode_bytes(mark: &ColumnMark) -> u64 {
    mark.uncompressed_size
        .saturating_add(u64::from(mark.row_count).saturating_mul(16))
}

impl HefFile {
    /// Reads `column_ids` across `granule_ids` in one pass, handing `sink` one batch per granule: the granules in
    /// the order asked for, each batch's columns in the order asked for, and never more than about
    /// [`SCAN_WINDOW_BYTES`] of decoded blocks held at once. Each block comes back in the form the per-column reads
    /// give it — string blocks as Arrow views, everything else materialized — so a scan and a per-column read of the
    /// same block agree value for value.
    ///
    /// The blocks of a window decode side by side on the shared thread pool when the window holds enough work to pay
    /// for waking the pool — at least two granules of at least 64 KiB of inflated block bytes each, the rule
    /// full-text search fans out by ([`scan_in_parallel`]); inflated bytes decode at about a gigabyte a second
    /// whatever the column kind, so that is a couple of hundred microseconds of work per granule against a hand-off
    /// that costs on the order of a hundred. A smaller scan stays on the calling thread and hands over each granule
    /// as it is decoded.
    /// A column or granule the footer does not list, or a block the file does not store, is an error, and an error
    /// from `sink` ends the scan.
    ///
    /// This is the bulk `read_columns` path of the reader interface. See: hef-apis/spec.md
    pub fn scan_projected(
        &self,
        column_ids: &[u32],
        granule_ids: &[u32],
        sink: impl FnMut(ScanBatch) -> Result<(), FormatError>,
    ) -> Result<(), FormatError> {
        self.scan_projected_inner(column_ids, granule_ids, true, SCAN_WINDOW_BYTES, sink)
    }

    /// [`Self::scan_projected`] kept on the calling thread whatever the size of the scan — the same batches in the
    /// same order — for a caller that already runs one scan per worker and wants no fan-out nested inside its own.
    pub fn scan_projected_serial(
        &self,
        column_ids: &[u32],
        granule_ids: &[u32],
        sink: impl FnMut(ScanBatch) -> Result<(), FormatError>,
    ) -> Result<(), FormatError> {
        self.scan_projected_inner(column_ids, granule_ids, false, SCAN_WINDOW_BYTES, sink)
    }

    fn scan_projected_inner(
        &self,
        column_ids: &[u32],
        granule_ids: &[u32],
        fan_out: bool,
        window_bytes: u64,
        mut sink: impl FnMut(ScanBatch) -> Result<(), FormatError>,
    ) -> Result<(), FormatError> {
        let string_columns = column_ids
            .iter()
            .map(|column_id| {
                self.footer
                    .columns
                    .iter()
                    .find(|column| column.column_id == *column_id)
                    .map(|column| column.kind == ColumnKind::String)
                    .ok_or(FormatError::RefOutOfRange {
                        what: "projected column missing from the footer",
                    })
            })
            .collect::<Result<Vec<bool>, _>>()?;
        let aliased = !self.aliased_extents.is_empty();
        let mut window: Vec<ScanGranule> = Vec::with_capacity(granule_ids.len());
        let mut blocks: Vec<ScanBlock> = Vec::with_capacity(granule_ids.len().saturating_mul(column_ids.len()));
        let mut window_used = 0u64;
        for &granule_id in granule_ids {
            let row_count = self
                .granule_entry(granule_id)
                .ok_or(FormatError::RefOutOfRange {
                    what: "scanned granule missing from the granule directory",
                })?
                .row_count;
            let first = blocks.len();
            let mut estimated_bytes = 0u64;
            for (&column_id, &string) in column_ids.iter().zip(&string_columns) {
                let mark = self.mark(column_id, 0, granule_id)?.ok_or(FormatError::RefOutOfRange {
                    what: "no mark for (column, projection, granule)",
                })?;
                estimated_bytes = estimated_bytes.saturating_add(estimated_decode_bytes(&mark));
                blocks.push(ScanBlock {
                    aliased,
                    column_id,
                    granule_id,
                    mark,
                    string,
                });
            }
            // A granule larger than the window on its own still goes through, as a window of one. Its blocks are
            // already at the tail of the list; they move to the front once the window before them is delivered.
            let first = if !window.is_empty() && window_used.saturating_add(estimated_bytes) > window_bytes {
                let delivered = blocks.get(..first).ok_or(OUT_OF_STEP)?;
                self.decode_window(&window, delivered, fan_out, &mut sink)?;
                window.clear();
                window_used = 0;
                blocks.drain(..first);
                0
            } else {
                first
            };
            window_used = window_used.saturating_add(estimated_bytes);
            window.push(ScanGranule {
                blocks: first..blocks.len(),
                estimated_bytes,
                granule_id,
                row_count,
            });
        }
        if !window.is_empty() {
            self.decode_window(&window, &blocks, fan_out, &mut sink)?;
        }
        Ok(())
    }

    /// The granule directory's entry for `granule_id`. The writer numbers granules in directory order, so the entry
    /// is normally at its own index; a directory in any other order is searched.
    fn granule_entry(&self, granule_id: u32) -> Option<&GranuleEntry> {
        let granules = &self.footer.granules;
        granules
            .get(granule_id as usize)
            .filter(|granule| granule.granule_id == granule_id)
            .or_else(|| granules.iter().find(|granule| granule.granule_id == granule_id))
    }

    /// Decodes one window's blocks and delivers its batches in granule order: side by side on the pool when
    /// `fan_out` allows and the window is worth it, otherwise one granule at a time on the calling thread, so a
    /// serial scan never holds more than the batch in hand.
    fn decode_window(
        &self,
        window: &[ScanGranule],
        blocks: &[ScanBlock],
        fan_out: bool,
        sink: &mut impl FnMut(ScanBatch) -> Result<(), FormatError>,
    ) -> Result<(), FormatError> {
        let inflated_bytes: u64 = blocks.iter().map(|block| block.mark.uncompressed_size).sum();
        if !(fan_out && scan_in_parallel(window.len(), inflated_bytes / window.len().max(1) as u64)) {
            for granule in window {
                let own = blocks.get(granule.blocks.clone()).ok_or(OUT_OF_STEP)?;
                let columns = own
                    .iter()
                    .map(|block| self.scan_block(block))
                    .collect::<Result<_, _>>()?;
                sink(Self::batch(granule, own, columns))?;
            }
            return Ok(());
        }
        let decoded: Vec<ScanColumn> = blocks
            .par_iter()
            .map(|block| self.scan_block(block))
            .collect::<Result<_, _>>()?;
        let mut decoded = decoded.into_iter();
        for granule in window {
            let own = blocks.get(granule.blocks.clone()).ok_or(OUT_OF_STEP)?;
            let columns = decoded.by_ref().take(own.len()).collect();
            sink(Self::batch(granule, own, columns))?;
        }
        Ok(())
    }

    /// One granule's batch from its decoded columns and `own`, its blocks.
    fn batch(granule: &ScanGranule, own: &[ScanBlock], columns: Vec<ScanColumn>) -> ScanBatch {
        ScanBatch {
            bytes: own.iter().map(|block| block.mark.compressed_size).sum(),
            columns,
            granule_id: granule.granule_id,
            row_count: granule.row_count,
        }
    }

    /// Decodes one located block: a string block through the view decoder where it applies, everything else — and a
    /// string block the view decoder declines — through the materializing column read, exactly as the per-column
    /// APIs would. Kept out of line so a profiler attributes worker samples to the scan.
    #[inline(never)]
    fn scan_block(&self, block: &ScanBlock) -> Result<ScanColumn, FormatError> {
        if block.string {
            if block.mark.page_count > 1 {
                if let Some((presence, views)) = self.read_column_string_views(block.column_id, block.granule_id)? {
                    return Ok(ScanColumn::Views { presence, views });
                }
            } else if block.mark.compressed_size != 0 {
                // The block is fetched once and its presence frame decoded once; only a declined view decode pays for
                // a second fetch through the materializing read.
                let raw = self.read_column_raw_with_mark(block.granule_id, &block.mark)?;
                if let Some(views) = self.decode_string_block_views(block.column_id, raw.pipeline, raw.body)? {
                    validate_block_counts(&raw.presence, views.len(), raw.row_count as usize)?;
                    return Ok(ScanColumn::Views {
                        presence: raw.presence.into_owned(),
                        views,
                    });
                }
            }
        }
        if !block.aliased && block.mark.page_count <= 1 && block.mark.compressed_size != 0 {
            // The direct path of a scan over a file without aliased extents: the block is sliced and decoded exactly
            // once, without the shared-extent cache a per-column read consults so that several columns naming one
            // extent decode it once between them.
            let raw = self.read_column_raw_with_mark(block.granule_id, &block.mark)?;
            let data = decode_block_shared(raw.pipeline, raw.body, self.shared_alphabet(block.column_id))?;
            validate_block_counts(&raw.presence, data.row_count(), raw.row_count as usize)?;
            return Ok(ScanColumn::Materialized(ColumnRead {
                presence: raw.presence.into_owned(),
                data,
            }));
        }
        let read = self.read_column_with_mark(block.column_id, block.granule_id, &block.mark)?;
        Ok(ScanColumn::Materialized(read))
    }
}

#[cfg(test)]
#[path = "test/scan.rs"]
mod tests;
