//! Plans how a compaction rewrite reuses the bytes it can and rebuilds only what changed.
//!
//! Two jobs live here. The first folds a derived-columns sidecar's settled columns into a rewritten base file, keeping
//! a sibling only for the columns still short of their settle horizon. The second — the stripe-reuse *rewrite planner*
//! — looks at a source file's stripes and decides, for each one, whether the rewrite can copy its bytes **by reference**
//! (it is unchanged and proven to reproduce byte-identically) or must rebuild it. Copying an unchanged stripe by
//! reference turns a day-scale compaction that touches a few stripes into a few requests instead of a full re-upload,
//! The replacement seal composes from its stripe checksums, so unchanged stripes reuse both their bytes and their
//! integrity leaves regardless of where they land. The output is byte-identical to a full rewrite.

use super::build::BuiltHef;
use crate::events::TimestampValue;
use crate::layout::footer::StripeEntry;
use hashbrown::HashMap;

/// The byte length of a stripe's BLAKE3 checksum, matching [`StripeEntry`]'s paired `stripe_checksums` entries.
const STRIPE_CHECKSUM_BYTES: usize = 32;

/// One Tier B column carried in a derived-columns sibling file, past which it is folded into the base file on the next
/// compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedColumnEntry {
    pub column_name: String,
    pub settle_horizon: TimestampValue,
}

/// A derived-columns sibling file: Tier B columns row-aligned ordinal-for-ordinal to a base HEF file, each carrying its
/// own settle horizon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedColumnsSidecar {
    pub columns: Vec<DerivedColumnEntry>,
    pub row_count: u64,
}

/// What a compaction pass does with a range's derived-columns sidecar.
pub struct FoldedCompaction {
    /// Columns past their settle horizon, now embedded in the rewritten base file.
    pub embedded_columns: Vec<DerivedColumnEntry>,
    /// A sibling re-emitted for the columns still short of their settle horizon; `None` once every column has settled.
    pub unsettled_sidecar: Option<DerivedColumnsSidecar>,
}

/// Splits `sidecar`'s columns by settle horizon as of `as_of`: columns whose settle horizon has passed are embedded
/// into the rewritten base; the rest are re-emitted in a new sidecar covering only the unsettled tail, with row
/// alignment carried over unchanged.
pub fn fold_settled_derived_columns(sidecar: &DerivedColumnsSidecar, as_of: TimestampValue) -> FoldedCompaction {
    let (embedded_columns, unsettled): (Vec<_>, Vec<_>) = sidecar
        .columns
        .iter()
        .cloned()
        .partition(|column| column.settle_horizon <= as_of);
    let unsettled_sidecar = (!unsettled.is_empty()).then_some(DerivedColumnsSidecar {
        columns: unsettled,
        row_count: sidecar.row_count,
    });
    FoldedCompaction {
        embedded_columns,
        unsettled_sidecar,
    }
}

/// What the rewrite planner decided to do with one source stripe when producing the replacement file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeDisposition {
    /// The stripe changed, or its byte-identical reuse could not be proven, so it is rebuilt from rows. Conservative:
    /// any doubt rebuilds, and a rebuild is never wrong, only slower.
    Rebuild { stripe_id: u32 },
    /// The stripe is unchanged and proven to reproduce byte-identically, so it is copied **by reference** from the
    /// source file's `[source_offset, source_offset + byte_len)` range — a `server_side_copy` on the cloud tier, a
    /// `copy_file_range`/reflink locally, or a buffered copy otherwise — instead of being re-read and re-uploaded.
    Reuse { byte_len: u64, source_offset: u64 },
}

/// One source stripe together with what the rewrite does to it — the rewrite planner's per-stripe input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeChange {
    /// The stripe intersects a sidecar fold, a cleared deletion vector, or a rewritten row range, so its bytes differ.
    pub changed: bool,
    pub stripe: StripeEntry,
    /// The planner cannot prove this stripe would reproduce byte-identically (an uncertain encoding strategy or row
    /// set). With any doubt the stripe is rebuilt.
    pub uncertain: bool,
}

/// A plan for rewriting a HEF file that reuses the bytes of its unchanged stripes instead of moving all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewritePlan {
    /// One disposition per source stripe, in file order.
    pub dispositions: Vec<StripeDisposition>,
}

impl RewritePlan {
    /// Whether every source stripe is reused — a rewrite that moves no stripe bytes at all.
    pub fn reuses_every_stripe(&self) -> bool {
        self.dispositions
            .iter()
            .all(|disposition| matches!(disposition, StripeDisposition::Reuse { .. }))
    }

