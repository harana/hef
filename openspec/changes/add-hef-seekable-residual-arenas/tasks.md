# Tasks — add-hef-seekable-residual-arenas

> A cold granule's residual arena is stored as independent Zstd-3 frames with a
> seek table, so a point read inflates the frame it needs instead of the whole
> arena. The level, the lifecycle rule that admits compression, and the
> reconstructed payload bytes are unchanged; whole-arena `Zstd3` arenas stay
> readable. Spec delta lands with the code.

## hef-encodings-and-compression — the MODIFIED requirement

- [x] MODIFY "Index, bitmap, and payload compression families": a cold/rewritten
      residual arena stores its Zstd-3 bytes as seekable frames plus a seek
      table; a reader inflates only the frames covering a row's span; the stored
      bytes stay an ordinary Zstandard stream; the residual-compression domain
      is closed and an unknown declaration refuses the file; arenas written as
      whole-arena Zstd-3 stay readable.

## Implementation

- [x] `encoding/seekable_zstd.rs`: `compress` (64 KiB plaintext frames at level
      3, seek table appended), `seek_table`, and `decompress_range` with a
      decode-bomb ceiling and a short-decode refusal.
- [x] `layout/footer.rs`: `ResidualCompression::ZstdSeekable = 2`, encoded and
      decoded with the closed domain unchanged.
- [x] `writer/build.rs`: `compress_residual` writes the seekable form when it
      shrinks the arena; an arena it does not shrink stays uncompressed.
- [x] `layout/reader.rs`: `residual_bytes` resolves the span through the seek
      table and inflates the covering frame; the frame cache is keyed
      `(granule_id, frame)` under the existing budget and LRU; a straddling
      value decompresses in one pass, uncached; the per-granule seek table is
      parsed once.
- [x] `pinned-crates.toml`: `zeekstd` introduced — its frames and seek table are
      stored bytes every reader must keep decoding.

## Verification

- [x] Frames span the arena and every frame's range decompresses to its own
      plaintext; a straddling range and a whole-arena range match the plaintext.
- [x] A plain `zstd` decoder recovers the whole arena from the stored bytes.
- [x] A range past the arena, an inverted range, and one past the per-call
      ceiling are refused rather than padded; bytes with no seek table refuse.
- [x] A rewritten file's payloads reconstruct identically to the uncompressed
      fresh publication, row for row.
- [x] A point read into a multi-frame arena holds at most one frame, not the
      arena it came from.
