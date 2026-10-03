# Tasks — incorporate-liquid-cache-learnings

> One spec delta: FSST range predicates answered from an optional, sampler-earned
> prefix-key stream, with only whole-width ties decompressed. Then the write path,
> the read path, and the equivalence oracle that pins the fast answer to a full
> decode. The disposition of every other LiquidCache mechanism — including the two
> deliberately not taken — is the register in `design.md` §1.

## hef-encodings-and-compression — spec delta

- [x] MODIFY "Compressed-data string predicates": FSST range answered from an optional
      prefix-key stream (leading uncompressed bytes to a descriptor-recorded width, plus a
      saturating length); only rows tying across the whole width with both values continuing
      past it are decompressed; the stream is a sampled candidate recorded in the page's
      encoding descriptor; a block without it declines range to full decode as before.
      Implements `hef-encodings-and-compression` — "Compressed-data string predicates".

## Code — write path (req: hef-encodings-and-compression "Compressed-data string predicates", "Adaptive per-block encoding selection", "Self-describing per-page encoding descriptor")

- [ ] Prefix-key entry type and its stream encoding in `storage/src/hef/encoding/string_column.rs`:
      leading uncompressed bytes to a fixed width plus the value's byte length, saturating at
      the sentinel that means "continues past what the evaluator can decide from"
- [ ] Name the width and the length sentinel in `storage/src/hef/encoding/constant.rs`; no bare
      literals at the use sites
- [ ] Emit the stream as a sampled candidate in the FSST branch of `encode_block`: measure, on
      the existing sample, both the stream's size against the block and the share of rows it
      would leave undecided, and emit only when it wins under the adaptive selection rules
- [ ] Record presence and width in the per-page encoding descriptor
      (`storage/src/hef/encoding/descriptor.rs`) so a reader knows from the page alone

## Code — read path (req: hef-encodings-and-compression "Compressed-data string predicates")

- [ ] Range arm for `Transform::FsstString` in `storage/src/hef/encoding/predicate.rs`, replacing
      today's `FormatError::Structural` decline: compare each key against the bound, decide on
      first difference inside the width, and decide the agree-across-the-width case from the
      lengths — a value ending inside the width is known in full, and a proper prefix sorts first
- [ ] Collect the undecided rows and resolve exactly those by decompressing them and comparing in
      full; assert nothing already decided is decompressed
- [ ] Keep the decline when the page carries no stream, so an old page behaves exactly as today
- [ ] Two-sided ranges evaluate both bounds against the same key pass, not two passes
- [ ] Nulls are never selected, on the decided and the resolved rows alike

## Code — provenance (req: implementation-toolchain "Dependency policy enforces the pinned and excluded library lists")

- [ ] Provenance note for the ported mechanism in the style of `query/src/expr_eval/VENDORING.md`
      — upstream repository, commit `746d639`, the file the mechanism comes from, and what
      changed in the port — recorded at the port site, not as a new vendored tree. No dependency
      is added and no upstream source is copied; the note records where the idea came from

## Tests (req: hef-encodings-and-compression "Compressed-data string predicates")

- [ ] Equivalence oracle: for a seeded corpus of FSST string blocks, every range predicate
      answered from the stream equals decode-then-filter row for row, nulls never selected
- [ ] Ambiguity edge cases as their own tests: values shorter than the width, values exactly the
      width, values differing only past the width, a value that is a proper prefix of the bound,
      the bound shorter than the width, and lengths at the saturating sentinel
- [ ] A block written without the stream still declines range and decodes, with results identical
      to a block that carries it
- [ ] Encoder decision snapshot: a shared-prefix corpus (values agreeing past the width) records
      the stream declined; a high-cardinality corpus records it emitted
- [ ] Extend the encoder oracle fuzz target in `server/fuzz` to cover range predicates over blocks
      with and without the stream

## Benchmarks (req: hef-benchmarks-and-acceptance-gates "Acceptance gates are mechanically enforced by the committed harness")

- [ ] Range-on-FSST leg in `crates/format-benchmark`: rows decompressed and walltime, with and
      without the stream, across selectivities and across identifier / email / URL corpora — the
      measurement that settles the key width (proposal Open Question 1)
- [ ] Measure what a decoded-column residency tier would save on HEF — repeat-query decode cost
      against page-resident bytes — as the evidence for or against proposing one at all
      (`design.md` §5). No requirement is proposed until this number exists

## Verification

- [ ] `openspec validate incorporate-liquid-cache-learnings --strict` green
- [ ] Applied after `incorporate-vortex-learnings` archives, or rebased onto whatever text that
      change archived (`design.md` §6)
- [ ] `cargo fmt` clean and `cargo test -p storage --features write --lib` green
