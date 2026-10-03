//! The one place the store proves bytes are the bytes it meant to store.
//!
//! Three jobs live here, and every consumer — HEF files, the HEJ journal, and cached objects — uses these and only
//! these:
//!
//! - **Whole-file BLAKE3**, spread across threads for big files but byte-for-byte
//! identical to a single-threaded hash, so a file's identity never depends on the machine that produced it
//! ([`hash_tree`]).
//! - **A CRC-64/NVME precheck** ([`crc64_nvme`]) — a fast, *non-authoritative*
//! gate that may reject early but never admits bytes BLAKE3 would reject.
//! - **An outboard BLAKE3 tree** ([`verify_object_range`]) that verifies any byte
//! range against an authenticated root without re-hashing the unread bytes.
//!
//! BLAKE3 is always the authoritative check. The outboard tree is kept *after* the file's own bytes, so the root stays
//! exactly `BLAKE3(content)` whether or not a tree is present; when the tree is absent or unusable, verification falls
//! back to the whole content. No unverified byte is ever returned.

use super::bytes::slice;
use super::constant::{TREE_TRAILER_LEN, TREE_TRAILER_MAGIC};
use super::error::CodecError;
use blake3::CHUNK_LEN;
use blake3::hazmat::*;
use rayon::prelude::*;

/// Inputs at least this large are hashed across threads; smaller ones hash on the calling thread, where spinning up
/// threads would cost more than it saves. It is also the piece [`hash_segments`] cuts a large segment into: 1 MiB is
/// 1024 chunks, a power of two, so every aligned piece is a complete subtree of the standard BLAKE3 tree.
const PARALLEL_HASH_MIN_LEN: usize = 1 << 20; // 1 MiB

/// One interior node of the outboard tree: its left and right child chaining values, [`blake3::OUT_LEN`] bytes each.
const NODE_LEN: usize = blake3::OUT_LEN * 2;

/// The CRC-64/NVME checksum of `bytes`. This is a fast precheck only: it may reject corrupt bytes early, but the
/// authoritative answer is always BLAKE3.
pub fn crc64_nvme(bytes: &[u8]) -> u64 {
    crc_fast::crc64_nvme(bytes)
}

/// Whether `bytes` match an expected CRC-64/NVME precheck value.
pub fn crc64_matches(bytes: &[u8], expected: u64) -> bool {
    crc64_nvme(bytes) == expected
}

/// Computes a CRC-64/NVME precheck over streamed byte ranges. Format-specific authoritative seals are derived by their
/// owners; this helper exists solely for providers that accept CRC-64/NVME during upload.
pub struct StreamingCrc64Nvme {
    crc: crc_fast::Digest,
}

impl StreamingCrc64Nvme {
    /// Starts a fresh checksum; feed the file's byte ranges to [`update`](Self::update) in stream order.
    pub fn new() -> Self {
        Self {
            crc: crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc64Nvme),
        }
    }

    /// Absorbs the next streamed byte range.
    pub fn update(&mut self, bytes: &[u8]) {
        self.crc.update(bytes);
    }

    /// Appends a checksum that was computed independently over the bytes that follow this digest's bytes.
    ///
    /// CRC composition is exact: this produces the same value as feeding both byte ranges to one digest in order,
    /// without reading either range again. It is useful when a format cannot finish its leading header until after a
    /// large data suffix has already been assembled.
    pub fn combine(&mut self, suffix: &Self) {
        self.crc.combine(&suffix.crc);
    }

    /// The CRC-64/NVME over everything streamed so far.
    pub fn finalize(&self) -> u64 {
        self.crc.finalize()
    }
}

impl Default for StreamingCrc64Nvme {
    fn default() -> Self {
        Self::new()
    }
}

