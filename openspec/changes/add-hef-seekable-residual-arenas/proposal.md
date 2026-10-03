Status: Proposed (2026-08-19)

## Why

A rewritten or compacted granule stores its variant residual arena compressed
with Zstd-3 over the whole arena. The per-row offsets index the *inflated*
bytes, so reading one row's residual means inflating the entire arena — every
row the granule holds — and then holding it resident so the next read does not
pay again. On an 8192-row granule that is hundreds of kilobytes of decompression
and resident memory to answer a single point lookup, and it is charged against
the `entity_id` point-lookup targets in `hef-benchmarks-and-acceptance-gates`.

The rest of the format already refuses this trade. "Adaptive per-block encoding
selection" prefers pipelines that decode without full decompression, the
granular deflate family exists precisely so a random-access block can still be
compressed, and whole-block LZ4/Zstd are excluded from random-access blocks
because their compressed bytes are not addressable by row. The residual arena is
the one place left where a cold granule pays whole-block inflation for a
single-row read.

The Zstandard Seekable Format removes the trade without changing the codec:
compress the arena as a series of independent frames and append a seek table
(itself a skippable frame) recording where each frame starts in both the
compressed and the decompressed stream. A reader resolves the byte range it
wants to the frames covering it and inflates only those. The stored bytes stay
an ordinary Zstandard stream — a decoder that knows nothing of the seekable
format concatenates the frames and skips the table, recovering the arena byte
for byte.

## What Changes

**A cold residual arena is stored as seekable Zstd-3 frames.** A rewrite or
compaction compresses the arena into frames of at most 64 KiB of plaintext each,
followed by the seek table, and records the choice in the granule's payload
index as a new `residual_compression` discriminant. The compression level, the
lifecycle rule that selects it (fresh publications stay uncompressed for
offset-jump access), and the "only when it shrinks the arena" rule are all
unchanged.

**A point read inflates the frame it needs.** The reader resolves a row's
`(offset, len)` against the seek table, inflates the covering frame, and caches
that frame — not the arena — under the same byte budget and LRU discipline the
decoded-block caches already use. A value straddling a frame boundary is
decompressed in one pass over exactly the frames it covers.

**Whole-arena `Zstd3` arenas stay readable.** The discriminant is read exactly as
today, so every file already written keeps working; no writer emits it any more.
A reader that does not know the new discriminant already refuses the file — the
`residual_compression` domain is closed and an unknown value is a structural
error — so no required-feature flag is added.

Nothing else moves: no new file shape beyond the arena's own bytes, no operator
config key, no change to query results, ordering, visibility, checksums, or
public output. The per-frame bytes are ordinary Zstandard frames, so the
optional QATZip decompression path applies to them unchanged.
