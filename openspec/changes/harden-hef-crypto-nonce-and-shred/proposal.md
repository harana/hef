Status: Approved (2026-07-09)

## Why

HEF's security spec pins AES-256-GCM (with XChaCha20-Poly1305 as the named
alternative) and demands "a unique nonce per key," but it never says *how* the
nonce is constructed. That gap is dangerous at HEF's write volume: AES-256-GCM
with random 96-bit nonces runs into the `2^32` birthday-collision bound per key,
and a single nonce reuse under one key is catastrophic — it can leak plaintext
and forge tags. A spec that mandates uniqueness without pinning a construction
leaves the most failure-prone crypto decision to each implementation.

The gap is not hypothetical. The crypto-shred path that is actually implemented
(`crates/storage/src/hef/deletes.rs`) does not use the pinned AEAD at all: it
XORs data against a BLAKE3-keyed extendable-output keystream with **no
authentication tag**. That is malleable ciphertext — an attacker can flip
plaintext bits undetected — sitting on the right-to-erasure path, exactly where
reject-on-tamper matters most. The key-unwrap in
`crates/storage/src/hef/security.rs` is likewise a stand-in, not the real
secrets-envelope unwrap. This is the moment to fix it: before any real tenant
data is written under the malleable construction.

Finally, the erasure story assumes a keystore that holds millions of
individually-destroyable per-subject keys per tenant and purges destroyed keys
from backups within the erasure deadline — but that scale and backup-purge
behavior is asserted, never designed. This change pins the nonce construction,
makes AEAD-seal-with-reject-on-tamper a conformance item for per-subject blocks,
and sizes the keystore story.

## What Changes

**The nonce construction is pinned, and the per-key message bound is stated.**
For the AES-256-GCM default, each 96-bit nonce is deterministically constructed
from `(key_epoch, block_identity_counter)` — collision-free because the block
identity is already bound as associated data — rather than drawn at random. An
implementation that instead uses random 96-bit nonces must enforce the `2^32`
messages-per-key ceiling and roll the key epoch before it is reached. On
high-volume paths where deterministic counters are impractical and random
nonces are operationally simpler, XChaCha20-Poly1305 becomes the default so its
192-bit extended nonce makes random-nonce collision negligible. Either way the
per-key message bound is stated, a nonce is never reused under a key, and
rolling the key epoch is the mechanism that retires an exhausted nonce space.
This is a closed, feature-flagged part of the encryption feature (old readers
refuse), it never changes decrypted plaintext, and it is compatible with
byte-identical software/hardware paths (INV-HARDWARE-ACCEL) and deterministic
simulation.

**Per-subject blocks must be AEAD-sealed and reject tampering.** A new
conformance requirement states that every per-subject encrypted block is sealed
with the pinned AEAD and rejects tampered ciphertext on decrypt. The
unauthenticated BLAKE3-keyed XOF XOR keystream in `hef/deletes.rs` is retired
and replaced with the pinned AEAD before any real tenant data exists, and the
key-unwrap stand-in in `hef/security.rs` is replaced with the real
secrets-envelope unwrap.

**The per-subject keystore story is sized, not asserted.** A new requirement
mandates a keystore designed to hold and individually destroy on the order of
millions of per-subject keys per tenant at constant destroy cost (no scan), and
to purge destroyed keys from every keystore backup within the tenant's erasure
deadline — with backup purge an explicit part of the design.

## Capabilities

### Modified Capabilities

- `hef-security-and-isolation` — MODIFY "Encryption control set" to require the
  nonce to be constructed as pinned by the new nonce requirement, and to state
  that every control (including the per-subject content-key path) is a real AEAD
  seal that rejects tampered ciphertext and is never satisfied by an
  unauthenticated keystream.

### Added Capabilities

- `hef-security-and-isolation` — ADD "Nonce construction and per-key message
  bound" (deterministic `(key_epoch, block_identity_counter)` GCM nonces or
  XChaCha20-Poly1305 default on high-volume paths, with the per-key message
  bound stated and key-epoch rolling before the nonce space is exhausted); ADD
  "Per-subject blocks are AEAD-sealed against tampering" (reject-on-tamper
  conformance, retiring the unauthenticated XOR keystream and the key-unwrap
  stand-in before real tenant data); ADD "Per-subject keystore sizing and backup
  purge" (millions of destroyable keys per tenant at constant destroy cost, and
  backup purge within the erasure deadline).

## Impact

- **No malleable stand-in ships into the erasure path.** The unauthenticated
  XOR keystream and the key-unwrap stand-in are retired before any real tenant
  data is written under them, so tampered per-subject ciphertext is rejected
  rather than silently altering plaintext.
- **The most failure-prone crypto decision is pinned, not left open.** Nonce
  construction and the per-key message bound are fixed in the spec, closing the
  gap between "unique nonce per key" and a concrete, reuse-proof mechanism.
- **The erasure guarantee is designed to scale.** The keystore is sized for
  millions of destroyable keys per tenant with a real backup-purge story, so
  crypto-shredding stays constant-cost and destroyed keys do not survive in
  backups past the erasure deadline.
- **Invariants hold.** BLAKE3 remains the sole integrity authority for content
  addressing; the AEAD change is confidentiality-and-authentication for
  encrypted blocks, not a second integrity authority. The nonce construction
  never changes decrypted plaintext, so byte-identical software/hardware paths
  (INV-HARDWARE-ACCEL) and deterministic simulation are unaffected.

## Open Questions

1. **Where the deterministic/high-volume line falls.** Which specific write
   paths adopt the deterministic GCM counter versus the XChaCha20-Poly1305
   random-nonce default is a measurement and operability question; the spec
   fixes only that each path states its per-key bound and never reuses a nonce.
2. **Keystore backend for millions of keys.** The concrete keystore engine and
   its backup-purge mechanism (and how the erasure deadline is enforced against
   backup rotation) are a design question the sizing requirement opens but does
   not close here.
