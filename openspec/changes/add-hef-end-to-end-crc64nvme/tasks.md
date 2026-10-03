The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-end-to-end-crc64nvme/).

# Tasks — add-hef-end-to-end-crc64nvme

> This change turns §3.7 of `docs/hef-format-review.md` into spec deltas that
> verify the platform's existing CRC-64/NVME precheck at both ends of the format.
> Upward: the `object-store` provider capability descriptor gains supported
> checksum algorithms, and where a provider supports CRC-64/NVME the uploader
> sends it (provider rejects a corrupted commit) and reads it back as a download
> precheck; `hef-write-path` computes that checksum once, streaming alongside the
> authoritative BLAKE3 hash. Downward: `store-file-layer` gains a probed
> `nvme_pi` guard-tag entry (PI format + Linux 6.14 io_uring integrity
> attributes) that attaches guard tags on writes and lets the device verify on
> reads, and `hef-hardware-deployment` records PI as a probe-gated, hint-only
> integrity layer. In both directions BLAKE3 stays the sole admission authority
> (INV unchanged); every added path is capability-probed with a portable
> fallback that is today's behavior, observably equivalent, and keyless
> (INV-HARDWARE-ACCEL); no new on-disk HEF file shape is introduced. Spec deltas
> land first; the code is enumerated below.

## object-store — provider checksum algorithms in the descriptor

- [x] MODIFY "Full provider capabilities — byte ranges, multipart uploads, and
      conditional writes" to name the provider's supported server-side checksum
      algorithms in the capability descriptor; send CRC-64/NVME as the multipart/
      full-object upload checksum where supported (provider rejects a corrupted
      commit); use a provider-returned CRC-64/NVME checksum as a download precheck
      evaluated before authoritative BLAKE3; degrade GCS (CRC32C), Azure, and the
      local-directory default to their native algorithm or none (today's
      behavior); keep the provider checksum non-authoritative and keyless.
      Implements `object-store` — "Full provider capabilities — byte ranges,
      multipart uploads, and conditional writes".

## hef-write-path — one streaming checksum pass alongside BLAKE3

- [x] ADD "Provider upload checksum is computed once alongside BLAKE3": on
      publish to a CRC-64/NVME-capable provider, compute the CRC-64/NVME checksum
      in the same single streaming pass that produces the authoritative BLAKE3
      hash (no second read) and hand it to the object store as the upload
      checksum; keep BLAKE3 the sole admission authority; keep non-CRC-64/NVME
      providers at today's behavior with no new on-disk file shape. Implements
      `hef-write-path` — "Provider upload checksum is computed once alongside
      BLAKE3".

## store-file-layer — probed nvme_pi guard tags

- [x] ADD "Probed NVMe protection-information guard tags are a device-verified
      precheck": add the `nvme_pi` capability-descriptor entry, reported present
      only on a PI-formatted namespace with a CRC-64/NVME guard tag and the Linux
      6.14 per-IO integrity attributes on io_uring read/write; attach guard tags
      on writes and verify on reads where present; fall back to today's no-guard-
      tag portable path where absent; keep verification a non-authoritative,
      observably-equivalent precheck with no new on-disk file shape and no
      operator key. Implements `store-file-layer` — "Probed NVMe
      protection-information guard tags are a device-verified precheck".

## hef-hardware-deployment — hint-only PI posture

- [x] ADD "NVMe protection information is a probed, hint-only integrity layer":
      state that PI is discovered by the `nvme_pi` probe, is never required, needs
      no config key, never changes correctness or the authoritative BLAKE3
      outcome, and falls back to the portable path with identical durable bytes —
      mirroring "NVMe data placement is a hint layer" applied to protection
      information. Implements `hef-hardware-deployment` — "NVMe protection
      information is a probed, hint-only integrity layer".

## Code — object-store provider checksum plumbing (req: object-store "Full provider capabilities — byte ranges, multipart uploads, and conditional writes")