/// Hashes `bytes` with BLAKE3 and returns the 32-byte result, spreading a large input over several threads. The result
/// is identical to `blake3::hash(bytes)` for every input — purely a speed-up, never a different hash, so existing
/// checksums and replay checks keep matching.
///
/// The work runs on the process-wide rayon pool, which holds one worker per core no matter how many hashes are in
/// flight. Hashing several large objects at once therefore queues on that fixed pool instead of each hash spawning its
/// own tree of OS threads.
pub fn hash_tree(bytes: &[u8]) -> blake3::Hash {
    if bytes.len() < PARALLEL_HASH_MIN_LEN {
        return blake3::hash(bytes);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update_rayon(bytes);
    hasher.finalize()
}

/// Hashes several byte segments with BLAKE3 in one flat parallel pass and returns each segment's 32-byte result in
/// order — every result identical to `blake3::hash` of that segment. A segment longer than 1 MiB is cut into 1 MiB
/// pieces whose chaining values are merged back up the standard tree, so all the work of all the segments spreads over
/// the process-wide rayon pool as one flat loop with nothing nested inside it; a segment no longer than one piece is
/// hashed whole. This is what a reader opening a file with many stripes calls, so the whole file costs one fan-out
/// rather than one per stripe.
pub fn hash_segments(segments: &[&[u8]]) -> Vec<blake3::Hash> {
    let pieces: Vec<(&[u8], usize)> = segments
        .iter()
        .flat_map(|segment| {
            (0..segment.len().max(1))
                .step_by(PARALLEL_HASH_MIN_LEN)
                .map(move |offset| (*segment, offset))
        })
        .collect();
    let piece_cvs: Vec<[u8; blake3::OUT_LEN]> = pieces
        .par_iter()
        .map(|&(segment, offset)| {
            if segment.len() <= PARALLEL_HASH_MIN_LEN {
                return *blake3::hash(segment).as_bytes();
            }
            leaf_cv(segment, offset, (offset + PARALLEL_HASH_MIN_LEN).min(segment.len()))
        })
        .collect();
    let mut remaining = piece_cvs.as_slice();
    segments
        .iter()
        .map(|segment| {
            let (cvs, rest) = remaining.split_at(segment.len().max(1).div_ceil(PARALLEL_HASH_MIN_LEN));
            remaining = rest;
            blake3::Hash::from_bytes(merge_pieces(cvs, segment.len(), true))
        })
        .collect()
}

/// Folds the chaining values of the consecutive 1 MiB pieces of one `len`-byte input up to its hash, splitting
/// exactly where BLAKE3's tree does: at the largest power-of-two chunk count below the subtree's length, which for a
/// subtree longer than one piece is always a whole number of pieces. A single piece is already its own hash.
fn merge_pieces(cvs: &[[u8; blake3::OUT_LEN]], len: usize, is_root: bool) -> [u8; blake3::OUT_LEN] {
    let [first, rest @ ..] = cvs else {
        // Every segment has at least one piece, so this never runs.
        return [0; blake3::OUT_LEN];
    };
    if rest.is_empty() {
        return *first;
    }
    let left_len = left_subtree_len(len as u64) as usize;
    let (left_cvs, right_cvs) = cvs.split_at(left_len / PARALLEL_HASH_MIN_LEN);
    let left = merge_pieces(left_cvs, left_len, false);
    let right = merge_pieces(right_cvs, len - left_len, false);
    if is_root {
        *merge_subtrees_root(&left, &right, Mode::Hash).as_bytes()
    } else {
        merge_subtrees_non_root(&left, &right, Mode::Hash)
    }
}

/// The content `C` and, when present, the outboard tree found in an on-disk object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboardObject<'a> {
    pub content: &'a [u8],
    pub tree: Option<&'a [u8]>,
}

/// Why a byte range could not be served from an outboard tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeFault {
    /// The tree is sound but the requested bytes do not match it: the data is corrupt and must not be served.
    Corrupt,
    /// The tree is malformed or does not descend from the root, so the range could not be checked through it; verify
    /// the whole content instead.
    TreeUnusable,
}

