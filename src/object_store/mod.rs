//! Keeps HEF's shared state in the application's object store: the uploaded HEF files and the versions of the file
//! catalogue that say which of them queries may read.
//!
//! The application supplies the store itself through [`ObjectStore`] — an S3-style interface with create-only and
//! If-Match conditional writes, ETags, and multipart uploads — so HEF depends on no particular client. On top of it
//! HEF provides [`LivePublishedSet`], the production catalogue: create-only generation objects, a head pointer that
//! moves only by compare-and-swap on its ETag, and log-structured generations that store only what changed since the
//! last full checkpoint. The catalogue implementation lives here in HEF, beside the interface it needs, so every
//! application gets the same conditional-write discipline instead of re-deriving it. [`sim`] holds the in-memory test
//! store.
//!
//! See: hef-manifest-integration/spec.md

pub mod api;
pub mod constant;
#[path = "live/live.rs"]
pub mod live;
pub mod model;
pub mod sim;

pub use api::{ObjectStore, hef_object_key};
pub use live::*;
pub use model::{ETag, MultipartUpload, ObjectStat, PutOutcome, StoredObject, UploadedPart};
