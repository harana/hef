# HEF File Lifecycle

Companion artifact for the `hef-file-lifecycle` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


HEF uses part-state names that match rewrite semantics and query snapshot safety.

```text
OpenTmp
  File is being written. Not queryable.

Sealed
  Footer and checksum are valid. Not yet visible.

Active
  Manifest references the file and coverage. Queryable.

Outdated
  Replaced by newer generation files but still retained for readers on previous snapshots.

DeleteOnDestroy
  No longer used by any active/readable manifest snapshot; waiting for sweeper deletion.

Deleted
  Removed after retention and safety window.
```

Only `Active` files may be selected for new public query snapshots. `Outdated` files may remain readable only for in-flight snapshots that already selected them.

---
