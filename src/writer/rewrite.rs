//! Turns a stripe-reuse plan into the ordered spans of a replacement file, so the stripes that did not change can be
//! copied straight into the new object instead of re-uploaded.
//!
//! A day-scale compaction that touches a handful of stripes should move a handful of stripes' worth of bytes, not the
//! whole file. Each unchanged stripe becomes a span copied **by reference** from the source object (a server-side copy
//! on the cloud tier, never routed through this process), and only the rebuilt stripes plus the header and footer are
//! spans uploaded fresh. Executing the spans against an object store belongs to the application that owns that store;
//! the finished object is byte-identical to a full rewrite of the same inputs, so a reader cannot tell the two apart.
//!
//! See: hef-write-path/spec.md

use super::compaction::{RewritePlan, StripeDisposition};
use crate::layout::footer::StripeEntry;

/// One contiguous span of the replacement file, in file order.
///
/// The spans tile the whole file back to front with no gaps or overlaps: an unchanged stripe is copied by reference
/// from the source object, and everything else — the header, a rebuilt stripe, inter-stripe padding, the footer — is
/// uploaded from the freshly built replacement bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteSegment {
    /// Copy `len` bytes from the source object starting at `source_offset` — an unchanged stripe reused by reference,
    /// never re-uploaded.
    CopyFromSource { len: u64, source_offset: u64 },
    /// Upload the next `len` bytes of the replacement, taken in file order — a rebuilt stripe, the header, the footer,
    /// or padding.
    Fresh { len: u64 },
}

impl RewriteSegment {
    /// How many bytes of the replacement this span covers.
    fn len(self) -> u64 {
        match self {
            RewriteSegment::CopyFromSource { len, .. } | RewriteSegment::Fresh { len } => len,
        }
    }
}

/// Builds the ordered spans for rewriting a file: the header, each stripe (reused by reference or rebuilt, per `plan`),
/// any inter-stripe padding, and the trailing footer — together tiling `[0, file_len)`.
///
/// `replacement_stripes` is the stripe layout of the *replacement* file (its byte offsets and lengths), one entry per
/// entry in `plan.dispositions` and in the same order. A stripe the plan marks `Reuse` is copied by reference; a stripe
/// it marks `Rebuild` is uploaded from the replacement. A reused stripe smaller than `min_part_bytes` is downgraded to a
/// rebuild so every copied span is a valid whole multipart part — the conservative way to handle a rare sub-minimum
/// stripe (rebuilding is never wrong, only slower). Pass `min_part_bytes` `0` to copy every reused stripe regardless of
/// size (the in-memory provider and any backend without a minimum).
pub fn rewrite_segments(
    plan: &RewritePlan,
    replacement_stripes: &[StripeEntry],
    file_len: u64,
    min_part_bytes: u64,
) -> Vec<RewriteSegment> {
    let mut segments = Vec::new();
    let mut cursor = 0u64;
    for (disposition, stripe) in plan.dispositions.iter().zip(replacement_stripes) {
        // Header on the first stripe, then any padding between stripes, is uploaded fresh.
        if stripe.file_offset > cursor {
            segments.push(RewriteSegment::Fresh {
                len: stripe.file_offset - cursor,
            });
            cursor = stripe.file_offset;
        }
        match disposition {
            StripeDisposition::Reuse {
                source_offset,
                byte_len,
            } if *byte_len >= min_part_bytes => {
                segments.push(RewriteSegment::CopyFromSource {
                    len: *byte_len,
                    source_offset: *source_offset,
                });
            }
            _ => segments.push(RewriteSegment::Fresh { len: stripe.byte_len }),
        }
        cursor += stripe.byte_len;
    }
    // The footer (and anything after the last stripe) is uploaded fresh.
    if cursor < file_len {
        segments.push(RewriteSegment::Fresh { len: file_len - cursor });
    }
    segments
}

#[cfg(test)]
#[path = "test/rewrite.rs"]
mod tests;
