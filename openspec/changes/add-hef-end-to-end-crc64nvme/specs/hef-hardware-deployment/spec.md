## ADDED Requirements

### Requirement: NVMe protection information is a probed, hint-only integrity layer
The deployment MAY run its journal and store-file paths on an NVMe namespace formatted with **end-to-end protection information** (PI), whose per-sector guard tag is CRC-64/NVME, so the device itself can catch a sector corrupted on the media. Whether PI is usable SHALL be discovered by the `nvme_pi` capability probe (`store-file-layer` — "Probed NVMe protection-information guard tags are a device-verified precheck"), which reports it present only when the namespace is PI-formatted with a CRC-64/NVME guard tag and the running kernel exposes the Linux 6.14 per-IO integrity attributes on io_uring read and write. PI SHALL remain a hint-only precheck layer, never a correctness gate: data SHALL remain correct and readable on a namespace without PI (recorded absent at startup), the deployment SHALL NOT require a PI format, SHALL define no operator config key to turn guard tags on or off, and SHALL surface the posture only through diagnostics. Guard-tag attachment and verification SHALL never change saved-record correctness, query results, commit boundaries, object-store visibility, or the authoritative BLAKE3 outcome — BLAKE3 remains the sole admission authority in both the upload and device directions (INV-HARDWARE-ACCEL, INV unchanged) — and a PI absence, format mismatch, or verification failure SHALL fall back to the portable path with identical durable bytes. PI adds no new on-disk file shape: the guard tags live in the device's protection-information area, so a non-PI namespace stores and reads the same file bytes.

#### Scenario: PI absent at startup
- **WHEN** the `nvme_pi` probe records protection information as absent — no PI format, a pre-6.14 kernel, or a non-NVMe host
- **THEN** writes proceed without guard tags and remain correct and readable, no config key or startup check references PI, and the durable bytes are identical to a PI-formatted namespace

#### Scenario: Guard tag is a precheck, not a gate
- **WHEN** a PI-formatted namespace attaches CRC-64/NVME guard tags and the device verifies them on read
- **THEN** a guard-tag mismatch rejects the read as a precheck, but a byte is served only after authoritative BLAKE3 verification, and the saved-record correctness and query results are identical to a host without PI
