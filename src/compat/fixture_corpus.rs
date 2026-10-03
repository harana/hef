//! Keeps old files readable forever by proving the current reader still opens every released-version fixture
//! committed under the corpus directory.
//!
//! Until format freeze the corpus is empty (or the directory absent) and the sweep trivially passes; at freeze, each
//! released writer version commits one fixture file here, and from then on any reader change that breaks an old file
//! fails the sweep with that file's name.
//!
//! See: hef-reader-compatibility/spec.md

use crate::error::FormatError;
use crate::layout::reader::HefFile;
use std::path::{Path, PathBuf};

/// Directory the released-version fixtures live in, relative to this crate's root.
pub const FIXTURE_CORPUS_DIR: &str = "test_data/hef-fixtures";

/// The format version the generator stamps fixtures for. Bumping the writer's version without updating this constant
/// (and committing a fixture for the old version) fails fixture generation — the freeze guard.
pub const FIXTURE_WRITER_VERSION: (u16, u16) = (1, 0);

/// What one corpus sweep found: every fixture that opened, and every fixture that no longer does.
#[derive(Debug)]
pub struct FixtureSweep {
    pub failures: Vec<(String, FormatError)>,
    pub opened: Vec<String>,
}

/// Opens every `.hef` fixture under `dir` with the current reader. An absent directory is an empty corpus — the
/// pre-freeze state — not an error. Fixture files are read in name order so a failing sweep reports deterministically.
pub fn read_fixture_corpus(dir: &Path) -> std::io::Result<FixtureSweep> {
    let mut sweep = FixtureSweep {
        failures: Vec::new(),
        opened: Vec::new(),
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(sweep),
        Err(error) => return Err(error),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "hef"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = std::fs::read(&path)?;
        match HefFile::open(bytes, None) {
            Ok(_) => sweep.opened.push(name),
            Err(error) => sweep.failures.push((name, error)),
        }
    }
    Ok(sweep)
}

/// Writes one built file into `dir` as a corpus fixture named `hef-v<major>.<minor>-<label>.hef`, refusing when the
/// build's declared format version disagrees with [`FIXTURE_WRITER_VERSION`] — so a version bump cannot silently
/// generate fixtures stamped with the wrong version.
#[cfg(feature = "write")]
pub fn generate_fixture(dir: &Path, label: &str, built: &crate::writer::build::BuiltHef) -> std::io::Result<PathBuf> {
    if built.footer.format_version != FIXTURE_WRITER_VERSION {
        return Err(std::io::Error::other(
            "fixture generator writer version disagrees with the built file's declared format version",
        ));
    }
    std::fs::create_dir_all(dir)?;
    let (major, minor) = FIXTURE_WRITER_VERSION;
    let path = dir.join(format!("hef-v{major}.{minor}-{label}.hef"));
    std::fs::write(&path, &built.bytes)?;
    Ok(path)
}

#[cfg(all(test, feature = "write"))]
#[path = "test/fixture_corpus.rs"]
mod tests;
