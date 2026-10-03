## MODIFIED Requirements

### Requirement: Encryption control set
HEF SHALL support the following encryption controls: file-level data encryption keys (file DEK), chunk-level AEAD, a separate metadata checksum, footer encryption for sensitive schemas, payload arena encryption, embedding/vector block encryption, and per-subject content keys for single-subject personal data enabling right-to-erasure crypto-shredding. The authenticated-encryption primitive for every one of these controls — chunk-level AEAD, footer encryption, payload-arena encryption, embedding/vector-block encryption, and the per-subject content-key path — SHALL be a single pinned AEAD: **AES-256-GCM** (via the hardware-only `hardware-rust-crypto` crate) as the pinned default, with **XChaCha20-Poly1305** (via the RustCrypto `chacha20poly1305` crate) as the named alternative selectable for deployments that require an extended-nonce or constant-time-software profile; no other cipher, and no hand-rolled AEAD construction, SHALL be used, and the chosen primitive SHALL NOT be removed or replaced except through a spec revision to the system-architecture library inventory. AES-256-GCM here is the same pinned algorithm the secrets envelope already uses to wrap per-tenant DEKs, so the master-key → per-tenant DEK → per-subject content-key envelope chain (Decisions 5 and 10) keeps one AEAD family from key wrapping through to payload bytes; each AEAD invocation SHALL use a unique nonce per key, constructed as pinned by Requirement: "Nonce construction and per-key message bound", and SHALL bind the relevant block/chunk identity as associated data so a ciphertext cannot be relocated to another block. Every one of these controls — including the per-subject content-key path — SHALL be a real AEAD seal that rejects tampered ciphertext on decrypt, as required by Requirement: "Per-subject blocks are AEAD-sealed against tampering"; no control SHALL be satisfied by an unauthenticated keystream (an XOR or stream cipher without a Poly1305/GMAC tag). Encryption granularity SHALL follow the data-class label: columns, payload-arena rows, and derived blocks labeled single-subject personal data (a `Person`/`Contact`/`Lead` and the payloads, embeddings, candidate keys, and context projections where that subject is the primary entity) SHALL be encrypted under that subject's content key, while all other columns (dimensions, timestamps, sequence, business foreign keys, numeric measures) SHALL be encrypted under the file DEK so columnar dictionary/FSST/FOR compression is preserved on the analytical scan path.

#### Scenario: Sensitive schema footer
- **WHEN** a tenant's policy requires protecting a sensitive schema
- **THEN** footer encryption is available and can be applied to that file

#### Scenario: Scan path uses file-DEK columns only
- **WHEN** an analytical scan reads dimensions, timestamps, and measures
- **THEN** it reads file-DEK-encrypted columns and never unwraps a per-subject content key

#### Scenario: Payload AEAD uses the pinned cipher
- **WHEN** a chunk, footer, payload-arena row, embedding block, or single-subject content-key block is encrypted
- **THEN** it is sealed with the pinned AEAD — AES-256-GCM (`hardware-rust-crypto`) by default, or the named XChaCha20-Poly1305 (`chacha20poly1305`) alternative — with a unique nonce per key and the block identity bound as associated data, and never with a hand-rolled or unlisted cipher

#### Scenario: Unauthenticated keystream is not a valid control
- **WHEN** an implementation would encrypt any of these controls with a BLAKE3-keyed (or other) extendable-output XOR keystream that carries no authentication tag
- **THEN** that is not a conforming encryption control: every control SHALL be sealed with the pinned AEAD so a decrypt of tampered ciphertext is rejected rather than silently returning altered plaintext

## ADDED Requirements

