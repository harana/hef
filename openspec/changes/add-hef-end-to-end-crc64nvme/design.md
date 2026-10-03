The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/add-hef-end-to-end-crc64nvme/).

# Design — End-to-end CRC-64/NVME: provider-verified uploads and device-verified sectors

Four decisions, in the tension → decision → why → rejected → spec-edits style of
`docs/design-review-decisions.md`. The running theme: the platform already
computes CRC-64/NVME for its own prechecks, and the same value is verifiable for
free at the two ends of the format — the storage provider on upload and the NVMe
device on read. Both are non-authoritative precheck layers slotted into
frameworks that already exist (the provider capability descriptor; the startup
fast-path probe), with BLAKE3 unchanged as the sole admission authority and a
portable fallback that is exactly today's behavior.

## 1. Provider checksum algorithms belong in the capability descriptor, not a new interface

- **Tension.** S3's `CRC64NVME` upload verification could surface as a new
  provider method ("upload with this checksum"), or as a property of the
  provider the existing upload path consults. A new method makes every caller
  reason about which providers verify; a descriptor property keeps the upload
  interface unchanged and lets the store decide from what the provider reports.
- **Decision.** Extend the existing provider capability descriptor — which
  already names the conditional-write primitive, range reads, and multipart
  limits — with the server-side checksum algorithms the provider supports. Where
  it reports CRC-64/NVME, the store sends that checksum on upload and reads it
  back as a download precheck; where it reports CRC32C (GCS), another algorithm,
  or none (local directory), the store degrades to that or to today's behavior.
  No new provider interface method; the choice follows the descriptor.
- **Why.** This mirrors how every other native capability is handled: the
  descriptor names what the provider supports and the one provider interface
  drives it. A checksum is precheck metadata, not a new operation. The
  authoritative BLAKE3 rules already hold "regardless of which capability moved
  the bytes," so a provider checksum slots in as one more non-authoritative
  precheck without touching admission.
- **Rejected.** *A dedicated upload-with-checksum method.* — Pushes
  provider-specific verification into every caller and into the in-memory
  provider, which would have to model it; the descriptor already exists to drive
  exactly these decisions. *Making the provider checksum authoritative on
  download.* — Would create a second integrity authority and break the single-
  authority invariant; the provider checksum can only reject early, never admit.
- **Spec edits.** `object-store` MODIFY "Full provider capabilities — byte
  ranges, multipart uploads, and conditional writes".

## 2. The provider checksum is computed once, folded into the BLAKE3 pass

- **Tension.** The upload checksum could be computed in a second read of the
  staged object (simple, but doubles the I/O and CPU over the bytes), or folded
  into the single streaming pass that already computes the authoritative BLAKE3
  hash on the write path.
- **Decision.** Compute the CRC-64/NVME provider checksum in the same single
  streaming pass as BLAKE3, over the same bytes, and hand it to the object store
  as the upload checksum. Never re-read the object to checksum it.
- **Why.** The write path already streams the bytes once to hash them; adding a
  CRC-64/NVME accumulator to that pass is nearly free (`crc-fast` is already the
  implementation), whereas a second read would erase the "cheapest real win"
  framing. Publication-eligibility still waits on authoritative BLAKE3, so the
  provider checksum changes only what the provider rejects early, never what is
  admitted.
- **Rejected.** *A second pass to checksum before upload.* — Doubles I/O for a
  precheck and contradicts the streaming discipline the write path already uses.
  *Computing the checksum inside the object-store layer.* — The write path is
  where the bytes are already being hashed; computing it there avoids a redundant
  read and keeps the object store consuming a ready checksum.
- **Spec edits.** `hef-write-path` ADD "Provider upload checksum is computed once
  alongside BLAKE3".

## 3. `nvme_pi` is one more probed fast-path entry, guard tags are a device precheck

- **Tension.** NVMe protection information could be treated as a new integrity
  subsystem the platform owns (attaching, stripping, and reconciling guard tags
  across the whole stack), or as one more capability-probed precheck the device
  performs on bytes the platform already trusts to BLAKE3.
- **Decision.** Add a single `nvme_pi` entry to the shared file layer's existing
  startup capability descriptor, reported present only when the namespace is
  PI-formatted with a CRC-64/NVME guard tag *and* the kernel exposes the Linux
  6.14 per-IO integrity attributes on io_uring read/write. Where present, attach
  a guard tag on write and let the device verify on read; where absent, use
  today's portable path with no guard tags. Verification is a non-authoritative
  precheck, observably equivalent to the portable path.
- **Why.** This is exactly the probe → use → portable fallback → observable
  equivalence pattern every storage fast path already follows
  (INV-HARDWARE-ACCEL). The 6.14 attributes let userspace attach and verify
  guard tags without owning the integrity stack, so the platform stays a
  consumer of a device feature, not the owner of a subsystem. A zeroed or
  corrupt sector is caught by the device as a precheck, but BLAKE3 still gates
  admission, so correctness is untouched.
- **Rejected.** *Owning the full PI integrity stack.* — Far more surface than a
  precheck warrants and would entangle the platform with metadata-format and
  reconciliation concerns the device already handles. *A `hef-hardware-
  deployment` config key to force PI on.* — Violates INV-HARDWARE-ACCEL's
  no-operator-key rule; the probe decides, diagnostics report.
- **Spec edits.** `store-file-layer` ADD "Probed NVMe protection-information
  guard tags are a device-verified precheck".

## 4. PI is a hint layer in the deployment posture, exactly like data placement

- **Tension.** A PI-capable deployment could be told PI is required for
  correctness (refuse without it), or that PI is a hint-only precheck the
  platform tolerates but never depends on.
- **Decision.** State PI as a probe-gated, hint-only integrity layer in
  `hef-hardware-deployment`, parallel to the existing "NVMe data placement is a
  hint layer" requirement: data stays correct and readable without PI, no config
  key turns guard tags on or off, and a PI absence or verification failure falls
  back to the portable path with identical durable bytes. BLAKE3 remains the sole
  admission authority in both the upload and device directions.
- **Why.** PI improves *when* a corrupt sector is caught, never *whether* the
  data is correct — that is BLAKE3's job. Treating it as a hint keeps
  deployments portable (a non-PI namespace is fully supported) and keeps the
  posture consistent with how FDP/ZNS placement is already framed. The guard tags
  add no on-disk file shape, so there is no reader-compatibility or feature-flag
  concern.
- **Rejected.** *Refuse without PI.* — Would make a device format a
  correctness dependency, contradicting the portable-fallback invariant and
  excluding every non-PI deployment. *A separate PI subsystem spec.* — The
  posture is one requirement; the mechanism lives in `store-file-layer`'s probe.
- **Spec edits.** `hef-hardware-deployment` ADD "NVMe protection information is a
  probed, hint-only integrity layer".