/// A BLAKE3 root and its optional outboard proof tree, produced by one traversal of the content. Inputs no larger than
/// one chunk group need no interior proof nodes, so `tree` is `None` and a range verifier must read the whole input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltOutboard {
    pub root: [u8; blake3::OUT_LEN],
    pub tree: Option<Vec<u8>>,
}

/// Exact outboard byte length for an input, excluding any container trailer. This is authenticated geometry: callers
/// can reject a proof entry before allocating or reading it when its declared length differs.
pub fn outboard_tree_bytes(content_len: usize, chunk_group_bytes: usize) -> usize {
    let group = chunk_group_bytes.max(CHUNK_LEN);
    node_count(content_len, group).saturating_mul(NODE_LEN)
}

/// Builds an outboard proof tree and the ordinary `BLAKE3(content)` root in the same parallel traversal. Every content
/// byte enters exactly one leaf computation; parent nodes are composed only from child chaining values. This is the
/// path a segmented format uses when the root is already its stripe checksum, avoiding a separate checksum pass.
pub fn build_outboard_tree_and_root(content: &[u8], chunk_group_bytes: usize) -> BuiltOutboard {
    let group = chunk_group_bytes.max(CHUNK_LEN);
    let tree_len = outboard_tree_bytes(content.len(), group);
    if tree_len == 0 {
        return BuiltOutboard {
            root: *hash_tree(content).as_bytes(),
            tree: None,
        };
    }

    let mut nodes = vec![0u8; tree_len];
    let _ = subtree_nodes_into(content, 0, content.len(), group, &mut nodes);
    // A non-empty outboard begins with the root parent: its two child CVs merge with the ROOT flag into the ordinary
    // BLAKE3 digest, while every lower parent uses the non-root merge inside `subtree_nodes_into`.
    let left = cv_at(&nodes[..NODE_LEN], 0).expect("root outboard node has a left CV");
    let right = cv_at(&nodes[..NODE_LEN], blake3::OUT_LEN).expect("root outboard node has a right CV");
    BuiltOutboard {
        root: *merge_subtrees_root(&left, &right, Mode::Hash).as_bytes(),
        tree: Some(nodes),
    }
}

/// Builds the outboard BLAKE3 tree for `content`, or `None` when the content fits in a single chunk group (where a tree
/// would save no reads). The returned bytes are the interior tree nodes in pre-order; appending them after the content
/// keeps the root `BLAKE3(content)` unchanged.
pub fn build_outboard_tree(content: &[u8], chunk_group_bytes: usize) -> Option<Vec<u8>> {
    build_outboard_tree_and_root(content, chunk_group_bytes).tree
}

/// Returns the on-disk object for `content`: the content bytes unchanged, followed by its outboard tree and the
/// trailer. When the content is small enough to need no tree, returns it unchanged.
pub fn attach_outboard_tree(mut content: Vec<u8>, chunk_group_bytes: usize) -> Vec<u8> {
    let Some(tree) = build_outboard_tree(&content, chunk_group_bytes) else {
        return content;
    };
    let tree_len = tree.len() as u64;
    content.extend_from_slice(&tree);
    content.extend_from_slice(&tree_len.to_le_bytes());
    content.extend_from_slice(&TREE_TRAILER_MAGIC);
    content
}

