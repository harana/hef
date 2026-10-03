## ADDED Requirements

### Requirement: Forced-pipeline conformance coverage
The conformance harness SHALL be able to force a named encoding pipeline per column through an internal, test-harness-only builder input — the same class of injected input as the simulation clock — so every encode/decode path, cascade level, and compression family is exercised deliberately rather than only when adaptive selection happens to choose it. Forcing SHALL be unreachable outside the harness: no operator-facing key, no production code path, and a forced pipeline that is invalid for the column's type SHALL be rejected by the builder exactly as a corrupt recorded pipeline would be at read time. Conformance SHALL run the round-trip, law-suite, and predicate-equivalence checks over every forceable pipeline, so a decoder path cannot rot unexercised behind an adaptive winner.

#### Scenario: Every pipeline family is exercised in CI
- **WHEN** the conformance suite runs
- **THEN** each pipeline family and cascade level is forced onto a suitable column at least once, and its round-trip, law, and predicate checks pass — including families adaptive selection rarely picks

#### Scenario: Forcing does not exist in production
- **WHEN** a production build constructs a writer
- **THEN** no input can force a pipeline; selection follows the adaptive requirements only, and the no-operator-knobs rule holds

#### Scenario: An invalid forced pipeline is rejected
- **WHEN** the harness forces a pipeline that is not valid for a column's type
- **THEN** the builder rejects it with a diagnostic rather than writing an undecodable block
