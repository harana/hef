# Design — add-hef-seekable-residual-arenas

## 0. What the cold arena costs today

`server/crates/storage/src/hef/writer/build.rs` — `compress_residual` frames a
rewritten granule's residual arena as `u32 uncompressed length | zstd bytes` at
level 3. `server/crates/storage/src/hef/layout/reader.rs` —
`residual_bytes` sees `ResidualCompression::Zstd3`, calls `inflated_residual`,
and gets back the whole arena: the per-row `(offset, len)` pairs index the
inflated bytes, so there is no smaller unit to inflate. The result is cached per
granule (`inflated_residuals`, byte-budgeted with `column_cache`) precisely
because inflating it again per row would be worse.

So a point read into a cold granule pays the whole arena's decompression and
holds the whole arena resident. Every other random-access path in the format
already refuses that shape: `apply_granular_deflate`
(`encoding/mod.rs`) will not put whole-block LZ4 or Zstd on a random-access
block at all — "their compressed bytes are not addressable by row" — and ships
the granular deflate family instead, 4 KiB granules indexed by a
`mini_block_directory`.

## 1. Why the seekable format rather than another mini-block directory

The deflate family's shape — fixed-size plaintext granules, a directory of
compressed lengths — would work here too, and `mini_block_directory` is already
written. Two things argue for the Zstandard Seekable Format instead:

- **The bytes stay a Zstandard stream.** Frames are concatenated and the seek
  table is a skippable frame, so any conforming decoder recovers the arena. A
  hand-rolled directory makes the arena readable only by this codebase, for a
  block that is otherwise plain zstd today.
- **The frame size that suits an arena is not the one that suits a column
  block.** Deflate's granules are 4 KiB because that is the IAA history window;
  at that size zstd's ratio advantage mostly disappears. An arena wants frames
  large enough to keep the ratio (64 KiB here) and a seek table sized in bytes
  per frame, which is what the format specifies.

`zeekstd` implements the format over `zstd-safe 7.2.4` — the exact version the
`zstd` crate already in the graph resolves to, so no second libzstd enters the
build.

## 2. Where the seek table lives

Appended to the arena's own bytes, not in the footer. The precedent is
`deflate`, whose mini-block directory rides inside the block it indexes: marks
are the authoritative directory for `(column, projection, granule)` extents, and
this is intra-extent layout below that. It also keeps the arena a self-contained
Zstandard stream, which the footer variant would not.

## 3. The frame is the cache unit

A per-row loop over a granule would decompress the same frame once per row if
only the requested span were inflated. So the reader inflates the whole covering
frame and caches it under `(granule_id, frame)` in the existing
`inflated_residuals` map — same budget, same LRU, same eviction accounting; a
`Zstd3` arena keeps its whole-arena entry under frame 0. A per-row loop then
pays one decompression per frame instead of one per row, and a point read holds
64 KiB where it used to hold the arena.

A value straddling a frame boundary is decompressed in one pass over exactly the
frames it covers and is not cached: it is rarer than a value inside one frame,
and it is not the span the next read will want.

The parsed seek table is held per granule for the reader's lifetime — every read
needs it to resolve an offset to a frame, and it is a few bytes per frame, the
same argument the text-token filter cache already makes for not evicting.

## 4. Compatibility

`ResidualCompression` is a closed domain decoded from one byte, and an
unrecognized value is already a structural refusal. That is exactly the
protection a required-feature flag would add, so no flag is introduced. The
`Zstd3` arm stays in the reader for files already written; no writer emits it.
