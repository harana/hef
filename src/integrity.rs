//! HEF's shared integrity commitments.
//!
//! Large stripe bytes are hashed exactly once. The resulting stripe checksums are then framed into two independent,
//! domain-separated roots: the deterministic publication identity and the authoritative file seal. Header, alignment
//! gaps, and footer bytes remain covered by the file seal, but no root re-hashes a stripe payload already represented
//! by its checksum.

use super::error::FormatError;
use super::layout::HEADER_BLOCK_LEN;
use super::layout::footer::{IntegrityGapEntry, StripeEntry};

const FILE_ID_CONTEXT: &str = "pulse HEF v1 deterministic file identity";
const FILE_SEAL_CONTEXT: &str = "pulse HEF v1 authoritative segment seal";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentKind {
    Gap = 0,
    Stripe = 1,
}

/// One commitment covering a disjoint byte range in the data area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DataCommitment {
    digest: [u8; 32],
    kind: SegmentKind,
    len: u64,
    offset: u64,
    stripe_id: u32,
}

/// Extracts the serializable non-stripe leaves from a computed data partition for storage in the authenticated footer.
pub(crate) fn integrity_gaps(commitments: &[DataCommitment]) -> Vec<IntegrityGapEntry> {
    commitments
        .iter()
        .filter(|commitment| commitment.kind == SegmentKind::Gap)
        .map(|commitment| IntegrityGapEntry {
            blake3: commitment.digest,
            byte_len: commitment.len,
            file_offset: commitment.offset,
        })
        .collect()
}

/// Reconstructs the exact data-area partition from footer-authenticated stripe and gap leaves without reading the data
/// bytes. This is what makes a header+footer cold open sufficient to authenticate the roots used by later range reads.
pub(crate) fn declared_data_commitments(
    data_end: usize,
    stripes: &[StripeEntry],
    stripe_checksums: &[[u8; 32]],
    gaps: &[IntegrityGapEntry],
) -> Result<Vec<DataCommitment>, FormatError> {
    if stripe_checksums.len() != stripes.len() {
        return Err(FormatError::Structural {
            rule: "stripe checksum count must match the stripe directory",
        });
    }
    if data_end < HEADER_BLOCK_LEN {
        return Err(FormatError::Structural {
            rule: "HEF data commitment range ends before the header",
        });
    }

    let mut commitments = Vec::with_capacity(stripes.len().saturating_add(gaps.len()));
    commitments.extend(
        stripes
            .iter()
            .zip(stripe_checksums)
            .map(|(stripe, checksum)| DataCommitment {
                digest: *checksum,
                kind: SegmentKind::Stripe,
                len: stripe.byte_len,
                offset: stripe.file_offset,
                stripe_id: stripe.stripe_id,
            }),
    );
    commitments.extend(gaps.iter().map(|gap| DataCommitment {
        digest: gap.blake3,
        kind: SegmentKind::Gap,
        len: gap.byte_len,
        offset: gap.file_offset,
        stripe_id: 0,
    }));
    commitments.sort_unstable_by_key(|commitment| commitment.offset);

    let mut cursor = HEADER_BLOCK_LEN as u64;
    for commitment in &commitments {
        if commitment.len == 0 || commitment.offset != cursor {
            return Err(FormatError::Structural {
                rule: "stripe and gap commitments must form one exact ordered data partition",
            });
        }
        cursor = cursor.checked_add(commitment.len).ok_or(FormatError::Structural {
            rule: "data commitment range overflows",
        })?;
    }
    if cursor != data_end as u64 {
        return Err(FormatError::Structural {
            rule: "stripe and gap commitments must end at the footer",
        });
    }
    Ok(commitments)
}

fn update_u8(hasher: &mut blake3::Hasher, value: u8) {
    hasher.update(&[value]);
}

