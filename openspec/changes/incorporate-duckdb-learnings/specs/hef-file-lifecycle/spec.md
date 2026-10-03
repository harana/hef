## ADDED Requirements

### Requirement: Compaction scheduling with an anti-thrashing bound
The lifecycle SHALL define *when* compaction runs, not only how a rewrite works. A scheduling policy SHALL select candidates as runs of sequence-adjacent small files within a tenant, size-tiered, with a bounded merge width per job. The policy SHALL enforce an anti-thrashing doubling bound: a run SHALL NOT be merged unless the merged output would be at least roughly twice the largest input or would reach the publish policy's roll byte target — so the trickle-sealed tail file of a low-volume tenant is not rewritten at every cycle, the quadratic write-amplification failure the bound exists to prevent, which is costlier here than in a mutable store because every merge is a new manifest generation and object-store round trips. Work per cycle SHALL be bounded, and the write-amplification gates of `hef-benchmarks-and-acceptance-gates` SHALL be the policy's acceptance test: a policy whose measured rewrite WAF exceeds the gated ceiling is not ready. Within a file, rewrite scope selection stays per-granule `clustering_quality`, and every merge remains an ordinary HEF rewrite under `hef-write-path` — same format, atomic generation publication, safe retirement. The policy's thresholds SHALL be pinned code parameters, never operator keys.

#### Scenario: A trickle tenant's tail is not rewritten every cycle
- **WHEN** a low-volume tenant seals a small compact file every roll window
- **THEN** the newest files are merged only once their run satisfies the doubling bound or reaches the roll byte target, not on every compaction cycle

#### Scenario: Accumulated small files do get merged
- **WHEN** a tenant has accumulated a run of adjacent small files whose merged size satisfies the doubling bound
- **THEN** a compaction job merges up to the bounded width of them into one larger file in a new generation, and repeated cycles converge the tenant toward roll-target-sized files

#### Scenario: Work per cycle is bounded
- **WHEN** many tenants have eligible runs at once
- **THEN** each cycle performs at most its bounded work, deferring the rest to later cycles rather than rewriting everything at once

#### Scenario: The WAF gate rejects a thrashing policy
- **WHEN** a candidate policy's measured rewrite write-amplification exceeds the gated ceiling on the benchmark workload
- **THEN** the policy is not marked ready, and the gate—not observation alone—holds the bound
