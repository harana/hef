# Tasks — harden-hef-crypto-nonce-and-shred

> This change closes the two open crypto decisions in `hef-security-and-isolation`
> §3.9: it pins the AEAD nonce construction and per-key message bound (deterministic
> `(key_epoch, block_identity_counter)` GCM nonces, or XChaCha20-Poly1305 as the
> high-volume default, with the bound stated and the key epoch rolled before the
> nonce space is exhausted), makes reject-on-tamper a conformance item for
> per-subject blocks, and sizes the per-subject keystore (millions of destroyable
> keys per tenant, backup purge within the erasure deadline). The spec deltas are
> the deliverable; the real implementation follow-up — retiring the unauthenticated
> BLAKE3-keyed XOF XOR keystream in `crates/storage/src/hef/deletes.rs` and the
> key-unwrap stand-in in `crates/storage/src/hef/security.rs` — is enumerated as code
> tasks below. The nonce construction never changes decrypted plaintext, so
> INV-HARDWARE-ACCEL byte-identity and deterministic simulation hold; BLAKE3 stays
> the sole content-addressing integrity authority.

## hef-security-and-isolation — nonce, seal, and keystore

- [x] MODIFY "Encryption control set" so the nonce is constructed as pinned by
      Requirement: "Nonce construction and per-key message bound", and so every
      control (including the per-subject content-key path) is a real AEAD seal that
      rejects tampered ciphertext and is never satisfied by an unauthenticated
      keystream. Implements `hef-security-and-isolation` — "Encryption control set".
- [x] ADD "Nonce construction and per-key message bound": deterministic
      `(key_epoch, block_identity_counter)` GCM nonces (block identity already bound
      as AAD) or XChaCha20-Poly1305 on high-volume paths, the per-key message bound
      stated (`2^32` for random 96-bit GCM, extended-nonce bound for XChaCha20), and
      the key epoch rolled before the nonce space is exhausted; feature-flagged,
      old readers refuse, plaintext unchanged. Implements
      `hef-security-and-isolation` — "Nonce construction and per-key message bound".
- [x] ADD "Per-subject blocks are AEAD-sealed against tampering": per-subject blocks
      sealed with the pinned AEAD and rejecting tampered ciphertext on decrypt, with
      the unauthenticated XOR keystream and the key-unwrap stand-in retired before
      any real tenant data exists. Implements `hef-security-and-isolation` —
      "Per-subject blocks are AEAD-sealed against tampering".
- [x] ADD "Per-subject keystore sizing and backup purge": a keystore designed for
      millions of destroyable per-subject keys per tenant at constant destroy cost
      (no scan), and backup purge within the tenant's erasure deadline as an explicit
      part of the design. Implements `hef-security-and-isolation` — "Per-subject
      keystore sizing and backup purge".

## Code — retire the unauthenticated crypto-shred stand-in (req: hef-security-and-isolation "Per-subject blocks are AEAD-sealed against tampering")

> All of this is guarded by the requirement above and must land before any real
> tenant data is written under the current construction; deterministic simulation
> and byte-identity are preserved because the seal change never alters decrypted
> plaintext.

- [x] Replace the BLAKE3-keyed extendable-output XOR keystream in
      `crates/storage/src/hef/deletes.rs` (the `apply_keystream` path used by encrypt
      and decrypt) with the pinned AEAD (AES-256-GCM by default, XChaCha20-Poly1305
      on the high-volume default path), so per-subject encrypt/decrypt produces an
      authenticated seal and a tampered ciphertext or altered associated data is
      rejected on decrypt rather than returning altered plaintext.
- [x] Construct the AEAD nonce in `crates/storage/src/hef/deletes.rs` from the key
      epoch and the block identity as pinned by "Nonce construction and per-key
      message bound", binding the block identity as associated data, and roll the
      key epoch before the per-key message bound is reached.
- [x] Replace the key-unwrap stand-in in `crates/storage/src/hef/security.rs`
      (`ContentKeyId` / content-key path) with the real secrets-envelope unwrap
      (master key → per-tenant DEK → per-subject content key), so a real content key
      guards production data and the plaintext content key exists only transiently
      in memory during an authorized decrypt.

## Tests

- [x] `hef-security-and-isolation`: a per-subject encrypt/decrypt round-trip test
      that flips a ciphertext byte and asserts decrypt returns an authentication
      failure (reject-on-tamper), replacing any test that relied on the malleable
      XOR keystream.
- [x] Nonce-uniqueness test: no two blocks sealed under one key epoch share a nonce,
      and the key epoch rolls before the stated per-key message bound is reached.
- [x] Deterministic simulation parity: sealing and unsealing produce identical
      decrypted plaintext with the deterministic-nonce and extended-nonce paths, and
      every simulated run stays reproducible from its seed.

## Companion edits (applied at archive, not part of the requirement deltas)

- [x] `hef-security-and-isolation/security-detail.md`: record the pinned nonce
      construction, the per-key message bounds, the reject-on-tamper seal for
      per-subject blocks, and the keystore sizing and backup-purge story.

## Verification

- [x] `openspec validate harden-hef-crypto-nonce-and-shred --strict` green; every
      `### Requirement:` in the delta carries SHALL or MUST and at least one
      `#### Scenario:`, and no requirement text dangles after a scenario.