fn update_u32(hasher: &mut blake3::Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn update_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

fn update_commitments(hasher: &mut blake3::Hasher, commitments: &[DataCommitment]) {
    update_u64(hasher, commitments.len() as u64);
    for commitment in commitments {
        update_u8(hasher, commitment.kind as u8);
        update_u32(hasher, commitment.stripe_id);
        update_u64(hasher, commitment.offset);
        update_u64(hasher, commitment.len);
        hasher.update(&commitment.digest);
    }
}

/// Builds an exact ordered partition of `[HEADER_BLOCK_LEN, data_end)`. Stripe ranges reuse their already-computed
/// checksums; alignment gaps are normally tiny and are hashed directly so the seal still covers every stored byte.
pub(crate) fn data_commitments(
    bytes: &[u8],
    data_end: usize,
    stripes: &[StripeEntry],
    stripe_checksums: &[[u8; 32]],
) -> Result<Vec<DataCommitment>, FormatError> {
    if data_end > bytes.len() {
        return Err(FormatError::Structural {
            rule: "HEF data commitment range is outside the file",
        });
    }
    data_commitments_with(data_end, stripes, stripe_checksums, |range| bytes.get(range))
}

/// [`data_commitments`] for a writer whose data area is not one contiguous buffer: each alignment gap's bytes are
/// fetched through `gap` by their file range instead of sliced from the whole file.
pub(crate) fn data_commitments_with<'a>(
    data_end: usize,
    stripes: &[StripeEntry],
    stripe_checksums: &[[u8; 32]],
    gap: impl Fn(std::ops::Range<usize>) -> Option<&'a [u8]>,
) -> Result<Vec<DataCommitment>, FormatError> {
    if stripe_checksums.len() != stripes.len() {
        return Err(FormatError::Structural {
            rule: "stripe checksum count must match the stripe directory",
        });
    }
    if data_end < HEADER_BLOCK_LEN {
        return Err(FormatError::Structural {
            rule: "HEF data commitment range is outside the file",
        });
    }

    let mut ordered: Vec<_> = stripes.iter().zip(stripe_checksums).collect();
    ordered.sort_unstable_by_key(|(stripe, _)| stripe.file_offset);
    let mut cursor = HEADER_BLOCK_LEN;
    let mut commitments = Vec::with_capacity(ordered.len().saturating_mul(2).saturating_add(1));

    for (stripe, checksum) in ordered {
        let start = usize::try_from(stripe.file_offset).map_err(|_| FormatError::Structural {
            rule: "stripe offset does not fit in memory",
        })?;
        let len = usize::try_from(stripe.byte_len).map_err(|_| FormatError::Structural {
            rule: "stripe length does not fit in memory",
        })?;
        let end = start.checked_add(len).ok_or(FormatError::Structural {
            rule: "stripe range overflows",
        })?;
        if start < cursor || end > data_end {
            return Err(FormatError::Structural {
                rule: "stripe ranges must be ordered, disjoint, and inside the data area",
            });
        }
        if start > cursor {
            let gap = gap(cursor..start).ok_or(FormatError::Truncated { what: "data gap" })?;
            commitments.push(DataCommitment {
                digest: *crate::file::integrity::hash_tree(gap).as_bytes(),
                kind: SegmentKind::Gap,
                len: gap.len() as u64,
                offset: cursor as u64,
                stripe_id: 0,
            });
        }
        commitments.push(DataCommitment {
            digest: *checksum,
            kind: SegmentKind::Stripe,
            len: stripe.byte_len,
            offset: stripe.file_offset,
            stripe_id: stripe.stripe_id,
        });
        cursor = end;
    }

    if cursor < data_end {
        let gap = gap(cursor..data_end).ok_or(FormatError::Truncated { what: "data gap" })?;
        commitments.push(DataCommitment {
            digest: *crate::file::integrity::hash_tree(gap).as_bytes(),
            kind: SegmentKind::Gap,
            len: gap.len() as u64,
            offset: cursor as u64,
            stripe_id: 0,
        });
    }
    Ok(commitments)
}

/// Recomputes the data partition's small gap leaves from bytes and checks that they match the authenticated footer
/// declarations. Stripe payloads are deliberately not re-hashed here; their one verification pass is performed by the
/// caller after the segment seal authenticates the expected stripe roots.
pub(crate) fn verify_declared_data_commitments(
    bytes: &[u8],
    data_end: usize,
    stripes: &[StripeEntry],
    stripe_checksums: &[[u8; 32]],
    gaps: &[IntegrityGapEntry],
) -> Result<Vec<DataCommitment>, FormatError> {
    let computed = data_commitments(bytes, data_end, stripes, stripe_checksums)?;
    let declared = declared_data_commitments(data_end, stripes, stripe_checksums, gaps)?;
    if computed != declared {
        return Err(FormatError::Blake3Mismatch { scope: "data gap" });
    }
    Ok(declared)
}