### Requirement: Nonce construction and per-key message bound
HEF SHALL pin how the AEAD nonce is constructed and SHALL bound the number of messages sealed under any one key, so that no key ever repeats a nonce. For the AES-256-GCM default, each 96-bit nonce SHALL be deterministically constructed from the key epoch and the block identity — a `(key_epoch, block_identity_counter)` pair unique within the key — rather than drawn at random, because the block identity is already bound as associated data and gives a collision-free counter; a per-key message limit SHALL be set below the point where random 96-bit nonces would reach their `2^32` birthday-collision bound, and an implementation that instead draws random 96-bit nonces SHALL enforce that same `2^32`-message-per-key ceiling and roll the key epoch before it is reached. On high-volume paths where deterministic counters are impractical and random nonces are operationally simpler, XChaCha20-Poly1305 SHALL be selected as the default so its 192-bit extended nonce makes random-nonce collision negligible; the per-key message bound for that path SHALL be stated as the extended-nonce bound rather than `2^32`. Nonce reuse under a single key is catastrophic for both primitives, so a nonce SHALL NEVER be reused under a key by any path, and rolling to a new key epoch SHALL be the mechanism that retires an exhausted nonce space. This construction is a closed part of the encryption feature and is feature-flagged with old readers refusing (per HEF's feature-directory discipline); it never changes decrypted plaintext, so it is compatible with byte-identical software/hardware paths (INV-HARDWARE-ACCEL) and with deterministic simulation.

#### Scenario: GCM nonce is derived from key epoch and block identity
- **WHEN** a block is sealed with the AES-256-GCM default
- **THEN** its 96-bit nonce is derived deterministically from the key epoch and the block identity (already bound as associated data), unique within the key, so no two blocks under one key share a nonce

#### Scenario: Per-key message ceiling rolls the key epoch
- **WHEN** the number of messages sealed under one key approaches the per-key bound (`2^32` for random 96-bit GCM nonces, the extended-nonce bound for XChaCha20-Poly1305)
- **THEN** the key epoch is rolled to a fresh key before the bound is reached, so the nonce space is never exhausted and no nonce is reused

#### Scenario: High-volume path takes the extended-nonce cipher
- **WHEN** a high-volume path cannot maintain deterministic per-block counters and prefers random nonces
- **THEN** XChaCha20-Poly1305 is selected as the default there so its 192-bit random nonce makes reuse negligible, and the per-key bound is documented as the extended-nonce bound

### Requirement: Per-subject blocks are AEAD-sealed against tampering
Every per-subject encrypted block — the single-subject personal-data payloads, embeddings, candidate keys, and context projections encrypted under a subject content key — SHALL be sealed with the pinned AEAD (Requirement: "Encryption control set") and SHALL reject tampered ciphertext on decrypt, returning an authentication failure rather than altered plaintext. The crypto-shred path SHALL NOT use an unauthenticated construction: the BLAKE3-keyed extendable-output XOR keystream currently implemented in `crates/storage/src/hef/deletes.rs` (which produces malleable ciphertext with no authentication tag) SHALL be retired and replaced with the pinned AEAD before any real tenant data is written under it, and the key-unwrap stand-in in `crates/storage/src/hef/security.rs` SHALL be replaced with the real secrets-envelope unwrap (master key → per-tenant DEK → per-subject content key) before it guards production data. Because these blocks sit on the right-to-erasure path, a malleable stand-in that let ciphertext be altered undetected would be a correctness and security defect, so conformance requires the reject-on-tamper property, not merely confidentiality.

#### Scenario: Tampered per-subject ciphertext is rejected
- **WHEN** a single-subject encrypted block's ciphertext or its bound associated data is altered and then decrypted
- **THEN** the AEAD authentication check fails and the decrypt is rejected, rather than returning altered plaintext

#### Scenario: Unauthenticated keystream retired before real data
- **WHEN** the per-subject crypto-shred path is prepared for production tenant data
- **THEN** the unauthenticated BLAKE3-keyed XOF XOR keystream is replaced by the pinned AEAD and the key-unwrap stand-in is replaced by the real secrets-envelope unwrap, so no tenant data is ever sealed by a malleable, unauthenticated construction

### Requirement: Per-subject keystore sizing and backup purge
The per-subject keystore SHALL be designed to hold and individually destroy the number of content keys a large tenant produces — on the order of millions of destroyable per-subject keys per tenant — without the destroy operation degrading as the key population grows, so that crypto-shredding stays a constant-cost key destruction rather than a scan. Destroying a subject's content keys SHALL purge those keys from every keystore backup within the erasure deadline the tenant is contractually held to, so that no backup copy of a destroyed key survives past that deadline to make the subject's ciphertext recoverable; this backup-purge story SHALL be an explicit part of the keystore design, not an assertion left to operations. Key destruction and backup purge SHALL be the sole mechanism of erasure: the immutable HEF/HEJ files and their backups SHALL NEVER be rewritten or purged to erase a subject (consistent with Requirement: "Per-subject crypto-shredding and erasure"), and once a subject's keys are destroyed and purged from backups the subject's ciphertext SHALL be unrecoverable across HEF, HEJ, and all data backups at once.

#### Scenario: Millions of keys, constant-cost destroy
- **WHEN** a tenant has accumulated millions of per-subject content keys and one subject is erased
- **THEN** that subject's keys are destroyed at constant cost without scanning the full key population, and the destroy does not slow as the tenant's key count grows

#### Scenario: Backup copies purged within the erasure deadline
- **WHEN** a subject's content keys are destroyed
- **THEN** those keys are purged from every keystore backup within the tenant's erasure deadline, so no surviving backup key can later re-expose the subject's ciphertext
