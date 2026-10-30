# Tasks — add-hef-vector-columns

- [x] Vector kind, `VECTOR_COLUMNS` footer section and required feature bit; footer round-trip per element type and
      metric; a footer without vector columns re-encodes byte-identical.
- [x] Writer: width, finiteness and cosine normalization checks; one aligned fixed-width block per granule; empty and
      single-row segments.
- [x] Reader: granule as Arrow `FixedSizeListArray`; single row at `row × width`; linked representation lookup.
- [x] Vector columns never appear in pruning statistics or public output (stats are kept only for non-vector columns).
- [x] Refuse reads and scans with a different model id, digest, dimension or metric.
- [x] Top-k kernel with the row filter applied during the scan; equals a brute-force reference.
- [x] Event id row key and sealed manifest with content hash.
- [x] Whole-file encryption with ranged reads; wrong key, tampered, swapped and truncated blocks fail.
- [x] Model-id invalidation: `segments_to_reembed`.
- [x] Benchmark 1-bit and int8 scans over 100k × 384 with and without a room filter (`benches/vector.rs`).

## Benchmark (2026-10-30, 4-core 2.1 GHz Xeon, one core, `cargo bench --bench vector -- --quick`)

Every scan decrypts its blocks inside the timed loop (AES-256-GCM, ~780 MB/s on this machine).

| Scan, top 100 of 100k × 384 | Estimate (plaintext) | Measured | Of which decryption |
|---|---|---|---|
| 1-bit, all rows | <1 ms | 12.6 ms | ~6 ms |
| 1-bit, one room in ten | — | 7.8 ms | ~6 ms |
| int8, all rows | ~3 ms | 93.9 ms | ~50 ms |
| int8, one room in ten | — | 82.0 ms | ~50 ms |

Decryption dominates and the scoring loops are not yet hand-vectorized; both are well inside an interactive budget
for one user's data, and the default search path scans 1-bit and re-scores only candidates in int8.