/// Derives the deterministic content identity from canonical metadata and the data-area commitments. Generation and
/// creation time are intentionally absent, so a retry of the same physical content produces the same id.
#[allow(clippy::too_many_arguments)]
pub(crate) fn derive_file_id(
    tenant: [u8; 16],
    min_epoch: u64,
    max_epoch: u64,
    min_sequence: u64,
    max_sequence: u64,
    row_count: u64,
    schema_fingerprint: &[u8; 32],
    layout_class: u8,
    commitments: &[DataCommitment],
) -> u128 {
    let mut hasher = blake3::Hasher::new_derive_key(FILE_ID_CONTEXT);
    hasher.update(&tenant);
    update_u64(&mut hasher, min_epoch);
    update_u64(&mut hasher, max_epoch);
    update_u64(&mut hasher, min_sequence);
    update_u64(&mut hasher, max_sequence);
    update_u64(&mut hasher, row_count);
    hasher.update(schema_fingerprint);
    update_u8(&mut hasher, layout_class);
    update_commitments(&mut hasher, commitments);
    let digest = hasher.finalize();
    u128::from_le_bytes(digest.as_bytes()[..16].try_into().unwrap_or([0; 16]))
}

/// Derives the authoritative seal over the exact file content. Stripe payloads enter through their checksums; only
/// the small header/footer regions and alignment gaps are hashed directly.
pub(crate) fn derive_file_seal(
    header: &[u8],
    footer_region: &[u8],
    trailer: &[u8],
    total_len: u64,
    commitments: &[DataCommitment],
) -> [u8; 32] {
    derive_file_seal_from_header_hash(
        header.len() as u64,
        crate::file::integrity::hash_tree(header).as_bytes(),
        footer_region,
        trailer,
        total_len,
        commitments,
    )
}

/// [`derive_file_seal`] for a reader that holds the header block's BLAKE3 and length but not its bytes — a cold open
/// that fetched only the file's tail. Byte-identical to sealing over the header itself.
pub(crate) fn derive_file_seal_from_header_hash(
    header_len: u64,
    header_hash: &[u8; 32],
    footer_region: &[u8],
    trailer: &[u8],
    total_len: u64,
    commitments: &[DataCommitment],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(FILE_SEAL_CONTEXT);
    update_u64(&mut hasher, total_len);
    update_u64(&mut hasher, header_len);
    hasher.update(header_hash);
    update_commitments(&mut hasher, commitments);
    update_u64(&mut hasher, footer_region.len().saturating_add(trailer.len()) as u64);
    let mut footer_hasher = blake3::Hasher::new();
    footer_hasher.update_rayon(footer_region);
    footer_hasher.update(trailer);
    hasher.update(footer_hasher.finalize().as_bytes());
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_region_and_domain_changes_its_root() {
        let mut bytes = vec![0; HEADER_BLOCK_LEN + 6];
        bytes[HEADER_BLOCK_LEN..].copy_from_slice(b"abcdef");
        let stripes = [StripeEntry {
            stripe_id: 7,
            file_offset: (HEADER_BLOCK_LEN + 1) as u64,
            byte_len: 4,
            first_row_ordinal: 0,
            row_count: 1,
        }];
        let checksum = [*crate::file::integrity::hash_tree(b"bcde").as_bytes()];
        let commitments = data_commitments(&bytes, bytes.len(), &stripes, &checksum).unwrap();
        let id = derive_file_id([1; 16], 2, 2, 3, 3, 1, &[4; 32], 0, &commitments);
        let seal = derive_file_seal(&bytes[..HEADER_BLOCK_LEN], b"footer", b"tail", 12, &commitments);
        assert_ne!(id.to_le_bytes().as_slice(), &seal[..16]);

        bytes[HEADER_BLOCK_LEN] ^= 1;
        let changed = data_commitments(&bytes, bytes.len(), &stripes, &checksum).unwrap();
        assert_ne!(commitments, changed, "the leading alignment gap is authoritative");
    }
}
