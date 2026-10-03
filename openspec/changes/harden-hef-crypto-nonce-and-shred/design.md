# Design — Harden HEF cryptography: pin the nonce scheme and the crypto-shred seal

Three decisions, in the tension → decision → why → rejected → spec-edits style of
`docs/design-review-decisions.md`. The running theme: the security spec already
pins one AEAD family end to end, but it left two things to chance — how the nonce
is built, and whether the per-subject path is actually sealed — and the shipped
crypto-shred code took the unsafe branch of both. These decisions close the gap
in the spec and name the code that has to follow.

## 1. Pin the nonce construction; do not leave "unique nonce per key" to each implementation

- **Tension.** The spec says "a unique nonce per key" but names no construction.
  Two safe answers exist and they pull apart: a *deterministic* nonce derived
  from a counter is collision-free but needs per-key state; a *random* 96-bit
  nonce needs no state but hits the `2^32` birthday bound per key, and a single
  reuse under GCM is catastrophic. Leaving the choice open means each write path
  can pick the unsafe combination (random 96-bit nonces at high volume with no
  ceiling) and still claim to satisfy the spec.
- **Decision.** Pin it. For the AES-256-GCM default, the 96-bit nonce is
  deterministically constructed from `(key_epoch, block_identity_counter)` —
  collision-free because the block identity is already bound as associated data,
  so it doubles as a unique counter. An implementation that instead draws random
  96-bit nonces must enforce the `2^32`-message-per-key ceiling and roll the key
  epoch before it is reached. On high-volume paths where deterministic counters
  are impractical, XChaCha20-Poly1305 becomes the default so its 192-bit nonce
  makes random collision negligible; that path states its bound as the
  extended-nonce bound. Rolling the key epoch is the one mechanism that retires
  an exhausted nonce space, and a nonce is never reused under a key by any path.
- **Why.** The block identity is *already* bound as AAD, so the deterministic
  counter costs nothing new and removes the birthday bound entirely. Where per-block
  counters are impractical, the extended-nonce cipher is the honest default
  rather than random 96-bit GCM. Either way the per-key bound is written down, so
  no path silently drifts toward reuse. The construction never changes decrypted
  plaintext, so INV-HARDWARE-ACCEL byte-identity and deterministic simulation are
  untouched; it is feature-flagged with old readers refusing.
- **Rejected.** *Random 96-bit GCM nonces everywhere with no stated ceiling.* —
  Reaches the `2^32` birthday bound at HEF's volume and makes catastrophic reuse
  a matter of time. *Leaving construction unspecified.* — Keeps the exact gap
  this change exists to close.
- **Spec edits.** `hef-security-and-isolation` MODIFY "Encryption control set"
  (nonce constructed as pinned by the new requirement); ADD "Nonce construction
  and per-key message bound".

## 2. Make reject-on-tamper a conformance item and retire the unauthenticated XOR keystream

- **Tension.** The spec pins an AEAD, but the crypto-shred code that actually
  ships (`crates/storage/src/hef/deletes.rs`) XORs data against a BLAKE3-keyed
  extendable-output keystream with no authentication tag. It provides
  confidentiality-shaped bytes but is *malleable*: an attacker can flip plaintext
  bits undetected. It sits on the right-to-erasure path, where tamper-detection
  matters most, and `hef/security.rs`'s key-unwrap is a stand-in. The spec never
  states, as a checkable property, that a per-subject block rejects tampered
  ciphertext — so the malleable code passes review.
- **Decision.** Add a conformance requirement: every per-subject encrypted block
  is sealed with the pinned AEAD and rejects tampered ciphertext on decrypt
  (returns an authentication failure, never altered plaintext). Name the retirement
  explicitly: the unauthenticated XOR keystream is replaced with the pinned AEAD,
  and the key-unwrap stand-in with the real secrets-envelope unwrap, before any
  real tenant data is written under them. State in "Encryption control set" that
  no control is ever satisfied by an unauthenticated keystream.
- **Why.** BLAKE3 is the platform's integrity authority for *content addressing*,
  but a keyed XOF used as a keystream is not authenticated encryption — it has no
  tag to check on decrypt. On the erasure path a malleable stand-in is a security
  defect, not a placeholder. Fixing it before real tenant data exists costs a code
  change and no migration; fixing it later means re-sealing live ciphertext.
- **Rejected.** *Keep the XOR keystream and add a separate MAC.* — Reinvents AEAD
  by hand, which the spec already forbids ("no hand-rolled AEAD construction");
  the pinned primitive already gives an authenticated seal. *Treat the stand-in as
  acceptable until later.* — "Later" is after tenant data is sealed by a malleable
  construction.
- **Spec edits.** `hef-security-and-isolation` MODIFY "Encryption control set"
  (real AEAD seal, no unauthenticated keystream); ADD "Per-subject blocks are
  AEAD-sealed against tampering".

## 3. Size the keystore for millions of destroyable keys and pin the backup-purge story

- **Tension.** Erasure is implemented by destroying per-subject content keys, and
  the spec already says destroying a key (and purging keystore backups within the
  erasure deadline) renders the subject's ciphertext unrecoverable. But the *scale*
  — millions of individually-destroyable keys per tenant — and the backup-purge
  mechanism are asserted, not designed. A keystore that degrades as keys accumulate,
  or a backup that keeps a destroyed key past the deadline, quietly breaks the
  erasure guarantee.
- **Decision.** Add a sizing requirement: the keystore is designed to hold and
  individually destroy on the order of millions of per-subject keys per tenant at
  constant destroy cost (no scan of the key population), and destroying a key
  purges it from every keystore backup within the tenant's erasure deadline — with
  the backup-purge story an explicit part of the design, not an operations
  afterthought. Key destruction plus backup purge stays the sole erasure
  mechanism; immutable HEF/HEJ files and their backups are never rewritten.
- **Why.** The erasure guarantee is only as strong as the weakest of {destroy is
  cheap enough to actually run, backups don't resurrect the key}. Both have to be
  designed, not assumed. Stating the scale and the backup deadline as requirements
  forces the keystore engine choice to be made against a real target.
- **Rejected.** *Leave sizing to the keystore implementation.* — The scale and the
  backup deadline are correctness-bearing for erasure; they belong in the spec, not
  discovered when a large tenant's destroy starts scanning.
- **Spec edits.** `hef-security-and-isolation` ADD "Per-subject keystore sizing and
  backup purge".