/// Splits an on-disk object into its content and outboard tree by reading the trailing magic. A trailer magic means a
/// tree is present: its length is in the trailer, and the content before it must end in `content_magic`. Anything else
/// is a no-tree object whose content is the whole input. Returns an error only when a trailer is present but does not
/// point at a content block ending in `content_magic`.
pub fn parse_outboard_object<'a>(object: &'a [u8], content_magic: &[u8; 4]) -> Result<OutboardObject<'a>, CodecError> {
    if object.len() < 4 {
        return Ok(OutboardObject {
            content: object,
            tree: None,
        });
    }
    let tail = slice(object, object.len() - 4, 4, "object trailing magic")?;
    if tail != TREE_TRAILER_MAGIC {
        return Ok(OutboardObject {
            content: object,
            tree: None,
        });
    }
    if object.len() < TREE_TRAILER_LEN {
        return Err(CodecError::Truncated {
            what: "outboard trailer",
        });
    }
    let len_bytes = slice(object, object.len() - TREE_TRAILER_LEN, 8, "outboard tree length")?;
    let tree_len = u64::from_le_bytes(len_bytes.try_into().unwrap_or([0; 8])) as usize;
    let content_end = object
        .len()
        .checked_sub(TREE_TRAILER_LEN)
        .and_then(|n| n.checked_sub(tree_len))
        .ok_or(CodecError::Structural {
            rule: "outboard tree length exceeds object",
        })?;
    let content = slice(object, 0, content_end, "outboard content")?;
    let tree = slice(object, content_end, tree_len, "outboard tree")?;
    if content.len() < 4 || slice(content, content.len() - 4, 4, "content end magic")? != content_magic {
        return Err(CodecError::BadMagic {
            expected: "outboard content magic",
        });
    }
    Ok(OutboardObject {
        content,
        tree: Some(tree),
    })
}

/// Verifies `content[start..start+len]` against the root through the outboard `tree`, reading only the chunk groups
/// that overlap the range (plus the tree nodes on the path to them). Returns the verified bytes, or a [`RangeFault`]
/// saying whether the caller should fall back to whole-content verification (`TreeUnusable`) or reject the bytes
/// outright (`Corrupt`).
pub fn verify_range<'a>(
    content: &'a [u8],
    tree: &[u8],
    root: &[u8; blake3::OUT_LEN],
    start: u64,
    len: u64,
    chunk_group_bytes: usize,
) -> Result<&'a [u8], RangeFault> {
    let group = chunk_group_bytes.max(CHUNK_LEN);
    let n = content.len();
    let start = usize::try_from(start).map_err(|_| RangeFault::Corrupt)?;
    let end = start
        .checked_add(usize::try_from(len).map_err(|_| RangeFault::Corrupt)?)
        .ok_or(RangeFault::Corrupt)?;
    if end > n {
        return Err(RangeFault::Corrupt);
    }
    if n <= group {
        return Err(RangeFault::TreeUnusable);
    }
    if len == 0 {
        // An empty range overlaps no chunk group, so the tree proves nothing about the content. Reporting it verified
        // here would accept an object with a corrupt root or a malformed tree; hand it to whole-content verification
        // instead, which still checks the root before serving the (empty) slice.
        return Err(RangeFault::TreeUnusable);
    }
    let mut cursor = 0usize;
    verify_subtree(content, tree, &mut cursor, 0, n, root, true, start, end, group)?;
    slice(content, start, end - start, "verified range").map_err(|_| RangeFault::Corrupt)
}

/// Verifies a range when the caller fetched only the chunk-group-expanded bytes that cover it rather than retaining the
/// whole content in memory. `fetched_start` and all requested offsets are relative to the authenticated content root.
/// This is the network-facing Bao-style path: proof nodes may cover the whole input, but content bytes outside the
/// requested chunk groups are never read or required.
#[expect(
    clippy::too_many_arguments,
    reason = "the proof needs authenticated content geometry plus fetched and requested subranges"
)]
pub fn verify_range_from_slice<'a>(
    content_len: u64,
    fetched: &'a [u8],
    fetched_start: u64,
    tree: &[u8],
    root: &[u8; blake3::OUT_LEN],
    start: u64,
    len: u64,
    chunk_group_bytes: usize,
) -> Result<&'a [u8], RangeFault> {
    let n = usize::try_from(content_len).map_err(|_| RangeFault::Corrupt)?;
    let fetched_start = usize::try_from(fetched_start).map_err(|_| RangeFault::Corrupt)?;
    let fetched_end = fetched_start.checked_add(fetched.len()).ok_or(RangeFault::Corrupt)?;
    let start = usize::try_from(start).map_err(|_| RangeFault::Corrupt)?;
    let end = start
        .checked_add(usize::try_from(len).map_err(|_| RangeFault::Corrupt)?)
        .ok_or(RangeFault::Corrupt)?;
    if fetched_end > n || end > n || start < fetched_start || end > fetched_end || len == 0 {
        return Err(RangeFault::Corrupt);
    }
    let group = chunk_group_bytes.max(CHUNK_LEN);
    if n <= group {
        if fetched_start != 0 || fetched_end != n || hash_tree(fetched).as_bytes() != root {
            return Err(RangeFault::Corrupt);
        }
    } else {
        if tree.len() != outboard_tree_bytes(n, group) {
            return Err(RangeFault::TreeUnusable);
        }
        let mut cursor = 0usize;
        verify_subtree_slice(
            fetched,
            fetched_start,
            tree,
            &mut cursor,
            0,
            n,
            root,
            true,
            start,
            end,
            group,
        )?;
    }
    let local_start = start - fetched_start;
    slice(fetched, local_start, end - start, "verified range").map_err(|_| RangeFault::Corrupt)
}

