# HEF Reader Compatibility and Versioning

Companion artifact for the `hef-reader-compatibility` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


HEF must support forward-compatible extension blocks.

Footer metadata:

```text
format_version
required_feature_flags
optional_feature_flags
schema_fingerprint
logical_schema
physical_schema
column_directory
stripe_directory
skip_index_directory
bitmap_index_directory
aggregate_directory
sketch_directory
context_directory optional
embedding_directory optional internal only
encryption_metadata
checksum_directory
```

Reader rule:

```text
Unknown required feature  -> fail file.
Unknown optional feature  -> ignore block.
Known feature             -> validate checksums and use.
```

Format feature flags must include whether the file uses optional blocks such as context projections, vector blocks, sparse cubes, encrypted footers, projections, compact/wide layout class, HEF-native deletion vectors, variant_shredded_field_blocks, variant_path_mphf_blocks, path_presence_indexes, Ribbon filters, or split-block Bloom filters.

---