    /// How many bytes this plan copies by reference rather than re-uploading — the write amplification splice avoids.
    pub fn reused_bytes(&self) -> u64 {
        self.dispositions
            .iter()
            .map(|disposition| match disposition {
                StripeDisposition::Reuse { byte_len, .. } => *byte_len,
                StripeDisposition::Rebuild { .. } => 0,
            })
            .sum()
    }
}

/// Plans a stripe-reuse rewrite, conservatively rebuilding any stripe whose byte identity cannot be proven. The former
/// `header_identical_len` argument is retained for source compatibility with planner callers but no longer affects hash
/// reuse: segment seals reuse stripe leaves independently of file position.
pub fn plan_stripe_reuse(changes: &[StripeChange], _header_identical_len: u64) -> RewritePlan {
    let dispositions: Vec<StripeDisposition> = changes
        .iter()
        .map(|change| {
            if change.changed || change.uncertain {
                StripeDisposition::Rebuild {
                    stripe_id: change.stripe.stripe_id,
                }
            } else {
                StripeDisposition::Reuse {
                    byte_len: change.stripe.byte_len,
                    source_offset: change.stripe.file_offset,
                }
            }
        })
        .collect();

    RewritePlan { dispositions }
}

/// The byte range a stripe occupies in a built file, or `None` if the footer's offsets fall outside the bytes.
fn stripe_bytes<'a>(built: &'a BuiltHef, stripe: &StripeEntry) -> Option<&'a [u8]> {
    let start = usize::try_from(stripe.file_offset).ok()?;
    let end = start.checked_add(usize::try_from(stripe.byte_len).ok()?)?;
    built.bytes.get(start..end)
}

/// Plans a stripe-reuse rewrite straight from a source file and its full rewrite, matching stripes by **content** so an
/// unchanged stripe is reused wherever it landed — even if an earlier change shifted it to a new offset.
///
/// Stripe-relative addressing makes a stripe's bytes position-independent: the same rows at the same lifecycle stage
/// encode to the same bytes with the same per-stripe BLAKE3, no matter where the stripe sits in the file (only the
/// footer's stripe base offset moves). So this matches each replacement stripe to a source stripe with the identical
/// checksum, confirms the bytes are byte-identical, and emits a `Reuse` that copies from the **source** offset into the
/// replacement's (possibly relocated) offset. Any stripe with no byte-identical source match is rebuilt — conservative,
/// so correctness never rests on an unproven reuse. The returned plan's dispositions line up one-for-one with
/// `replacement.footer.stripes`, ready for [`rewrite_segments`](super::rewrite::rewrite_segments).
///
/// The replacement writer has already built its authoritative seal from these checksum leaves, so no raw whole-file
/// or prefix-subtree re-hash is part of this plan.
pub fn plan_rewrite(source: &BuiltHef, replacement: &BuiltHef) -> RewritePlan {
    // Index the source stripes by their per-stripe BLAKE3, so a replacement stripe can find its unchanged twin wherever
    // it moved. First checksum wins when several source stripes share bytes; any of them copies identically.
    let mut source_by_checksum: HashMap<[u8; STRIPE_CHECKSUM_BYTES], &StripeEntry> = HashMap::default();
    for (stripe, checksum) in source.footer.stripes.iter().zip(&source.footer.stripe_checksums) {
        source_by_checksum.entry(*checksum).or_insert(stripe);
    }

    let changes: Vec<StripeChange> = replacement
        .footer
        .stripes
        .iter()
        .zip(&replacement.footer.stripe_checksums)
        .map(|(replacement_stripe, checksum)| {
            let reused = source_by_checksum
                .get(checksum)
                .filter(|source_stripe| source_stripe.byte_len == replacement_stripe.byte_len)
                // Confirm byte-identity outright rather than trusting the checksum: any doubt rebuilds.
                .filter(|source_stripe| {
                    match (
                        stripe_bytes(source, source_stripe),
                        stripe_bytes(replacement, replacement_stripe),
                    ) {
                        (Some(from), Some(to)) => from == to,
                        _ => false,
                    }
                });
            match reused {
                // Carry the *source* stripe so `Reuse.source_offset` points at where the bytes live in the source.
                Some(source_stripe) => StripeChange {
                    changed: false,
                    stripe: **source_stripe,
                    uncertain: false,
                },
                None => StripeChange {
                    changed: true,
                    stripe: *replacement_stripe,
                    uncertain: false,
                },
            }
        })
        .collect();

    plan_stripe_reuse(&changes, 0)
}

#[cfg(test)]
#[path = "test/compaction.rs"]
mod tests;