/// Verifies and returns the bytes of `object` for the range `[start, start+len)` against `root`. Uses the object's
/// outboard tree when it has a sound one — reading only the bytes in range — and otherwise verifies the whole content.
/// Returns an error, and serves nothing, when the bytes cannot be proven.
pub fn verify_object_range<'a>(
    object: &'a [u8],
    root: &[u8; blake3::OUT_LEN],
    start: u64,
    len: u64,
    chunk_group_bytes: usize,
    content_magic: &[u8; 4],
) -> Result<&'a [u8], CodecError> {
    let group = chunk_group_bytes.max(CHUNK_LEN);
    if let Ok(parsed) = parse_outboard_object(object, content_magic) {
        let content = parsed.content;
        if let Some(tree) = parsed.tree {
            match verify_range(content, tree, root, start, len, group) {
                Ok(bytes) => return Ok(bytes),
                Err(RangeFault::Corrupt) => {
                    return Err(CodecError::Blake3Mismatch {
                        scope: "verified range",
                    });
                }
                Err(RangeFault::TreeUnusable) => {}
            }
        }
        return verify_whole(content, root, start, len);
    }
    // The trailer is damaged, so the split it declares cannot be trusted — but the trailer is outboard metadata, not
    // content, and BLAKE3 over the content is the authoritative check either way. Recover the content boundary from the
    // object's own length (the tree's size is a function of the content's), and fall back to the whole object for a
    // tree-less object whose last bytes happen to look like a trailer. Intact content stays readable through both.
    let boundary = derived_content_end(object.len(), group);
    for candidate in boundary.into_iter().chain(std::iter::once(object.len())) {
        if let Some(content) = object.get(..candidate)
            && let Ok(bytes) = verify_whole(content, root, start, len)
        {
            return Ok(bytes);
        }
    }
    Err(CodecError::Blake3Mismatch { scope: "whole content" })
}

/// Recovers the content length of a tree-carrying object from the object's total length alone, without reading the
/// trailer that declares it. An object is `content + tree + trailer`, and the tree's size is fixed by the content's
/// length, so `total(len)` below rises strictly with the content length and the one content length that produces this
/// object can be found by bisection. Returns `None` when no content length yields exactly `object_len`.
fn derived_content_end(object_len: usize, group: usize) -> Option<usize> {
    let total = |content_len: usize| content_len + node_count(content_len, group) * NODE_LEN + TREE_TRAILER_LEN;
    let (mut low, mut high) = (0usize, object_len.checked_sub(TREE_TRAILER_LEN)?);
    while low <= high {
        let mid = low + (high - low) / 2;
        match total(mid).cmp(&object_len) {
            std::cmp::Ordering::Equal => return Some(mid),
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid.checked_sub(1)?,
        }
    }
    None
}

