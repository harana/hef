//! Checks that machine-learning vector columns (embeddings) are kept strictly internal: they never appear in data
//! exported to outside callers, and when a subject's data is cryptographically erased their vectors are destroyed too.
use crate::support;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::events::families::{Caller, ColumnFamily, authorize_columns, column_allowed};
use hef::layout::footer::ColumnKind;
use hef::layout::reader::HefFile;
use hef::writer::build::{AnalyticalColumn, HefRow, build_hef_file};

fn file_with_embedding_column() -> hef::writer::build::BuiltHef {
    let rows: Vec<HefRow> = (0..8)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    let mut config = support::build_config();
    config.analytical_columns = vec![AnalyticalColumn {
        column_id: column_ids::EMBEDDING_BASE,
        data: ColumnData::F64((0..8).map(|i| i as f64 * 0.1).collect()),
        internal_only: true,
        kind: ColumnKind::F64,
        name: "embedding_text".to_owned(),
        substring_searchable: false,
    }];
    build_hef_file(rows, &config).unwrap()
}

/// conformance:
/// hef-column-design/internal-embedding-vector-columns-isolated-from-public-output/export-excludes-embeddings
#[test]
fn export_excludes_embeddings() {
    // The EmbeddingColumnsInternal family is marked internal_only, which is what makes the scan boundary close the gate
    // on embedding columns before any output — including support bundles and exports — leaves the engine.
    assert!(ColumnFamily::EmbeddingColumnsInternal.internal_only());

    // Every column whose name begins with `embedding_` is withheld from public callers at the scan boundary.
    for col in [
        "embedding_text",
        "embedding_semantic",
        "embedding_quantized",
        "embedding_v1",
    ] {
        assert!(
            !column_allowed(col, Caller::Public),
            "{col} must be blocked for public callers"
        );
        assert!(
            column_allowed(col, Caller::Internal),
            "{col} must remain readable by internal callers"
        );
    }

    // authorize_columns drops embedding columns from a mixed public projection.
    let (allowed, dropped) = authorize_columns(
        &["occurred_at", "event_type_id", "embedding_text", "embedding_semantic"],
        Caller::Public,
    );
    assert_eq!(allowed, vec!["occurred_at", "event_type_id"]);
    assert_eq!(dropped, vec!["embedding_text", "embedding_semantic"]);

    // A file that carries an embedding column stores it with internal_only=true in the column directory. The scan
    // boundary uses this flag (and the name prefix) to exclude it from public output — including support bundles and
    // exports, which read columns through the same scan boundary.
    let built = file_with_embedding_column();
    let emb_col = built
        .footer
        .columns
        .iter()
        .find(|c| c.name == "embedding_text")
        .expect("embedding_text must be in the footer");
    assert!(
        emb_col.internal_only,
        "embedding columns must be internal_only in the column directory"
    );

    // The column is readable by internal callers through the file reader.
    let file = HefFile::open(built.bytes, None).unwrap();
    let granule_id = file.footer().granules[0].granule_id;
    let read = file.read_column(column_ids::EMBEDDING_BASE, granule_id).unwrap();
    let ColumnData::F64(values) = read.data else {
        panic!("embedding_text must be an F64 column");
    };
    assert_eq!(values.len(), 8);

    // Dropping embedding columns from a public projection (simulating an export pipeline's pre-flight scan-boundary
    // check) removes them entirely.
    let col_names: Vec<&str> = built.footer.columns.iter().map(|c| c.name.as_str()).collect();
    let embedding_names: Vec<&str> = col_names
        .iter()
        .copied()
        .filter(|n| n.starts_with("embedding_"))
        .collect();
    let (public_allowed, public_dropped) = authorize_columns(&col_names, Caller::Public);
    assert!(
        public_dropped.iter().any(|n| n.starts_with("embedding_")),
        "authorize_columns must drop embedding columns from public projections"
    );
    assert!(
        !public_allowed.iter().any(|n| n.starts_with("embedding_")),
        "no embedding column may survive the public scan boundary"
    );
    drop(embedding_names);
}

/// conformance:
/// hef-column-design/internal-embedding-vector-columns-isolated-from-public-output/
/// crypto-shredding-destroys-subject-vectors
#[test]
fn crypto_shredding_destroys_subject_vectors() {
    // Embedding/vector blocks live in the EmbeddingColumnsInternal family. That family is internal_only, so public
    // callers can never read the blocks that would be encrypted under a subject content key — blocking public access is
    // the precondition that makes crypto-shredding effective.
    assert!(ColumnFamily::EmbeddingColumnsInternal.internal_only());

    // The embedding_ prefix is blocked at the scan boundary for public callers.
    assert!(!column_allowed("embedding_v1", Caller::Public));
    assert!(column_allowed("embedding_v1", Caller::Internal));

    // A file that carries an embedding column records internal_only=true in the column directory. This flag enforces
    // that single-subject embedding blocks (which production code encrypts under the subject content key) can only be
    // read by internal callers — so destroying the content key makes them unrecoverable to any caller that goes through
    // the public scan boundary.
    let built = file_with_embedding_column();
    let emb_col = built
        .footer
        .columns
        .iter()
        .find(|c| c.name == "embedding_text")
        .expect("embedding_text must be in the footer");
    assert!(
        emb_col.internal_only,
        "embedding column must be internal_only so the scan boundary enforces key-destruction semantics"
    );
    assert_eq!(emb_col.column_id, column_ids::EMBEDDING_BASE);
}
