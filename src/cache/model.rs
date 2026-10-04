//! The key a cached block is stored under: whose file, which file, what part of it, and which byte range.
//!
//! See: hef-hardware-deployment/spec.md

use crate::events::TenantId;

/// Names one cached byte range of one HEF file for one tenant.
///
/// The tenant is part of the key, so two tenants never share a cached copy even if their bytes happen to match. HEF
/// files are immutable, so a key always names the same bytes for as long as the file exists.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockKey {
    pub file_id: u128,
    pub kind: BlockKind,
    pub length: u64,
    /// Byte offset from the start of the file.
    pub offset: u64,
    pub tenant_id: TenantId,
}

impl BlockKey {
    /// A fixed-width, order-independent encoding of the key, used to name the key's copy on disk without exposing the
    /// tenant id or file id there.
    pub fn digest(&self) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.tenant_id.uuid().as_bytes());
        hasher.update(&self.file_id.to_le_bytes());
        hasher.update(&[self.kind.tag()]);
        hasher.update(&self.offset.to_le_bytes());
        hasher.update(&self.length.to_le_bytes());
        hasher.finalize()
    }
}

/// Which part of a HEF file a cached range holds.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BlockKind {
    /// One column block (granule) inside a stripe.
    ColumnBlock,
    /// The file's footer tail, read on every open.
    Footer,
    /// A marks page mapping granules to byte ranges.
    MarksPage,
}

impl BlockKind {
    /// A stable byte for the kind, independent of declaration order, so a key's digest never changes when variants are
    /// re-sorted.
    fn tag(self) -> u8 {
        match self {
            BlockKind::ColumnBlock => 1,
            BlockKind::Footer => 2,
            BlockKind::MarksPage => 3,
        }
    }
}