/// The refusing fallback: verify the whole content against the root, then return the requested range. No byte is
/// served unless the whole content matches `root`.
fn verify_whole<'a>(
    content: &'a [u8],
    root: &[u8; blake3::OUT_LEN],
    start: u64,
    len: u64,
) -> Result<&'a [u8], CodecError> {
    if blake3::hash(content).as_bytes() != root {
        return Err(CodecError::Blake3Mismatch { scope: "whole content" });
    }
    let start = usize::try_from(start).map_err(|_| CodecError::Truncated { what: "range start" })?;
    let len = usize::try_from(len).map_err(|_| CodecError::Truncated { what: "range length" })?;
    slice(content, start, len, "verified range")
}

/// Hashes `content[lo..hi]` to its non-root chaining value and writes its interior nodes in pre-order into the exact
/// output span reserved for this subtree. Disjoint child spans run in parallel without locks or intermediate vectors.
fn subtree_nodes_into(content: &[u8], lo: usize, hi: usize, group: usize, out: &mut [u8]) -> [u8; blake3::OUT_LEN] {
    if hi - lo <= group {
        debug_assert!(out.is_empty());
        return leaf_cv(content, lo, hi);
    }
    let left_len = left_subtree_len((hi - lo) as u64) as usize;
    let mid = lo + left_len;
    let (node, children) = out.split_at_mut(NODE_LEN);
    let left_tree_len = outboard_tree_bytes(mid - lo, group);
    let (left_out, right_out) = children.split_at_mut(left_tree_len);
    let (left_cv, right_cv) = rayon::join(
        || subtree_nodes_into(content, lo, mid, group, left_out),
        || subtree_nodes_into(content, mid, hi, group, right_out),
    );
    node[..blake3::OUT_LEN].copy_from_slice(&left_cv);
    node[blake3::OUT_LEN..].copy_from_slice(&right_cv);
    merge_subtrees_non_root(&left_cv, &right_cv, Mode::Hash)
}

/// Walks from one tree node down to the chunk groups overlapping the requested range, verifying each node against the
/// chaining value its parent promised (the root for the top node) and each overlapping leaf against its own.
#[expect(
    clippy::too_many_arguments,
    reason = "one recursive walk carries the tree cursor, the subtree bounds, and the query range"
)]
fn verify_subtree(
    content: &[u8],
    tree: &[u8],
    cursor: &mut usize,
    lo: usize,
    hi: usize,
    expected: &[u8; blake3::OUT_LEN],
    is_root: bool,
    range_start: usize,
    range_end: usize,
    group: usize,
) -> Result<(), RangeFault> {
    if hi - lo <= group {
        if lo < range_end && range_start < hi {
            let cv = leaf_cv(content, lo, hi);
            if &cv != expected {
                return Err(RangeFault::Corrupt);
            }
        }
        return Ok(());
    }
    let node = slice(tree, *cursor, NODE_LEN, "outboard node").map_err(|_| RangeFault::TreeUnusable)?;
    let left_cv = cv_at(node, 0)?;
    let right_cv = cv_at(node, blake3::OUT_LEN)?;
    *cursor += NODE_LEN;
    let actual = if is_root {
        *merge_subtrees_root(&left_cv, &right_cv, Mode::Hash).as_bytes()
    } else {
        merge_subtrees_non_root(&left_cv, &right_cv, Mode::Hash)
    };
    if &actual != expected {
        return Err(RangeFault::TreeUnusable);
    }
    let mid = lo + left_subtree_len((hi - lo) as u64) as usize;
    if lo < range_end && range_start < mid {
        verify_subtree(
            content,
            tree,
            cursor,
            lo,
            mid,
            &left_cv,
            false,
            range_start,
            range_end,
            group,
        )?;
    } else {
        *cursor += node_count(mid - lo, group) * NODE_LEN;
    }
    if mid < range_end && range_start < hi {
        verify_subtree(
            content,
            tree,
            cursor,
            mid,
            hi,
            &right_cv,
            false,
            range_start,
            range_end,
            group,
        )?;
    } else {
        *cursor += node_count(hi - mid, group) * NODE_LEN;
    }
    Ok(())
}

