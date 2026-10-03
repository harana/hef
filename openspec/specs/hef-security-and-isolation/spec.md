## Purpose

Defines HEF's security and tenant-isolation rules:

- Keeping each tenant's data separate, encryption controls, and protecting the raw payload.
- Producing evidence references that are safe to show in public output.

The concrete tenant-isolation, encryption, payload-protection, and evidence-ref detail are embedded in [security-detail.md](security-detail.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-security-and-isolation/spec.md).
## Requirements
### Requirement: Tenant isolation
`tenant_id` SHALL never be exposed in public APIs. HEF files SHALL NOT mix unrelated tenants — cross-tenant HEF files SHALL be forbidden. Vector/context/text indexes SHALL preserve tenant boundaries, and projections SHALL NOT mix tenants in a way that weakens access checks.

#### Scenario: Cross-tenant file rejected
- **WHEN** a write would place events from unrelated tenants in one HEF file
- **THEN** it is rejected

### Requirement: Encryption control set
HEF SHALL support the following encryption controls: file-level data encryption keys (file DEK), chunk-level AEAD, a separate metadata checksum, footer encryption for sensitive schemas, payload arena encryption, embedding/vector block encryption, and per-subject content keys for single-subject personal data enabling right-to-erasure crypto-shredding. The authenticated-encryption primitive for every one of these controls — chunk-level AEAD, footer encryption, payload-arena encryption, embedding/vector-block encryption, and the per-subject content-key path — SHALL be a single pinned AEAD: **AES-256-GCM** (via the RustCrypto `aes-gcm` crate) as the pinned default, with **XChaCha20-Poly1305** (via the RustCrypto `chacha20poly1305` crate) as the named alternative selectable for deployments that require an extended-nonce or constant-time-software profile; no other cipher, and no hand-rolled AEAD construction, SHALL be used, and the chosen primitive SHALL NOT be removed or replaced except through a spec revision to the system-architecture library inventory. AES-256-GCM here is the same pinned algorithm the secrets envelope already uses to wrap per-tenant DEKs, so the master-key → per-tenant DEK → per-subject content-key envelope chain (Decisions 5 and 10) keeps one AEAD family from key wrapping through to payload bytes; each AEAD invocation SHALL use a unique nonce per key and SHALL bind the relevant block/chunk identity — and, for a per-subject content-key block, the identity of the containing file — as associated data so a ciphertext cannot be relocated to another block, or to another file, even if it is copied verbatim under a mishandled key. Encryption granularity SHALL follow the data-class label: columns, payload-arena rows, and derived blocks labeled single-subject personal data (a `Person`/`Contact`/`Lead` and the payloads, embeddings, candidate keys, and context projections where that subject is the primary entity) SHALL be encrypted under that subject's content key, while all other columns (dimensions, timestamps, sequence, business foreign keys, numeric measures) SHALL be encrypted under the file DEK so columnar dictionary/FSST/FOR compression is preserved on the analytical scan path.

#### Scenario: Sensitive schema footer
- **WHEN** a tenant's policy requires protecting a sensitive schema
- **THEN** footer encryption is available and can be applied to that file

#### Scenario: Scan path uses file-DEK columns only
- **WHEN** an analytical scan reads dimensions, timestamps, and measures
- **THEN** it reads file-DEK-encrypted columns and never unwraps a per-subject content key

#### Scenario: Payload AEAD uses the pinned cipher
- **WHEN** a chunk, footer, payload-arena row, embedding block, or single-subject content-key block is encrypted
- **THEN** it is sealed with the pinned AEAD — AES-256-GCM (`hardware-rust-crypto`) by default, or the named XChaCha20-Poly1305 (`chacha20poly1305`) alternative — with a unique nonce per key and the block identity — and, for a per-subject content-key block, the containing file identity — bound as associated data, and never with a hand-rolled or unlisted cipher

