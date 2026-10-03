//! Fixed values the block encoder and decoder are built around — sampling, cascade thresholds, and format bounds —
//! gathered here so every consumer reads the same numbers.

/// The constant behind the branch-free integer→float conversion in [`super::decode_alp`]'s vectorized reconstruction:
/// `1.5 × 2^52`. Adding an integer `|i| ≤ 2^51` to this value's bits and subtracting the value back yields exactly
/// `i as f64`, because every intermediate lands where an f64's spacing is 1.
pub const ALP_CONVERT_MAGIC: f64 = 6_755_399_441_055_744.0;

/// Widest bit-packed ALP stream whose zigzag integers stay within `±2^51`, the range [`ALP_CONVERT_MAGIC`]'s
/// branch-free conversion reproduces bit-exactly; a wider (forged or pathological) stream falls back to the scalar
/// `as f64` reference conversion.
pub const ALP_EXACT_CONVERT_MAX_WIDTH: u32 = 52;

/// [`super::encode_alp`] rejects a block for ALP once the sampled exception count reaches this fraction (1/8, 12.5%)
/// of the sample; above that, byte-stream-split or plain floats encode smaller.
pub const ALP_MAX_EXCEPTION_SAMPLE_DIVISOR: usize = 8;

/// [`super::alp_try`]'s overflow guard: a value scaled by an ALP power is rejected once its magnitude reaches this,
/// since `i64::MAX` is about `9.22e18` and the rounded scaled value needs headroom below that to round-trip safely.
pub const ALP_MAX_SCALED_MAGNITUDE: f64 = 9.0e18;

/// Sentinel [`super::encode_alp_rd`]'s split kernel writes for a value whose left part is missing from the dictionary;
/// a later pass turns it into code 0 plus an exception entry. Can never collide with a real code, which is bounded by
/// [`ALP_RD_MAX_DICT_LEN`].
pub const ALP_RD_CODE_MISS: u64 = u64::MAX;

/// Most entries the ALP-RD left-part dictionary may hold: 8 patterns fit a 3-bit code, the sweet spot the ALP paper
/// pins for doubles whose sign/exponent/leading-mantissa bits cluster.
pub const ALP_RD_MAX_DICT_LEN: usize = 8;

/// Largest right-part bit width the ALP-RD split search tries — one below the full value, so at least one bit is left
/// for the dictionary-coded part.
pub const ALP_RD_MAX_RIGHT_WIDTH: u32 = 63;

/// Smallest right-part bit width the ALP-RD split search tries, so the dictionary-coded left part never exceeds the
/// 16 bits its stored `u16` patterns can carry.
pub const ALP_RD_MIN_RIGHT_WIDTH: u32 = 48;

/// Marker in the ALP exponent byte's position that flags the per-vector escape form: the real exponent follows, then
/// the escaped-vector list, and each escaped vector's values are stored raw after the packed words. Can never collide
/// with a real exponent index, which is bounded by the ALP power table's length.
pub const ALP_VECTOR_ESCAPE_SENTINEL: u8 = 255;

/// Denominator of the `DecodeOptimized` cascade threshold (paired with [`DECODE_OPTIMIZED_THRESHOLD_NUM`]): a
/// candidate is kept only when its size is at most 3/4 of the original — a saving of at least 25%.
pub const DECODE_OPTIMIZED_THRESHOLD_DEN: usize = 4;

/// Longest dictionary entry (bytes) the FSST inner cascade level will consider for a dictionary string block; a longer
/// entry keeps the value stream plain so a single huge value cannot make every dictionary materialization pay an
/// unbounded decompress.
pub const DICTIONARY_FSST_MAX_VALUE_LEN: usize = 4096;

/// Numerator of the `DecodeOptimized` cascade threshold. See [`DECODE_OPTIMIZED_THRESHOLD_DEN`].
pub const DECODE_OPTIMIZED_THRESHOLD_NUM: usize = 3;

/// [`super::choose_string_transform`] picks dictionary encoding only when
/// distinct values are at most this fraction (1/4, 25%) of the sample; above that, FSST or raw wins.
pub const DICTIONARY_MAX_CARDINALITY_DIVISOR: usize = 4;

/// fsst-rs's lossy perfect-hash multiplier (`fsst::lossy_pht::HASH_MULTIPLIER`), pinned with fsst-rs 0.5.11. Used by
/// [`super::fsst_lossy_pht_slot`] to reproduce the slot a symbol lands in inside the crate's lossy hash table.
pub const FSST_LOSSY_PHT_HASH_MULTIPLIER: u64 = 2_971_215_073;

/// fsst-rs's lossy perfect-hash shift (`fsst::lossy_pht::HASH_SHIFT`), pinned with fsst-rs 0.5.11. See
/// [`FSST_LOSSY_PHT_HASH_MULTIPLIER`].
pub const FSST_LOSSY_PHT_HASH_SHIFT: u32 = 15;

/// [`super::choose_string_transform`] picks FSST for a high-cardinality sample only when the average value length is
/// at or under this many bytes; longer values are stored raw instead.
pub const FSST_MAX_AVERAGE_VALUE_LEN: usize = 64;

/// The most symbols an FSST table may declare before [`super::read_fsst_compressor`] refuses it — the ceiling
/// fsst-rs itself would abort on.
pub const FSST_MAX_SYMBOL_COUNT: usize = 255;

/// [`super::choose_string_transform`] picks FSST only when the sample has at least this many values; below that,
/// training a symbol table rarely pays for itself and the sample falls back to raw.
pub const FSST_MIN_SAMPLE_COUNT: usize = 16;

