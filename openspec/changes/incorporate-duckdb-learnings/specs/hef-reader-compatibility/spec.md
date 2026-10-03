## ADDED Requirements

### Requirement: Encoding availability windows gate conformance
Each pipeline family SHALL record an availability window: the format version that introduced it and, once retired, the version that retired it — retirement meaning read-only, never written again, with old files remaining readable forever. Conformance and footer validation SHALL reject a block whose recorded pipeline lies outside its window for the file's declared format version — a pipeline the declared version could not legally have written is treated as corruption or forgery, not decoded — generalizing the existing ad-hoc rule for the one retired transform id into a mechanical check over every family. This is validation tightening only: no stored byte changes, and no currently valid file becomes invalid.

#### Scenario: A block claiming a future pipeline is rejected
- **WHEN** validation meets a block whose recorded pipeline was introduced after the file's declared format version
- **THEN** the block is rejected as invalid rather than decoded

#### Scenario: A retired pipeline cannot appear in new files
- **WHEN** a file declaring a version at or past a pipeline's retirement records that pipeline on a block
- **THEN** validation rejects the block, while files declaring earlier versions keep decoding it

#### Scenario: Old files stay readable
- **WHEN** a reader opens a file written before a pipeline's retirement
- **THEN** the pipeline decodes exactly as before — retirement closes the writer, never the reader
