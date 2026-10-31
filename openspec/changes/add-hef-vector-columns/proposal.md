Status: Proposed (2026-10-30)

## Why

Harana Matrix stores message embeddings for semantic search and for Envoy's memory without adding a database. The
embeddings go into HEF files that a user's own devices create, sync and search; the homeserver only stores ciphertext.
HEF had reserved embedding column ids and a per-row offset index, but no vector column type: no dimension, element
type, metric or model in the footer, no per-row quantization factors, no way to find the 1-bit, int8 and float copies
of the same vectors, no top-k kernel, and no whole-file encryption that still allows ranged reads.

## What Changes

- Add `ColumnKind::Vector` (code 6) and a `VECTOR_COLUMNS` footer section holding one `VectorColumnMeta` per vector
  column: dimension, element type (`F16`, `F32`, `I8`, `U1`), metric (cosine on normalized vectors, dot, L2), model id
  and model digest, and the logical embedding column it represents. Guarded by the new `VECTOR_COLUMNS` required
  feature bit, so an older reader refuses such a file. Footers without vector columns stay byte-identical. (H1, H3)
- Add the vector segment file (`hef::vector`): a HEF file of one batch of embeddings with their event id, room,
  sender and timestamp columns. Rows are stored row-major, uncompressed, `row × width`, with an inline per-row
  quantization trailer (int8: scale and offset; 1-bit: norm and RaBitQ alignment). Every vector block starts on a
  64-byte boundary. (H2, H5, H7)
- Seal every block and the footer under a random per-file data key with XChaCha20-Poly1305, each block on its own
  nonce bound to its position; only magic, version, nonce prefix and the wrapped data key are in the clear. The caller
  supplies the wrapping key. Rewrapping replaces only the header. (H10)
- A sealed manifest in the footer: model, window (room, first and last event), row count, producer device, the
  representations stored and a BLAKE3 content hash of the sealed blocks. (H8)
- A top-k scan kernel (Hamming over 1-bit, dot or L2 over int8, f16 and f32) that applies a row filter during the scan,
  and a multi-segment search that prunes segments and granules by room, sender and time statistics, scans the 1-bit
  copy and re-scores candidates with the finest copy. Reads and scans refuse a different model or metric. (H6, H12)
- `segments_to_reembed` lists every file built with a model other than the current one. (H11)

- Prefix reads: the leading dimensions of one f16 or f32 row, for models whose prefix is itself an embedding. (H4)
- A per-segment centroid in the sealed footer (mean vector, normalized for cosine) and `nearest_segments`, which ranks
  segments by it so a search can scan the likeliest first. Footers written without one still open. (H9)
- `merge_segments` replaces many small segments with one, re-encoding each file's finest stored vectors. (H13)
- The read and write path builds and runs on WebAssembly: AES-256-GCM falls back to a software implementation there
  (same ciphertext), and randomness comes from the host. (H14)

Deferred: ANN indexes, vector compression codecs.

## Impact

- New capability `hef-vector-columns`; `hef-file-layout` gains the vector kind, footer section and feature bit.
- Deletion vectors are unchanged; vector segment files are replaced, never patched.
