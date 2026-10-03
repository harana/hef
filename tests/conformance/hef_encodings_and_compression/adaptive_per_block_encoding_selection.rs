//! Checks that the file records exactly how each block of column data was packed (its chosen encoding), so a reader can
//! always unpack it again. Every block's recorded recipe must decode back to a valid combination of transform,
//! compression, and value type.

use crate::support;
use hef::layout::reader::HefFile;

/// conformance: hef-encodings-and-compression/adaptive-per-block-encoding-selection/pipeline-recorded
#[test]
fn pipeline_recorded() {
    // Every column block's chosen pipeline id is recorded in its mark (and the page metadata mirrors the stats), and
    // the recorded id decodes back to a valid (transform, compression, kind) triple.
    let built = support::built_file(40);
    let file = HefFile::open(built.bytes, None).unwrap();
    assert!(!file.footer().marks.is_empty());
    for mark in &file.footer().marks {
        mark.codec_pipeline_id.transform().unwrap();
        mark.codec_pipeline_id.compression().unwrap();
        mark.codec_pipeline_id.value_kind().unwrap();
        assert!(
            file.footer()
                .page_stats
                .iter()
                .any(|stats| { stats.column_id == mark.column_id && stats.granule_id == mark.granule_id })
        );
    }
}