### Requirement: Key-scope and residency as job-eligibility constraints
HEF background jobs (compaction, projection and derived-column builds, re-extraction, index rebuilds) SHALL declare the key scope they require (none, tenant-DEK, subject-keys) and SHALL be schedulable only on nodes holding a valid lease for that scope; jobs over subject-encrypted blocks SHALL respect residency/region constraints so processing stays in the subject's jurisdiction. A key lease SHALL carry the residency region it was granted for, and a node SHALL be eligible for a scope-requiring job only when it holds the required-scope lease for the job's residency region: a lease granted for another region SHALL NOT satisfy the job, so a tenant's keys are never used to process data outside their jurisdiction. Blocks SHALL stay encrypted at rest and in transit on every node, decrypted only transiently during authorized processing, and a key-lease revocation (including erasure) SHALL fail in-flight jobs closed so no output derived from revoked keys is ever published.

#### Scenario: Job without key lease is ineligible
- **WHEN** the scheduler considers a node lacking the required tenant-DEK lease for a compaction job over encrypted blocks
- **THEN** the node is ineligible and the job is placed only on a node with a valid lease

#### Scenario: Job in the wrong region is ineligible
- **WHEN** a node holds the required-scope lease for a tenant but only for a residency region other than the one the job must run in
- **THEN** the node is ineligible and the job is placed only on a node holding the lease for the job's region

### Requirement: Payload protection across rewrite and indexes
The payload arena SHALL be encrypted when tenant or deployment policy requires encrypted event payloads; promoted columns SHALL obey field-level redaction rules; context projections SHALL carry data-class labels. HEF rewrite SHALL NOT resurrect deleted, redacted, or crypto-shredded fields, and text/token/vector indexes SHALL NOT leak blocked fields through public APIs.

#### Scenario: Rewrite does not resurrect redacted field
- **WHEN** an HEF rewrite repacks a file containing a previously redacted field
- **THEN** the redacted field is not resurrected in the rewritten file

### Requirement: Public-safe evidence references
Chat/investigation outputs SHALL NOT expose file paths, row offsets, payload refs, or raw sequences. Evidence references SHALL be public-safe IDs or route-owned opaque cursors that resolve only through authorized services.

#### Scenario: Evidence ref hides storage identity
- **WHEN** an investigation output references an event as evidence
- **THEN** it uses a public-safe id or opaque cursor, not a storage path, row offset, payload ref, or raw sequence

### Requirement: Authorized exports pass the public-output boundary
Any Vortex export, or any other interchange/export artifact, SHALL pass through the same authorization, redaction, and crypto-shredding boundary that wraps every other egress from the system; an export SHALL NOT be a side door around that boundary, and there SHALL be no export path that trades the boundary away for performance. An export SHALL honor field-level redaction and per-subject erasure, so a field that is redacted or crypto-shredded on the authorized read path is likewise absent from the export (see Requirement: "Payload protection across rewrite and indexes" and Requirement: "Per-subject crypto-shredding and erasure"). An export SHALL NOT mix tenants: it SHALL contain data for a single tenant boundary and SHALL NOT expose `tenant_id` or weaken any access check (see Requirement: "Tenant isolation"). An export SHALL NOT expose file paths, row offsets, payload refs, raw sequences, internal tenant identifiers, or embedding values; it SHALL carry only public-safe references under the same rule as chat and investigation output (see Requirement: "Public-safe evidence references"). A subject whose content keys have been destroyed by crypto-shredding SHALL NOT have their data appear in any export produced after that erasure: because the export is taken at the authorized boundary where the shredded subject already renders as a tombstone, the erased subject's ciphertext is never recoverable into an export.

#### Scenario: Export omits a redacted field
- **WHEN** an interchange export is produced for an `events` table that contains a field redacted on the authorized read path
- **THEN** that field is absent from the export, because the export passes the same redaction boundary as every other egress and never resurrects a redacted field

#### Scenario: Export does not mix tenants
- **WHEN** an interchange export is produced
- **THEN** it contains data for a single tenant boundary only, does not expose `tenant_id`, and does not weaken any access check

#### Scenario: Crypto-shredded subject is absent from later exports
- **WHEN** a subject is crypto-shredded and an interchange export is produced afterward
- **THEN** that subject's single-subject data does not appear in the export, because the authorized boundary the export is taken at renders the shredded subject as a tombstone and the destroyed content keys make the ciphertext unrecoverable

