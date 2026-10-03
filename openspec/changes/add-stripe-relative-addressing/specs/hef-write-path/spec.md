## ADDED Requirements

### Requirement: Incomplete pipelined uploads leave nothing visible and abort
A pipelined upload that does not reach a successful publish SHALL leave no visible object and SHALL be aborted. When a publish attempt fails a check or loses the manifest commit race after it has uploaded stripe parts, the publisher SHALL abort the multipart upload so no completed object is created, consistent with the publish boundary's "failed publish leaks nothing" rule. Because a multipart upload that is never completed exposes no object, an interrupted or crashed attempt SHALL leave no partial object readable through any pointer or manifest entry. Orphaned incomplete multipart uploads — those left by a crashed publisher that holds no live handle to abort them — SHALL be reaped by object-store lifecycle policy (see `object-store` — "Full provider capabilities — byte ranges, multipart uploads, and conditional writes"), so abandoned parts never accumulate or become visible.

#### Scenario: Lost publish race aborts the upload
- **WHEN** a publish attempt has uploaded stripe parts and then loses the manifest commit race
- **THEN** the publisher aborts the multipart upload, no object is completed at the target path, and no peer notice or public read observes the attempt

#### Scenario: Orphaned incomplete upload is reaped
- **WHEN** a publisher crashes after starting a multipart upload and never completes or aborts it
- **THEN** the incomplete upload exposes no visible object and is reaped by object-store lifecycle policy rather than lingering as a partial object