- [x] Extend the provider capability descriptor with the supported server-side
      checksum algorithms, populated per backend (S3 → CRC-64/NVME, GCS → CRC32C,
      Azure → its native set or none, local directory → none, in-memory provider →
      none).
- [x] On upload, where the descriptor reports CRC-64/NVME, pass the computed
      checksum as the `object_store`/`opendal` multipart or full-object checksum
      so the provider verifies at commit and stores it; on download, evaluate a
      provider-returned CRC-64/NVME checksum as a precheck before the
      authoritative BLAKE3 check. Surface the choice only through diagnostics.

## Code — write-path streaming checksum (req: hef-write-path "Provider upload checksum is computed once alongside BLAKE3")

- [x] Fold a `crc-fast` CRC-64/NVME accumulator into the single streaming pass
      that already computes the file's authoritative BLAKE3 hash on publish, and
      hand the result to the object store as the upload checksum; compute the
      provider's native checksum or none where the descriptor is not
      CRC-64/NVME. No second read of the object.

## Code — store-file-layer nvme_pi probe and guard tags (req: store-file-layer "Probed NVMe protection-information guard tags are a device-verified precheck")

> Guard-tag work stays inside the sanctioned unsafe surface (`storage::file` /
> the io_uring-ops crate it calls); the in-memory simulation backend is untouched
> and keeps reporting `nvme_pi` absent.

- [x] Extend the startup capability probe to discover `nvme_pi`: namespace PI
      format with a CRC-64/NVME guard tag plus the Linux 6.14 per-IO integrity
      attributes on io_uring read/write; the conservative "absent" answer stays a
      valid result.
- [x] Where confirmed, attach a CRC-64/NVME guard tag on each block-file write and
      request device verification on each read via the 6.14 attributes; otherwise
      keep today's no-guard-tag path. Written-extent reporting, read-bounds
      checks, and torn-tail recovery are unchanged.

## Code — deployment posture (req: hef-hardware-deployment "NVMe protection information is a probed, hint-only integrity layer")

- [x] Confirm no operator config key, startup check, or health signal makes PI
      required or toggles guard tags; the posture is surfaced only through
      diagnostics/metrics, and a PI-absent host starts and runs unchanged.

## Tests

- [x] `object-store`: upload/download parity with a CRC-64/NVME-capable provider
      and with a none/CRC32C provider — identical committed bytes, BLAKE3 outcome,
      visibility, and error taxonomy; a provider that reports a checksum mismatch
      at commit rejects the upload and the object never becomes visible; a
      download whose provider checksum mismatches refuses before BLAKE3.
- [x] `hef-write-path`: publishing the same HEF to a CRC-64/NVME provider and to a
      none provider yields the same file identity and the same authoritative
      BLAKE3 result; the checksum is computed without a second read; bytes that
      fail BLAKE3 never become publication-eligible even if a provider precheck
      would pass them.
- [x] `store-file-layer`: append/sync/read-back parity with `nvme_pi` reported
      present and absent — identical durable bytes, offsets, written extent, read
      results, and error taxonomy; an injected sector corruption is caught as a
      device precheck where present and by BLAKE3 where absent, and is never
      served either way.
- [x] Simulation: the in-memory provider and in-memory file backends run unchanged
      reporting no CRC-64/NVME and no `nvme_pi`, and every simulated run stays
      reproducible from its seed.

## Companion edits (applied at archive, not part of the requirement deltas)

- [x] `hef-hardware-deployment/hardware-detail.md`: note the `nvme_pi` guard-tag
      precheck, the PI hint-only posture, and the provider CRC-64/NVME upload/
      download prechecks.
- [x] `configuration/operator-config-registry.md` and
      `configuration/code-parameter-registry.md`: confirm no new operator key
      exists for provider checksums or NVMe protection information — they follow
      the provider descriptor and the `nvme_pi` probe, not `harana.toml`.

## Verification

- [x] `openspec validate add-hef-end-to-end-crc64nvme --strict` green; every
      `### Requirement:` in the deltas carries SHALL or MUST and at least one
      `#### Scenario:`.
