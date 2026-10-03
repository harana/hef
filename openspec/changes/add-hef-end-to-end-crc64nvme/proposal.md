The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-end-to-end-crc64nvme/).

Status: Approved (2026-07-09)

## Why

HEF standardized on CRC-64/NVME for its fast prechecks (the archived
`2026-06-14-migrate-hef-crc-to-crc64-nvme` change; `crc-fast` in the
implementation). The same algorithm is now verified for free on *both* sides of
the format, and today the platform uses neither side:

- **Upward, at the provider.** Amazon S3 natively supports `CRC64NVME` as a
  full-object and trailer checksum (GA December 2024, the SDK default), so a
  provider can verify an upload server-side at commit and store the checksum for
  later re-verification. Nothing in `object-store` or the write path passes a
  checksum algorithm to the provider today; the provider capability descriptor
  does not mention checksums at all. This is the cheapest real win in the review.
- **Downward, at the device.** NVMe end-to-end protection information (PI) uses a
  CRC-64/NVME guard tag per 4 KiB+ sector, and Linux 6.14 added per-IO integrity
  attributes on io_uring read and write, so userspace can attach guard tags on
  writes and let the device verify them on reads without owning the whole
  integrity stack. Neither PI nor the 6.14 attributes appear in any spec or in
  the kernel triage — the one storage item the 6.0 → 7.1 sweep missed.

Both are correctness-neutral precheck layers over the algorithm the platform
already computes. In both directions BLAKE3 stays the sole admission authority,
so the integrity invariant is unchanged.

## What Changes

**The provider capability descriptor gains checksum algorithms, and uploads/
downloads use them.** `object-store`'s "Full provider capabilities" requirement
is extended so a provider names the server-side checksum algorithms it supports.
Where a provider supports CRC-64/NVME (S3), the uploader sends it as the
multipart/full-object checksum and the provider rejects a corrupted upload at
commit; on download, a provider-returned CRC-64/NVME checksum is one more
precheck evaluated before the authoritative BLAKE3 check. GCS (native CRC32C)
and Azure degrade to their native algorithm or none; a provider with no
server-side checksum (the local-directory default) sends none — in every such
case the behavior is exactly today's. The provider checksum is a
non-authoritative precheck only and defines no operator config key.

**The write path computes the provider checksum once, streaming alongside
BLAKE3.** `hef-write-path` gains a requirement: when publishing to a
CRC-64/NVME-capable provider, the CRC-64/NVME checksum is computed in the same
single streaming pass that already produces the file's authoritative BLAKE3
hash — never a second read — and handed to the object store as the upload
checksum. BLAKE3 remains the sole admission authority; publication-eligibility
still waits on authoritative BLAKE3 verification.

**A probed `nvme_pi` guard-tag capability joins the shared file layer.**
`store-file-layer` gains an `nvme_pi` capability-descriptor entry, reported
present only when the namespace is PI-formatted with a CRC-64/NVME guard tag and
the kernel exposes the Linux 6.14 per-IO integrity attributes on io_uring read/
write. Where present, the block-file path attaches guard tags on writes and lets
the device verify on reads — one more device-verified precheck. Where absent
(no PI, pre-6.14 kernel, non-NVMe host), it is today's behavior. Guard-tag
verification is observably equivalent to the portable path, adds no on-disk file
shape, and defines no operator config key.

**The deployment records a hint-only PI posture.** `hef-hardware-deployment`
gains a requirement stating PI is a probe-gated, hint-only integrity layer: data
stays correct and readable without PI, no config key turns it on or off, and a
PI absence or verification failure falls back to the portable path with
identical durable bytes — mirroring the existing "NVMe data placement is a hint
layer" discipline, applied to protection information.

None of this changes the HEF on-disk file shape: provider checksums travel with
the upload, guard tags live in the device's protection-information area, so a
non-CRC-64/NVME provider and a non-PI namespace store and read the same bytes.
There is no new format feature and no reader compatibility concern.

## Capabilities

### Modified Capabilities

- `object-store` — MODIFY "Full provider capabilities — byte ranges, multipart
  uploads, and conditional writes" to add supported server-side checksum
  algorithms to the provider capability descriptor, send CRC-64/NVME as the
  upload checksum where supported (provider rejects a corrupted commit), use a
  provider-returned checksum as a download precheck before BLAKE3, and degrade
  GCS/Azure/local to their native algorithm or none. The provider checksum is a
  non-authoritative precheck; the object-store contract and authoritative BLAKE3
  hold unchanged.

### Added Capabilities

- `hef-write-path` — ADD "Provider upload checksum is computed once alongside
  BLAKE3": the CRC-64/NVME provider checksum is computed in the single streaming
  pass that produces the authoritative BLAKE3 hash and handed to the object
  store; BLAKE3 stays the sole admission authority and non-CRC-64/NVME providers
  keep today's behavior.
- `store-file-layer` — ADD "Probed NVMe protection-information guard tags are a
  device-verified precheck": the `nvme_pi` capability entry, gated on a PI format
  plus the Linux 6.14 io_uring integrity attributes, attaches guard tags on
  writes and verifies on reads as an observably-equivalent, non-authoritative
  precheck with a portable fallback.
- `hef-hardware-deployment` — ADD "NVMe protection information is a probed,
  hint-only integrity layer": PI is probe-gated and hint-only, requires no PI
  format and no config key, never changes correctness, and falls back to the
  portable path with identical durable bytes.

## Impact

- **The provider leg is a near-free correctness win.** On S3 deployments every
  HEF upload is verified server-side at commit and its checksum stored for later
  re-verification, at the cost of one streaming CRC-64/NVME computation folded
  into the pass that already hashes the bytes — no extra read, no format change.
- **The device leg closes the one gap the kernel sweep missed.** On PI-formatted
  NVMe namespaces with a 6.14+ kernel, media corruption is caught by the device
  before the bytes reach the reader, one probe slotted into the existing
  capability framework.
- **BLAKE3 stays the only admission authority (INV unchanged).** Both new
  prechecks may reject early but can never admit bytes BLAKE3 would reject, and
  nothing is served or committed durable on a precheck alone.
- **INV-HARDWARE-ACCEL holds.** Every added path — the provider checksum and the
  `nvme_pi` guard tags — is capability-probed (the provider descriptor / the
  `nvme_pi` probe), has a portable fallback that is today's behavior, is
  observably equivalent byte-for-byte, defines no operator config key, and is
  surfaced only through diagnostics; the software/portable path is the oracle.
- **Deterministic simulation is unaffected.** The in-memory provider and
  in-memory file backends report no CRC-64/NVME and no `nvme_pi` (the
  conservative answer stays valid), so identical production code runs under
  simulation and every run stays reproducible from its seed.

## Open Questions

1. **Trailer vs. full-object checksum on multipart S3 uploads.** Whether the
   uploader sends the CRC-64/NVME checksum as a per-part trailer aggregated to a
   full-object checksum or as a single full-object checksum is an SDK-integration
   detail; the requirement fixes only that the provider verifies it at commit.
2. **Guard-tag application block size.** Whether guard tags are attached at the
   4 KiB sector granularity or a larger metadata-per-IO granularity depends on
   how the namespace is formatted and how the 6.14 attributes expose it; the
   requirement fixes only the probe, the fallback, and observable equivalence.