#### Scenario: Export exposes no engine-internal references
- **WHEN** an interchange export is produced at the authorized boundary
- **THEN** it exposes no file paths, row offsets, payload refs, raw sequences, internal tenant identifiers, or embedding values, carrying only public-safe references just as chat and investigation output must

### Requirement: Confidential-computing and in-use hardening posture
This capability already protects data in two states: at rest (the encryption control set and per-subject crypto-shredding) and between tenants (tenant isolation). A third state exists while a node is actually working — the bytes in its memory and the running process an attacker might try to subvert — and modern silicon with the target kernel line (Linux 6.4 → 7.1) offers optional protections for it. This requirement fixes the posture for that in-use layer: every in-use control SHALL be optional, silicon- and deployment-gated hardening — never a correctness input, and never a substitute for or weakening of the at-rest encryption controls, key handling, crypto-shredding, redaction, or tenant-isolation rules of this capability, which apply unchanged whether or not any in-use control is active.

A deployment MAY run a node inside a hardware-encrypted trust domain (an Intel TDX or AMD SEV-SNP confidential VM), so that the node's memory is unreadable even to the host and hypervisor — the strongest available confidential-multi-tenancy lever. Inside a trust domain every existing control SHALL apply unchanged — the pinned AEADs, the master-key → tenant-DEK → subject-key envelope, BLAKE3 verification, redaction, and tenant isolation — and results SHALL be identical to the same node running outside a domain. The platform loads no BPF program today (a BPF CPU scheduler is explicitly deferred); if any BPF program is ever loaded on a production node, it SHALL be provenance-checked — signed and verified against a deployment-trusted key — before load, and an unsigned or unverifiable program SHALL NOT be loaded. Kernel-side mitigations the deployment inherits with no platform work (for example the anti-heap-spray bucket slab allocator on Linux 6.11+) are a recorded baseline of the deployment target, not a platform control, and no platform behaviour SHALL depend on them.

Whether a control must be present is a per-deployment declaration: a deployment MAY declare any control of this posture — including the deployment-host and build controls (`hef-hardware-deployment`, Requirement: "In-use hardening controls at the deployment host"; `implementation-toolchain`, Requirement: "Optional control-flow and memory-safety hardening builds") — required, and startup SHALL then refuse with a diagnostic naming the missing control when the silicon, kernel, or attestation cannot provide it, exactly as the NVMe character-device deployment constraint refuses. Where a control is not declared required, its absence SHALL change nothing: the un-hardened path SHALL remain fully correct, and whether a control is active SHALL be surfaced only through diagnostics, never through results, public output, or tenant-facing surfaces. No in-use control SHALL become a correctness input, alter the BLAKE3 integrity authority, weaken tenant isolation, select an accelerator path, or change behaviour under deterministic simulation.

#### Scenario: Node inside a trust domain behaves identically
- **WHEN** a node runs inside a TDX or SEV-SNP hardware-encrypted trust domain
- **THEN** every at-rest encryption control, key-envelope rule, crypto-shredding rule, redaction rule, tenant-isolation rule, and BLAKE3 verification applies unchanged, and queries return results identical to the same node outside a domain

#### Scenario: Deployment-required control refuses when absent
- **WHEN** a deployment declares an in-use control (for example the trust-domain profile) required and a node starts on a host whose silicon, kernel, or attestation cannot provide it
- **THEN** startup refuses with a diagnostic naming the missing control, and the node does not silently serve traffic without the promised protection

#### Scenario: Absent controls change nothing when not required
- **WHEN** a node starts on silicon with no confidential-computing support and the deployment declares no in-use control required
- **THEN** the node starts and serves normally on the un-hardened path with identical results, and the inactive controls are visible only in diagnostics

#### Scenario: Unsigned BPF program is refused
- **WHEN** a future feature attempts to load a BPF program on a production node without a valid signature verifiable against a deployment-trusted key
- **THEN** the load is refused and no unsigned BPF runs

