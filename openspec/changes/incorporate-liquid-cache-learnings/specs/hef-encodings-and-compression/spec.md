## MODIFIED Requirements

### Requirement: Compressed-data string predicates
Dictionary string blocks SHALL assign codes in ascending sorted order of their
distinct values, so a code's numeric order matches its value's byte order. The
reader SHALL provide a string-predicate evaluator that answers common filters
directly from a block's compressed form, without rebuilding every row's text:

- For a dictionary block, it SHALL answer equality (`=`), inequality (`!=`), set
  membership (`IN`), and range (`<`, `<=`, `>`, `>=`, open or closed on either
  side) by resolving the predicate against the sorted dictionary and testing the
  stored codes.
- For an FSST block, it SHALL answer the equality class (`=`, `!=`, `IN`) by
  compressing the comparison value(s) with that block's symbol table and
  byte-comparing against the stored compressed values, decompressing nothing.
- For an FSST block, it SHALL answer prefix (`LIKE 'p%'`) and substring
  (`LIKE '%s%'`, `contains`) predicates by running a deterministic automaton,
  derived once per predicate from the comparison bytes and the block's symbol
  table, over the stored compressed code stream — one table transition per
  compressed byte, escapes included — decompressing nothing.
- For an FSST block that carries a **prefix-key stream**, it SHALL answer range
  (`<`, `<=`, `>`, `>=`, open or closed on either side) by comparing the stored
  keys against the comparison value, decompressing only the values the keys
  leave undecided.

FSST codes are not order-preserving, so a range predicate SHALL NOT be answered
from the compressed code bytes themselves. An FSST block MAY instead carry an
optional prefix-key stream that restores order comparison without decoding: one
entry per present value, holding that value's leading uncompressed bytes up to a
fixed width recorded in the page's encoding descriptor, together with the value's
byte length (saturating at a recorded sentinel that means "longer than the
evaluator can decide from"). Comparing an entry against the comparison value
SHALL decide the row whenever the two differ inside the stored width, or agree
across the stored width while at least one value ends inside it — because a value
that ends inside the stored width is known in full, and a value that is a proper
prefix of the other sorts before it. Only rows where both values agree across the
whole stored width and both are known to continue past it SHALL remain
undecided; the evaluator SHALL resolve exactly those rows by decompressing them
and comparing in full, and SHALL leave every other row decided without any
decompression.

The prefix-key stream SHALL be a sampled candidate under the adaptive selection
rules (Requirement: "Adaptive per-block encoding selection"), never statically
assigned and never selected by an operator config key: the writer SHALL emit it
only when its sample shows the stream earns its bytes — its size against the
block, and the share of rows it would leave undecided against the full decode it
replaces. Its presence and its width SHALL be recorded in the page's encoding
descriptor (Requirement: "Self-describing per-page encoding descriptor") so a
reader knows from the page alone whether the stream is there and how wide its
entries are. The stream SHALL carry no value a decode could not reproduce, so a
reader that ignores it decodes the same rows.

The evaluator SHALL be an optimisation, never a correctness dependency: a reader
SHALL always be able to decode the block and filter the values, and SHALL do so
whenever the block's encoding or the predicate is unsupported — including an
FSST range predicate on a block with no prefix-key stream, which SHALL decline
to full decode exactly as before. The evaluator's result SHALL be exact — every
selected row truly satisfies the predicate — and SHALL equal a full
decode-then-filter row for row, with null rows never selected.

#### Scenario: Dictionary codes preserve value order
- **WHEN** the writer encodes a dictionary string block
- **THEN** the codes are assigned in ascending sorted order of the distinct
  values, so comparing codes orders rows the same way as comparing their strings

#### Scenario: Dictionary equality and range answered from codes
- **WHEN** an `=`, `IN`, or range filter runs on a dictionary-encoded column
- **THEN** the evaluator resolves the predicate to a set or interval of codes and
  selects rows by testing the stored codes, without rebuilding any row's string

#### Scenario: FSST equality answered from compressed bytes
- **WHEN** an `=` or `IN` filter runs on an FSST-encoded column
- **THEN** the evaluator compresses the comparison value(s) with the block's
  symbol table and byte-compares against the stored compressed values, without
  decompressing any value

#### Scenario: FSST prefix and substring answered by a compressed-domain automaton
- **WHEN** a prefix or substring filter runs on an FSST-encoded column
- **THEN** the evaluator builds the automaton from the pattern and the block's
  symbol table once, runs it over the stored code bytes with one transition per
  byte, and selects exactly the rows a decode-then-match would select, without
  decompressing any value

#### Scenario: FSST range answered from prefix keys
- **WHEN** a range filter runs on an FSST-encoded column whose page carries a
  prefix-key stream
- **THEN** the evaluator decides every row whose key differs from the comparison
  value inside the stored width, and every row where the two agree across that
  width but one of them ends inside it, without decompressing those values

#### Scenario: Only genuinely tied rows are decompressed
- **WHEN** a stored key and the comparison value agree across the whole stored
  width and both continue past it
- **THEN** the evaluator decompresses exactly those rows and compares them in
  full, and the rows it already decided are never decompressed

#### Scenario: FSST range without prefix keys still falls back to decode
- **WHEN** a range filter targets an FSST-encoded column whose page carries no
  prefix-key stream
- **THEN** the evaluator declines and the reader decodes the block and filters
  the values, exactly as it did before the stream existed

#### Scenario: The prefix-key stream is earned, not assumed
- **WHEN** the writer encodes an FSST string block whose sample shows the
  prefix-key stream would not earn its bytes — it is too large against the block,
  or it would leave too many rows undecided to beat the decode it replaces
- **THEN** the block is written without the stream, and its encoding descriptor
  records the absence

#### Scenario: Compressed-data filter equals full decode
- **WHEN** the evaluator answers any predicate from the compressed form
- **THEN** the selected rows equal those of a full decode-then-filter, and null
  rows are never selected
