## MODIFIED Requirements

### Requirement: Signed-event provenance column family
The logical event model SHALL define an optional Tier A provenance column
family for events originating from a signed protocol, carrying at minimum:
`author_pubkey` (the signing public key), `signature` (the protocol signature
bytes), `signature_scheme` (a registry-controlled tag), `protocol_event_id`
(the protocol's own content-derived event identifier), `protocol_kind` (the
protocol's event-type discriminator), and `claimed_at` (the author-claimed
timestamp as a `TimestampValue`). The scheme registry SHALL be closed and
SHALL carry the schemes the platform actually verifies, each naming its
signature byte length and public-key byte length so a malformed row is refused
at write rather than at read. The family SHALL be absent — materializing no
columns — for streams whose events carry no signatures; its presence SHALL NOT
alter the fixed envelope, and `sequence` assignment, pruning, and ordering
SHALL remain governed by the envelope alone, with `claimed_at` treated as
author-influenced data, never as an ordering or retention authority.
Provenance columns SHALL participate in the ordinary column machinery —
physical encodings, workload-aware promotion, data-class labels, and
field-level authorization — and SHALL be exactly as visible as the event they
attest, never more.

Where a protocol signs an event with more than one key — as a federated
protocol does, signing per originating server — the family SHALL record the
signature the platform verified at ingest and the key it verified against, not
an arbitrary member of the set, so re-verification years later reproduces the
ingest-time decision rather than guessing at it.

#### Scenario: An archived event re-verifies offline
- **WHEN** an auditor reads an event out of a published HEF years after it was written
- **THEN** the provenance columns carry the signature, the signing public key, the scheme tag, and the protocol event identifier byte-exact, so the signature re-verifies without trusting the store that returned it

#### Scenario: A scheme the platform does not verify is refused
- **WHEN** an ingest path offers a provenance row whose `signature_scheme` is absent from the registry
- **THEN** the write is refused and nothing is appended, rather than a row being stored that no reader can check

#### Scenario: A signature of the wrong length never reaches the journal
- **WHEN** a provenance row carries a signature or public key whose byte length does not match what its declared scheme fixes
- **THEN** the write is refused at the write path, not surfaced as a verification failure on a later read
