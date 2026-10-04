//! The small value types that describe how a file backend makes bytes durable and which fast paths the host can take.
//!
//! These are deliberately backend-neutral: the same [`Atomicity`] and [`IoCapabilities`] describe a journal shard, an
//! object commit, or a cache fill, so every consumer reads durability the same way.

use super::constant::FRAME_ALIGNMENT;

/// Names one append-only target a [`crate::file::api::BlockStore`] writes to — a journal shard, or any other
/// append-only file the backend owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockTarget(pub u64);

/// One contiguous run of bytes in a stored object, as a [`crate::file::api::RangeSource`] is asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub len: u64,
    /// Measured from the first byte of the object.
    pub offset: u64,
}

/// How a backend turns a write into a durable write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityMode {
    /// Buffered filesystem writes: durability requires the sync barrier.
    Buffered,
    /// Direct I/O write semantics (bypassing the OS page cache): completion of the write plus its linked flush implies
    /// the bytes reached the media.
    DirectIo,
}

/// What a backend learned about a target's untorn-write guarantees, recorded once at startup. The all-zero answer is
/// valid and correct: durability then rests on whole-frame BLAKE3 validation plus torn-tail truncation, and
/// atomic-write support only lowers torn-write recovery cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Atomicity {
    /// max(4096, awupf_bytes) rounded to an allowed frame size.
    pub atomic_frame_multiple: u32,
    /// NVMe Atomic Write Unit Power Fail in bytes; zero when unknown.
    pub awupf_bytes: u32,
    pub durability_mode: DurabilityMode,
    /// Untorn-write unit from the NVMe Identify Namespace AWUN field; zero when unreported. Untorn claims require AWUN
    /// >= 1 confirmed.
    pub untorn_write_bytes: u32,
}

impl Atomicity {
    /// The conservative no-capability answer for a buffered target.
    pub const fn buffered_unprobed() -> Self {
        Self {
            atomic_frame_multiple: FRAME_ALIGNMENT as u32,
            awupf_bytes: 0,
            durability_mode: DurabilityMode::Buffered,
            untorn_write_bytes: 0,
        }
    }
}

/// Which kernel and device fast paths a host supports, discovered at startup. A fast path is taken only when its field
/// reports support; otherwise the portable fallback runs, with byte-for-byte identical results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoCapabilities {
    /// Largest single write the device publishes untorn, in bytes (0 = unknown).
    pub atomic_write_unit_bytes: u32,
    /// Alignment a direct-I/O read/write must meet, in bytes (0 = no direct I/O).
    pub direct_io_alignment_bytes: u32,
    /// The host can preallocate and zero a write-once extent in one call — the `fallocate` write-zeroes mode
    /// (Linux 6.17+) — so a later write lands in already-materialised, already-zeroed blocks instead of allocating on
    /// write. When false the portable fallback is plain preallocation or ordinary allocation-on-write, with identical
    /// durable bytes. Discovered by a probe `fallocate` call, not by `statx`.
    pub efficient_extent_zeroing: bool,
    /// Block size larger than a page the filesystem reports (0 = none). Filesystem-neutral: any mounted filesystem that
    /// reports a block size above the page size qualifies (XFS on Linux 6.12+, EXT4 on 6.19+), so a capable EXT4 host
    /// takes the same fast path a capable XFS host does.
    pub large_block_bytes: u32,
    /// The device can catch a sector corrupted on the media itself: the namespace is formatted with NVMe end-to-end
    /// protection information whose per-sector guard tag is CRC-64/NVME, and the kernel exposes the Linux 6.14 per-IO
    /// integrity attributes on io_uring read/write so userspace can attach a guard tag on write and have the device
    /// verify it on read. A non-authoritative precheck — BLAKE3 stays the admission authority. Reported `false` in
    /// production today: the io_uring write path does not yet attach the per-I/O guard tag, so a real device-verified
    /// precheck is not performed. The test-only `with_nvme_pi` override exercises the in-memory software precheck.
    pub nvme_pi: bool,
    /// The host can attach a peer's NVMe namespace over NVMe-over-Fabrics and read it through the accelerated block
    /// path. True only when the `nvmeof` feature is built in and the kernel fabrics control device is present;
    /// otherwise the caller falls back to the two-sided fill then the durable tier.
    pub nvmeof_initiator: bool,
    /// The host can read without leaving a copy in the page cache. Reported `false` in production today: the block read
    /// path uses ordinary buffered positioned reads, so no O_DIRECT (uncached) read path exists yet even where the
    /// direct-I/O alignment is known.
    pub uncached_reads: bool,
    /// The target sits on zoned media that supports zone-append placement.
    pub zoned_placement: bool,
}

impl IoCapabilities {
    /// The conservative profile: no fast path, every operation portable.
    pub const fn portable() -> Self {
        Self {
            atomic_write_unit_bytes: 0,
            direct_io_alignment_bytes: 0,
            efficient_extent_zeroing: false,
            large_block_bytes: 0,
            nvme_pi: false,
            nvmeof_initiator: false,
            uncached_reads: false,
            zoned_placement: false,
        }
    }
}

impl Default for IoCapabilities {
    fn default() -> Self {
        Self::portable()
    }
}