/// [`verify_subtree`] over a chunk-group-expanded content slice. Only overlapping leaves touch `fetched`; every other
/// subtree is skipped using the authenticated pre-order proof geometry.
#[expect(
    clippy::too_many_arguments,
    reason = "one recursive walk carries proof, fetched-slice, subtree, and requested-range geometry"
)]
fn verify_subtree_slice(
    fetched: &[u8],
    fetched_start: usize,
    tree: &[u8],
    cursor: &mut usize,
    lo: usize,
    hi: usize,
    expected: &[u8; blake3::OUT_LEN],
    is_root: bool,
    range_start: usize,
    range_end: usize,
    group: usize,
) -> Result<(), RangeFault> {
    if hi - lo <= group {
        if lo < range_end && range_start < hi {
            let local_lo = lo.checked_sub(fetched_start).ok_or(RangeFault::Corrupt)?;
            let local_hi = hi.checked_sub(fetched_start).ok_or(RangeFault::Corrupt)?;
            let bytes = fetched.get(local_lo..local_hi).ok_or(RangeFault::Corrupt)?;
            let mut hasher = blake3::Hasher::new();
            hasher.set_input_offset(lo as u64);
            hasher.update(bytes);
            if &hasher.finalize_non_root() != expected {
                return Err(RangeFault::Corrupt);
            }
        }
        return Ok(());
    }
    let node = slice(tree, *cursor, NODE_LEN, "outboard node").map_err(|_| RangeFault::TreeUnusable)?;
    let left_cv = cv_at(node, 0)?;
    let right_cv = cv_at(node, blake3::OUT_LEN)?;
    *cursor += NODE_LEN;
    let actual = if is_root {
        *merge_subtrees_root(&left_cv, &right_cv, Mode::Hash).as_bytes()
    } else {
        merge_subtrees_non_root(&left_cv, &right_cv, Mode::Hash)
    };
    if &actual != expected {
        return Err(RangeFault::TreeUnusable);
    }
    let mid = lo + left_subtree_len((hi - lo) as u64) as usize;
    if lo < range_end && range_start < mid {
        verify_subtree_slice(
            fetched,
            fetched_start,
            tree,
            cursor,
            lo,
            mid,
            &left_cv,
            false,
            range_start,
            range_end,
            group,
        )?;
    } else {
        *cursor += node_count(mid - lo, group) * NODE_LEN;
    }
    if mid < range_end && range_start < hi {
        verify_subtree_slice(
            fetched,
            fetched_start,
            tree,
            cursor,
            mid,
            hi,
            &right_cv,
            false,
            range_start,
            range_end,
            group,
        )?;
    } else {
        *cursor += node_count(hi - mid, group) * NODE_LEN;
    }
    Ok(())
}

/// The chaining value of one leaf: `content[lo..hi]` hashed as a non-root subtree at its offset in the whole file.
fn leaf_cv(content: &[u8], lo: usize, hi: usize) -> [u8; blake3::OUT_LEN] {
    let data = content.get(lo..hi).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.set_input_offset(lo as u64);
    hasher.update(data);
    hasher.finalize_non_root()
}

/// Reads a [`blake3::OUT_LEN`]-byte chaining value out of a node at `offset`.
fn cv_at(node: &[u8], offset: usize) -> Result<[u8; blake3::OUT_LEN], RangeFault> {
    let bytes = slice(node, offset, blake3::OUT_LEN, "chaining value").map_err(|_| RangeFault::TreeUnusable)?;
    bytes.try_into().map_err(|_| RangeFault::TreeUnusable)
}

/// How many interior nodes a subtree of `len` content bytes has, so a walk that does not descend into it can step past
/// its pre-order nodes.
fn node_count(len: usize, group: usize) -> usize {
    if len <= group {
        return 0;
    }
    let left_len = left_subtree_len(len as u64) as usize;
    1 + node_count(left_len, group) + node_count(len - left_len, group)
}

#[cfg(test)]
#[path = "test/integrity.rs"]
mod tests;
