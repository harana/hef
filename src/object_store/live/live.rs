use super::*;
use crate::error::{PublishError, StorageError};
use crate::invariants::PublishedSet;
use crate::lifecycle::ManifestGeneration;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The production file catalogue: publishes each new version of the catalogue to the application's object store, so
/// every leader and reader agrees on which files queries may read without any lock service.
///
/// Each generation is its own create-only object (`If-None-Match: *`), so a racing publisher can never overwrite one. A
/// small pointer object names the current generation and moves only by compare-and-swap: `head` remembers the
/// pointer's ETag, and `advance_head` writes with `If-Match` on it, so a publisher that read a head which has since
/// moved loses with `CasLost` and must rebase. Generations are stored log-structured - a full checkpoint every
/// `CHECKPOINT_INTERVAL` generations and, in between, only the changes since that checkpoint - so publishing costs the
/// same however many files the catalogue holds. A store with no pointer object yet reads as the empty generation 0.
///
/// The store must offer conditional writes; see [`ObjectStore`] for the degraded mode when it does not.
///
/// See: hef-manifest-integration/spec.md
pub struct LivePublishedSet {
    /// The pointer as this instance last read or wrote it; the ETag `advance_head` compares against.
    head_read: Mutex<Option<HeadRead>>,
    /// Key prefix every catalogue object lives under, such as a deployment root.
    prefix: String,
    store: Arc<dyn ObjectStore>,
}

/// The pointer object as one read saw it.
#[derive(Debug, Clone)]
struct HeadRead {
    /// `None` when no pointer object exists yet, so the first advance must create it.
    etag: Option<ETag>,
    generation: u64,
}

impl LivePublishedSet {
    /// A catalogue kept in `store` under `prefix`. Nothing is read until the first call.
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            head_read: Mutex::new(None),
            prefix: prefix.into(),
            store,
        }
    }

    fn head_key(&self) -> String {
        format!("{}/head", self.prefix)
    }

    fn generation_key(&self, id: u64) -> String {
        format!("{}/generations/{id:020}", self.prefix)
    }

    fn remembered_head(&self) -> MutexGuard<'_, Option<HeadRead>> {
        self.head_read.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads the pointer object and remembers its ETag for the next `advance_head`.
    fn read_head(&self) -> Result<HeadRead, PublishError> {
        let read = match self.store.get(&self.head_key()).map_err(backend)? {
            None => HeadRead {
                etag: None,
                generation: 0,
            },
            Some(object) => HeadRead {
                generation: simdutf8::basic::from_utf8(&object.bytes)
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .ok_or_else(|| PublishError::Io {
                        detail: "catalogue head pointer does not hold a generation id".to_owned(),
                    })?,
                etag: Some(object.etag),
            },
        };
        *self.remembered_head() = Some(read.clone());
        Ok(read)
    }
}

fn backend(error: StorageError) -> PublishError {
    PublishError::Io {
        detail: error.to_string(),
    }
}

impl PublishedSet for LivePublishedSet {
    fn head(&self) -> Result<(u64, ManifestGeneration), PublishError> {
        let read = self.read_head()?;
        Ok((read.generation, self.generation(read.generation)?))
    }

    fn put_generation(&mut self, generation: ManifestGeneration) -> Result<(), PublishError> {
        let id = generation.generation;
        if id == 0 {
            // Generation 0 is the implicit empty catalogue every store starts from.
            return Err(PublishError::GenerationExists);
        }
        let checkpoint = id - id % constant::CHECKPOINT_INTERVAL;
        let stored = if checkpoint == id {
            model::StoredGeneration::Checkpoint(generation)
        } else {
            model::StoredGeneration::Delta(model::GenerationDelta::between(
                &self.generation(checkpoint)?,
                &generation,
            ))
        };
        let bytes = serde_json::to_vec(&stored).map_err(|error| PublishError::Io {
            detail: format!("catalogue generation {id} does not encode: {error}"),
        })?;
        match self
            .store
            .put_if_absent(&self.generation_key(id), &bytes)
            .map_err(backend)?
        {
            PutOutcome::Written { .. } => Ok(()),
            PutOutcome::PreconditionFailed => Err(PublishError::GenerationExists),
        }
    }

    fn advance_head(&mut self, expected: u64, next: u64) -> Result<(), PublishError> {
        if self.store.stat(&self.generation_key(next)).map_err(backend)?.is_none() {
            return Err(PublishError::UnknownGeneration);
        }
        let remembered = self
            .remembered_head()
            .clone()
            .filter(|read| read.generation == expected);
        let read = match remembered {
            Some(read) => read,
            None => self.read_head()?,
        };
        if read.generation != expected {
            return Err(PublishError::CasLost {
                current_generation: read.generation,
            });
        }
        let pointer = next.to_string().into_bytes();
        let outcome = match &read.etag {
            None => self.store.put_if_absent(&self.head_key(), &pointer),
            Some(etag) => self.store.put_if_match(&self.head_key(), &pointer, etag),
        }
        .map_err(backend)?;
        match outcome {
            PutOutcome::Written { etag } => {
                *self.remembered_head() = Some(HeadRead {
                    etag: Some(etag),
                    generation: next,
                });
                Ok(())
            }
            // The pointer moved after this instance read it: one more read reports where it went.
            PutOutcome::PreconditionFailed => Err(PublishError::CasLost {
                current_generation: self.read_head()?.generation,
            }),
        }
    }

    fn generation(&self, id: u64) -> Result<ManifestGeneration, PublishError> {
        let Some(object) = self.store.get(&self.generation_key(id)).map_err(backend)? else {
            return if id == 0 {
                Ok(ManifestGeneration::default())
            } else {
                Err(PublishError::UnknownGeneration)
            };
        };
        let stored: model::StoredGeneration =
            serde_json::from_slice(&object.bytes).map_err(|error| PublishError::Io {
                detail: format!("catalogue generation {id} does not decode: {error}"),
            })?;
        match stored {
            model::StoredGeneration::Checkpoint(generation) => Ok(generation),
            // A delta always names an earlier checkpoint; refusing anything else keeps a corrupt object from looping.
            model::StoredGeneration::Delta(delta) if delta.checkpoint < id => {
                let checkpoint = self.generation(delta.checkpoint)?;
                Ok(delta.apply(checkpoint))
            }
            model::StoredGeneration::Delta(_) => Err(PublishError::Io {
                detail: format!("catalogue generation {id} names a checkpoint that is not earlier than itself"),
            }),
        }
    }
}

#[cfg(test)]
#[path = "test/live.rs"]
mod tests;