/// Bound on what an FSST block's per-value keys (see [`super::sidecar`]) may cost: they are stored only when their
/// bytes are at most this fraction (1/2) of the plaintext a filter avoids decompressing by reading them. A block of
/// short values therefore keeps the plain layout — decompressing it whole is cheap enough that the keys would be
/// pure overhead — while the long-value blocks, where the fallback hurts, carry them.
pub const FSST_VALUE_KEYS_MAX_PLAINTEXT_DIVISOR: usize = 2;

/// The largest decode buffer a thread keeps between block decodes, in values (8 MiB at 8 bytes each). Every page a
/// granule holds fits well inside it, so the buffer is retained across a whole scan; a wider one-off decode hands
/// back a buffer past the bound, which is dropped rather than held for the life of the thread.
pub const MAX_RETAINED_SCRATCH_VALUES: usize = 1 << 20;

/// Largest plaintext arena buffer a thread keeps between block searches, in bytes. Four mebibytes covers a granule of
/// ordinary free text; a block that needed more hands its buffer back to the allocator rather than pinning it.
pub const MAX_RETAINED_ARENA_BYTES: usize = 4 << 20;

/// Largest trailing-stage output buffer a thread keeps between block decodes, in bytes. Four mebibytes holds any
/// block a granule of this format's default shape inflates to, so the buffer is retained across a whole scan; a block
/// that needed more hands its buffer back to the allocator rather than pinning it for the life of the thread.
pub const MAX_RETAINED_INFLATE_BYTES: usize = 4 << 20;

/// The most bytes one FSST symbol — and so one stored code — expands to.
pub const FSST_MAX_SYMBOL_BYTES: usize = 8;

/// Bytes one stored [`super::sidecar::PrefixKey`] occupies: its prefix bytes plus the length byte.
pub const PREFIX_KEY_BYTES: usize = PREFIX_KEY_PREFIX_LEN + 1;

/// How many leading bytes of a value a [`super::sidecar::PrefixKey`] keeps in the clear. Seven, so the prefix and its
/// length byte together are eight — the width that keeps a block's keys a flat run and still separates every pair of
/// values except those agreeing on their first seven bytes.
pub const PREFIX_KEY_PREFIX_LEN: usize = 7;

/// Decode-bomb guard on an RLE block's total decoded value count (not its declared run count), so a run of forged runs
/// cannot make [`super::decode_rle`] amplify a small input into an unbounded allocation.
pub const MAX_RLE_EXPANSION_VALUES: usize = 64 * 1024 * 1024;

/// Decoded column bytes per granule below which [`super::predicate::scan_in_parallel`] keeps a multi-granule scan on
/// the calling thread. At the gigabyte-per-second rate a block inflates and searches, 64 KiB is a few tens of
/// microseconds of work — about what handing a granule to a pool worker costs — so a smaller granule gains nothing
/// from fanning out, while a granule of ordinary free text (a few hundred kibibytes) clears it several times over.
pub const PARALLEL_SCAN_MIN_GRANULE_BYTES: u64 = 64 * 1024;

/// Fewest surviving granules [`super::predicate::scan_in_parallel`] fans out over: one granule has nothing to run
/// beside.
pub const PARALLEL_SCAN_MIN_GRANULES: usize = 2;

/// Denominator of the replay trip-wire bound (paired with [`REPLAY_TRIP_NUM`]): a replayed block is kept while its
/// encoded-to-raw ratio stays within 5/4 of its capture head's, and re-arms full selection past that — so a
/// distribution shift costs one sub-optimal block, never the rest of the column.
pub const REPLAY_TRIP_DEN: u64 = 4;

/// Numerator of the replay trip-wire bound. See [`REPLAY_TRIP_DEN`].
pub const REPLAY_TRIP_NUM: u64 = 5;

/// Divisor of the `SizeOptimized` cascade threshold: a candidate is kept only when it saves at least 1/20 (5%) of the
/// original bytes.
pub const SIZE_OPTIMIZED_SAVINGS_DIVISOR: usize = 20;

/// Number of evenly spaced contiguous runs the transform-selection sampler draws across a block larger than
/// [`TRANSFORM_SAMPLE_SIZE`], so the sample reflects the whole block rather than just its head. See
/// [`super::transform_sample`].
pub const TRANSFORM_SAMPLE_RUNS: usize = 8;

/// Fixed sample size the encoder draws to pick a winning transform without scanning every value: used for the u64
/// FOR/DELTA/RLE/plain trial, the ALP exponent search, the byte-stream-split-vs-plain float trial, and the
/// dictionary/FSST/raw string trial. Drawn as [`TRANSFORM_SAMPLE_RUNS`] evenly spaced runs spanning the block (see
/// [`super::transform_sample`]), never a leading prefix. Unrelated to [`super::FASTLANES_VECTOR`], which happens to
/// share the same value for a different reason (the FastLanes transpose width).
pub const TRANSFORM_SAMPLE_SIZE: usize = 1024;

/// Bytes the zstd trailing stage prepends to its compressed body: the uncompressed length a reader sizes its output
/// buffer from. Stored with the block, so a candidate's savings are measured against the body plus this prefix rather
/// than the body alone.
pub const ZSTD_LENGTH_PREFIX_BYTES: usize = 4;

/// Widest value range [`super::unpack_fastlanes_range_into`] reads value by value before falling back to unpacking
/// whole FastLanes vectors. A vector's sixteen-lane shift-and-mask kernel touches 1024 values however few are wanted,
/// so a narrow range — a point read most of all — is far cheaper addressed directly; the crossover sits well above
/// this bound, which is set low enough that neither side can lose.
pub const FASTLANES_SPARSE_MAX_VALUES: usize = 64;
