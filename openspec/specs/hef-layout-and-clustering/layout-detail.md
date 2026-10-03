# HEF Sorting, Clustering, Projections, and Workload-Driven Layout

Companion artifact for the `hef-layout-and-clustering` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


HEF has one file format and may use declared projections for alternate sort orders.

Default primary projection:

```text
sort by tenant_id internal, epoch, sequence
cluster by occurred_at bucket when it does not break sequence-range coverage metadata
```

Allowed projection types:

```text
time_major
  sort by occurred_at, epoch, sequence;
  optimized for dashboards, counts, period comparisons, and time buckets.

entity_major
  sort by entity_id_hash, occurred_at, epoch, sequence;
  optimized for entity/account/customer/opportunity timelines.

source_type_major
  sort by source_id, event_type_id, occurred_at, epoch, sequence;
  optimized for alert/rule scans and event-type dashboards.

context_major
  sort by evidence_group_key, occurred_at, epoch, sequence;
  optimized for chat and investigation context retrieval.

revenue_metric_major
  sort by metric_period, metric_key, entity refs, occurred_at, epoch, sequence;
  used only when a physical event projection is better than a PreparedView.
```

Projection rules:

```text
A projection is a read alternative over the same logical rowset.
A projection has its own marks, column data, local indexes, and aggregate summaries.
A projection shares file-level deletion vectors and correction generations with the primary rowset.
A query snapshot chooses at most one projection per logical sequence range.
A projection must not cause double-counting.
```

Z-order and Hilbert curves are not primary clustering strategies. Explicit projections are preferred because the named Harana query patterns need predictable ordering, direct marks, and clear manifest accounting.

Reclustering at rewrite is incremental, not whole-table:

```text
incremental clustering model (liquid-style)
  each granule carries a clustering_quality score per projection
  (key-range overlap with sibling granules, sortedness, size skew);
  HEF rewrite selects only granules below the quality threshold plus newly
  published unclustered ranges, and rewrites those into well-clustered granules;
  full-file or full-projection resorts are an explicit operator action, never a
  background default;
  clustering keys may change over time without rewriting history: a projection's
  declared sort order applies from its declaring generation forward, and the
  manifest records per-file effective clustering metadata.
```

HEF rewrite may add, remove, or rebuild projections when workload evidence justifies the read/write amplification.

---
