## ADDED Requirements

### Requirement: Writer peak-memory gate
Writer peak memory SHALL be a numeric pass/fail gate, not just a reported metric, for the fresh-publish and rewrite lifecycle stages. The gated ceiling SHALL be proportional to stripe scope — the configured stripe byte target plus a fixed directory allowance — and SHALL NOT scale with output file size, so the stripe-scope streaming bound of the hef-write-path requirement "The builder streams at stripe scope with bounded memory" is held rather than merely intended. The gate SHALL be backed by a named benchmark in the committed harness per the mechanically-enforced-gates requirement, measured on the declared profile with a file at least several multiples of the stripe target, and a baseline whose peak memory regresses beyond tolerance SHALL drop the streamed-build path from the ready set until resolved. The gate is a cost measurement only: it changes no durable byte, checksum, ordering, or visibility outcome.

#### Scenario: A large build passes on stripe-scope memory
- **WHEN** the harness builds a file several multiples of the stripe byte target on the declared profile
- **THEN** measured writer peak memory stays within the stripe-scope ceiling, and the gate fails if it scales with the file instead

#### Scenario: A memory regression drops the path
- **WHEN** a later harness run shows writer peak memory regressing beyond tolerance
- **THEN** the streamed-build path drops from the ready set until the regression is resolved
