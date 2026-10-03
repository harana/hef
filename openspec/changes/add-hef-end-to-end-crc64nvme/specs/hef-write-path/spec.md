## ADDED Requirements

### Requirement: Provider upload checksum is computed once alongside BLAKE3
When the write path publishes a HEF to a durable provider whose capability descriptor reports CRC-64/NVME support, it SHALL compute that CRC-64/NVME checksum **once, streaming over the same bytes as they are hashed**, in the same single pass that already produces the file's authoritative BLAKE3 hash — never in a second read of the object. The computed checksum SHALL be handed to the object store to send as the provider's multipart or full-object checksum so the provider verifies the upload server-side at commit, matching the object-store capability "Full provider capabilities — byte ranges, multipart uploads, and conditional writes". BLAKE3 SHALL remain the sole admission authority on the write path (INV-HARDWARE-ACCEL and the "BLAKE3 is the authoritative integrity check" rule are unchanged): the provider checksum is a non-authoritative precheck that MAY cause the provider to reject a corrupted upload early but SHALL NEVER admit bytes the authoritative BLAKE3 check would reject, and a HEF SHALL become publication-eligible only after its authoritative BLAKE3 verification passes. Where the provider descriptor reports a different native algorithm or none — GCS's CRC32C, Azure, or the local-directory default — the write path SHALL compute the provider's native checksum or none, exactly as it does today; the streaming computation SHALL add no new on-disk HEF file shape and SHALL define no operator config key.

#### Scenario: One streaming pass produces both hashes
- **WHEN** a HEF is published to a provider whose descriptor reports CRC-64/NVME support
- **THEN** the CRC-64/NVME checksum is computed in the same single streaming pass that computes the authoritative BLAKE3 hash, with no second read of the object, and is handed to the object store as the provider's upload checksum

#### Scenario: BLAKE3 stays the admission authority on publish
- **WHEN** bytes pass the provider's CRC-64/NVME check but fail the authoritative BLAKE3 verification on the write path
- **THEN** the HEF is rejected and never becomes publication-eligible, and the provider precheck never admits it

#### Scenario: Non-CRC-64/NVME provider uses today's behavior
- **WHEN** the durable provider reports a native CRC32C, another algorithm, or no server-side checksum
- **THEN** the write path computes that native checksum or none exactly as today, adds no new on-disk file shape, and the publish sequence and BLAKE3 outcome are unchanged
