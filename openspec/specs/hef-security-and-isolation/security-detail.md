# HEF Security and Isolation — Tenant Isolation, Encryption, Payload Protection, Evidence Refs

Companion artifact for the `hef-security-and-isolation` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


### Tenant isolation

```text
tenant_id must never be exposed in public APIs;
HEF files must not mix unrelated tenants. Cross-tenant HEF files are forbidden.
vector/context/text indexes must preserve tenant boundaries;
projections must not mix tenants in a way that weakens access checks.
```

### Encryption

Required encryption control set:

```text
file-level data encryption key (file DEK) support
chunk-level AEAD support
separate metadata checksum
footer encryption support for sensitive schemas
payload arena encryption support
embedding/vector block encryption support
per-subject content-key support for single-subject personal data (see Per-subject crypto-shredding and erasure below)
```

Encryption granularity follows the data-class label. Columns, payload-arena rows, and derived blocks labeled single-subject personal data — a `Person`/`Contact`/`Lead` and the payloads, embeddings, candidate keys, and context projections where that subject is the primary entity — must be encrypted under that subject's content key. All other columns — dimensions, timestamps, sequence, business foreign keys, and numeric measures — must be encrypted under the file DEK so columnar dictionary/FSST/FOR compression is preserved on the analytical scan path. Per-subject-encrypted blocks do not dictionary-compress across subjects; this is acceptable because those columns are high-cardinality, late-materialized evidence columns rather than scan-path columns.

#### Pinned AEAD primitive

Every one of the controls above — chunk-level AEAD, footer encryption, payload-arena encryption, embedding/vector-block encryption, and the per-subject content-key path — is sealed with a single pinned authenticated cipher, never a hand-rolled or unlisted one:

```text
default       AES-256-GCM              via the hardware-only `hardware-rust-crypto` crate (AES-NI/PCLMULQDQ, ARMv8 AES/PMULL); 96-bit (12-byte) nonce
alternative   XChaCha20-Poly1305       via the RustCrypto `chacha20poly1305` crate; 192-bit (24-byte) nonce
forbidden     any XOR / stream keystream with no Poly1305 or GMAC tag
```

The `AeadScheme` selected for a block is recorded (by crate name) in `encryption_metadata` so a reader can verify the primitive without a crate scan. AES-256-GCM here is the same algorithm the secrets envelope uses to wrap per-tenant DEKs, so one AEAD family runs from key wrapping through to payload bytes. A control that is "encrypted" by an unauthenticated keystream is not a conforming control: because it carries no authentication tag, a decrypt of tampered ciphertext would silently return altered plaintext instead of being rejected.

#### Nonce construction and per-key message bound

The AEAD nonce is not drawn at random on the payload-sealing path; it is constructed deterministically from the key epoch and the block identity, so no two blocks sealed under one key ever share a nonce:

```text
nonce bytes = block_id (u64, little-endian) || key_epoch (u64, little-endian)
              then resized to the scheme's nonce length:
  AES-256-GCM         12 bytes  = block_id (8) + low 32 bits of key_epoch (4)
  XChaCha20-Poly1305  24 bytes  = block_id (8) + key_epoch (8) + zero padding (8)
AAD = block_id (u64, little-endian)   # binds each ciphertext to its block; relocation fails auth
```

The block identity is already bound as associated data, so the `(key_epoch, block_id)` pair is a collision-free counter and needs no random draw. A per-key message bound is stated for each path and the key epoch is rolled to a fresh key before the bound is reached, so the nonce space is never exhausted and a nonce is never reused:

```text
random 96-bit GCM nonces   2^32 messages per key    (birthday-collision ceiling; enforce and roll below it)
deterministic GCM nonces   bounded by the epoch's 32-bit block-id space before the epoch rolls
XChaCha20-Poly1305         extended-nonce bound      (192-bit nonce makes random-nonce collision negligible)
```

Rolling to a new key epoch is the one mechanism that retires an exhausted nonce space. Nonce reuse under a single key is catastrophic for both primitives (it can leak plaintext and forge tags), so no path ever reuses a nonce under a key. The construction is feature-flagged with old readers refusing, and it never changes decrypted plaintext, so byte-identical software/hardware paths (INV-HARDWARE-ACCEL) and deterministic simulation are unaffected. Key wrapping is the one place a random nonce is used, because only a handful of keys are wrapped per subject — far below any birthday bound.

### Payload protection

Raw payloads are often the highest-risk region.

```text
payload arena must be encrypted when tenant or deployment encryption policy requires encrypted event payloads;
promoted columns must obey field-level redaction rules;
context projections must carry data-class labels;
HEF rewrite must not resurrect deleted/redacted fields;
text/token/vector indexes must not leak blocked fields through public APIs.
```

### Public-safe evidence refs

Chat/investigation outputs must not expose file paths, row offsets, payload refs, or raw sequences. Evidence references are public-safe IDs or route-owned opaque cursors that resolve through authorized services.

### Per-subject crypto-shredding and erasure

Right-to-erasure is a first-class HEF/HEJ property, implemented by destroying keys, never by rewriting immutable data. It reuses the secrets model's envelope chain (the multi-tenancy-and-secrets capability): a master key resolved out-of-band wraps a per-tenant DEK, which wraps individually-destroyable per-subject content keys, which encrypt single-subject PII ciphertext in HEF and HEJ.

```text
master key            resolved out-of-band; never in config or any replicated frame or log
  -> per-tenant DEK   wrapped under a KEK from the master key; stored wrapped in the keystore
    -> per-subject content key   wrapped under the tenant DEK; catalogued; individually destroyable
      -> single-subject PII ciphertext in HEF and HEJ
```

Binding. Each single-subject PII block in HEF or HEJ must record in its `encryption_metadata` the `content_key_id` and key epoch of the content key that encrypted it. The plaintext content key must exist only transiently in the processing node's memory during an authorized decrypt; it must not be persisted in plaintext, replicated, or returned to clients. Authorized product reads unwrap the chain on the processing node and return decrypted PII only after column authorization; the analytical scan path reads file-DEK columns and never unwraps a content key.

AEAD seal and tamper rejection. Every per-subject block is sealed with the pinned AEAD above (nonce from `(key_epoch, block_id)`, block id bound as AAD), not with an unauthenticated keystream. On decrypt the authentication tag is checked, so a block whose ciphertext or bound block identity has been altered is rejected — an authentication failure, never altered plaintext. The read path therefore distinguishes three outcomes, keeping tamper separate from erasure:

```text
Plaintext   content key live, tag verifies      -> decrypted PII (after column authorization)
Rejected    content key live, tag fails          -> tamper: ciphertext or AAD altered, decrypt refused
Tombstone   content key destroyed or unknown     -> crypto-shredded: ciphertext unrecoverable
```

Both the per-subject block seal and each key-unwrap step in the master → tenant DEK → content-key chain reject tampering, so neither a forged block nor a re-pointed wrapped key can smuggle out altered plaintext. The earlier BLAKE3-keyed extendable-output XOR keystream and the key-unwrap stand-in are retired before any real tenant data is written under them; BLAKE3 remains the sole integrity authority for content addressing, and the AEAD change is confidentiality-and-authentication for encrypted blocks, not a second integrity authority.

Pre-resolution and append-only. HEJ frames are written at ingest, before entity resolution, and are immutable, so a single-subject payload must be encrypted under a per-payload content key whose wrapped form lives in the mutable keystore, initially associated with the tenant (unresolved). An erasure-reconciliation backlog re-wraps that content key under the resolved subject once resolution assigns one. Re-keying re-wraps the key in the keystore and never re-encrypts the immutable HEJ frame or HEF block.

Erasure. Destroying a subject's content keys in the keystore — and purging them from key-store backups within the erasure deadline — renders that subject's ciphertext unrecoverable across HEF, HEJ, and all data backups at once, with no rewrite and independent of legal-hold/retention timing on the data. Immutable HEF/HEJ files and their backups are never purged for erasure: post-shred their ciphertext is noise, so only the small keystore and its backups are purged. Erasure-aware rebuild and HEJ replay must render any block whose content key has been destroyed as a tombstone rather than failing.

Keystore sizing and backup purge. The erasure guarantee is only as strong as the weakest of {destroy is cheap enough to run, backups don't resurrect the key}, so both are designed rather than assumed:

```text
capacity        millions of destroyable per-subject content keys per tenant
destroy cost    constant — a single keyed entry update, no scan of the key population
backup purge    destroyed keys removed from every keystore backup within the tenant's erasure deadline
never rewritten immutable HEF/HEJ files and their backups; only the keystore and its backups are purged
```

Destroying a subject's key must not slow as the tenant's key count grows — a tenant with millions of keys erases a subject as fast as one with a handful — so crypto-shredding stays a constant-cost key destruction, never a scan. Backup purge is an explicit part of the keystore design, not an assertion left to operations: no backup copy of a destroyed key may survive past the erasure deadline to make the subject's ciphertext recoverable. Key destruction plus backup purge is the sole mechanism of erasure; once a subject's keys are destroyed and purged from backups, the subject's ciphertext is unrecoverable across HEF, HEJ, and all data backups at once.

Single-subject vs multi-subject. A single-subject record is crypto-shredded outright by destroying its content key. A multi-subject event (an email or meeting touching several subjects) must use field-level deletion vectors (see the hef-deletes-and-corrections capability) so erasure redacts only the forgotten subject's fields on rebuild while the event survives for co-mentioned subjects.

Scope. Single-subject PII columns/payloads and derived single-subject artifacts (embeddings, candidate keys, context projections) are encrypted under the subject content key and are destroyed by the shred; rebuildable accelerators (vector/candidate indexes) are reproduced erasure-aware without the shredded subject. Cross-subject business aggregates (footer/stripe sums and counts, and the `DeletionAggregateDelta` from the hef-aggregation-metadata capability) are NOT single-subject artifacts: they remain under the file DEK, and because erasure de-identifies and retains the underlying transaction rather than deleting it, and an individual subject is not a footer dimension, crypto-shredding a subject does not perturb measure aggregates.

---

