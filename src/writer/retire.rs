//! Retires replaced files: publishes a compaction's output in place of the files it merged, and later deletes those
//! files once no query can still be reading them.
//!
//! The application drives the whole cycle; nothing here runs on its own. The expected call pattern is:
//!
//! 1. While a file is open for new journal data, check [`should_roll`](super::publish::should_roll) after each flush
//!    and, when it fires, publish the range with [`HefPublisher::publish_range`].
//! 2. Each compaction cycle, read the head generation and call
//!    [`plan_compaction_cycle`](crate::lifecycle::plan_compaction_cycle) for each tenant. For every job, build one file
//!    from the rows of the job's input files and hand it to [`HefPublisher::publish_compaction`], which publishes one
//!    generation where the output is `Active` and every input is `Outdated`.
//! 3. Periodically - not continuously - call [`sweep_retired_files`] with the oldest generation any running query
//!    still reads. It moves `Outdated` files no query can still select to `DeleteOnDestroy`, and deletes the objects
//!    of `DeleteOnDestroy` files that have waited out the safety window, dropping them from the catalogue.
//!
//! See: hef-file-lifecycle/spec.md

use super::build::{BuiltHef, HefBuildConfig};
use super::publish::{HefPublisher, MAX_REBASE_ATTEMPTS, PeerNotices, PublishFailure, Published};
use super::upload::upload_hef;
use crate::clock::Clock;
use crate::error::PublishError;
use crate::events::{SequenceRange, TenantId};
use crate::invariants::PublishedSet;
use crate::lifecycle::{FileType, HefFileEntry, ManifestGeneration, PartState, Retirement};
use crate::object_store::{ObjectStore, constant::MIN_MULTIPART_PART_BYTES, hef_object_key};
use hashbrown::HashSet;

/// How long a replaced file waits at each step before the sweeper moves it on.
///
/// Both values are operator configuration with no default: the file-lifecycle spec requires concrete values before
/// general availability.
///
/// See: hef-file-lifecycle/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepPolicy {
    /// Nanoseconds a file stays `Outdated` after it was replaced before it may become `DeleteOnDestroy`. Set it to at
    /// least the longest a query is allowed to run.
    pub in_flight_query_horizon_nanos: u64,
    /// Nanoseconds a `DeleteOnDestroy` file waits before its object is deleted. A value below the in-flight-query
    /// horizon is raised to it.
    pub safety_window_nanos: u64,
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Files whose objects were deleted and whose entries left the catalogue (`DeleteOnDestroy -> Deleted`).
    pub deleted: Vec<u128>,
    /// The generation the sweep published, or `None` when nothing was due.
    pub generation: Option<u64>,
    /// Files no live query can still read, moved `Outdated -> DeleteOnDestroy`.
    pub unreferenced: Vec<u128>,
}

impl HefPublisher {
    /// Publishes `built` - one file merged from the files named in `inputs` - as a single new generation in which the
    /// output is `Active` and every input is `Outdated`, so new queries read the output while queries already reading
    /// the inputs can finish.
    ///
    /// The inputs must be `Active` files of `config`'s tenant whose coverage joins into one contiguous range; the
    /// output covers exactly that range and must pass the same checks as a fresh publication. The output is uploaded
    /// and checked before the generation is written, and peers are told about it once the generation wins. A lost race
    /// rebases and retries; if an input was meanwhile replaced by someone else, the publication is refused and the
    /// uploaded object is left unreferenced.
    #[expect(
        clippy::too_many_arguments,
        reason = "the compaction publish wires every interface exactly once"
    )]
    pub fn publish_compaction(
        &mut self,
        built: BuiltHef,
        config: &HefBuildConfig,
        inputs: &[u128],
        published: &mut dyn PublishedSet,
        objects: &dyn ObjectStore,
        notices: &mut dyn PeerNotices,
        clock: &dyn Clock,
    ) -> Result<Published, PublishFailure> {
        let (_, head) = published.head().map_err(PublishFailure::Publish)?;
        let coverage = merged_coverage(&head, inputs, config.tenant_id)?;
        self.verify_staged(&built, config, &coverage)?;
        upload_hef(
            objects,
            &hef_object_key(config.tenant_id, built.file_id),
            &built,
            MIN_MULTIPART_PART_BYTES,
        )?;
        let output = HefFileEntry {
            coverage,
            feature_metadata: None,
            file_seal: built.file_seal,
            file_id: built.file_id,
            file_type: FileType::HefFile,
            footer_len: Some(built.footer_len),
            optional_feature_flags: built.footer.optional_feature_flags,
            part_index: 0,
            part_state: PartState::Active,
            required_feature_flags: built.footer.required_feature_flags,
            size_bytes: built.bytes.len() as u64,
            tenant_id: config.tenant_id,
            tree_len: built.tree_len,
        };

        let mut head_id = 0;
        for _ in 0..=MAX_REBASE_ATTEMPTS {
            let (current_id, head) = published.head().map_err(PublishFailure::Publish)?;
            head_id = current_id;
            if head.files.contains(&output) {
                // This publication already won: a rival helped this attempt's generation onto the head after it saw
                // its own head advance fail.
                return Ok(Published {
                    entry: output,
                    file_bytes: built.bytes,
                    generation: head_id,
                });
            }
            let next_id = head_id + 1;
            let now = clock.now_nanos();
            // The file set changes, so any footer mirror of the previous generation no longer covers it.
            let mut next = ManifestGeneration {
                footer_mirror: None,
                generation: next_id,
                ..head
            };
            for input in inputs {
                let entry = next
                    .files
                    .iter_mut()
                    .find(|entry| entry.file_id == *input && entry.part_state == PartState::Active)
                    .ok_or(PublishFailure::Verification(
                        "a compaction input is no longer an active file",
                    ))?;
                debug_assert!(entry.part_state.can_transition_to(PartState::Outdated));
                entry.part_state = PartState::Outdated;
                next.retirements.push(Retirement {
                    file_id: *input,
                    generation: next_id,
                    since_nanos: now,
                });
            }
            next.files.push(output.clone());
            if commit(published, head_id, next)? {
                notices.hef_published(&output);
                return Ok(Published {
                    entry: output,
                    file_bytes: built.bytes,
                    generation: next_id,
                });
            }
        }
        Err(PublishFailure::Publish(PublishError::CasLost {
            current_generation: head_id,
        }))
    }
}

/// Moves replaced files toward deletion, and deletes the ones that have waited long enough, in one new generation.
///
/// `oldest_live_snapshot` is the oldest catalogue generation any running query opened its snapshot at (the head
/// generation when no query is running); the application tracks it. An `Outdated` file becomes `DeleteOnDestroy` only
/// when every live snapshot was opened at or after the generation that outdated it - so no running query selected it -
/// and it has been `Outdated` for at least the in-flight-query horizon. A `DeleteOnDestroy` file is deleted from
/// `objects` once it has waited the safety window, and its entry then leaves the catalogue. A file whose object key is
/// still named by an `Active` or `Outdated` entry is never deleted.
///
/// Objects are deleted before the catalogue forgets them, so a crash in between leaves the entries for the next sweep,
/// whose repeat delete is harmless. A retired file with no retirement record (written before records existed) gets one
/// starting now. Returns what changed; a sweep with nothing due publishes no generation.
pub fn sweep_retired_files(
    published: &mut dyn PublishedSet,
    objects: &dyn ObjectStore,
    policy: &SweepPolicy,
    oldest_live_snapshot: u64,
    clock: &dyn Clock,
) -> Result<SweepReport, PublishFailure> {
    let safety_window = policy.safety_window_nanos.max(policy.in_flight_query_horizon_nanos);
    let mut head_id = 0;
    for _ in 0..=MAX_REBASE_ATTEMPTS {
        let (current_id, head) = published.head().map_err(PublishFailure::Publish)?;
        head_id = current_id;
        let next_id = head_id + 1;
        let now = clock.now_nanos();
        let mut next = ManifestGeneration {
            footer_mirror: None,
            generation: next_id,
            ..head
        };
        let mut report = SweepReport::default();
        let mut started_clocks = Vec::new();
        for entry in &mut next.files {
            if !matches!(entry.part_state, PartState::Outdated | PartState::DeleteOnDestroy) {
                continue;
            }
            let Some(retirement) = next
                .retirements
                .iter_mut()
                .find(|retirement| retirement.file_id == entry.file_id)
            else {
                started_clocks.push(Retirement {
                    file_id: entry.file_id,
                    generation: next_id,
                    since_nanos: now,
                });
                continue;
            };
            let waited = u64::try_from(now.saturating_sub(retirement.since_nanos)).unwrap_or(0);
            match entry.part_state {
                PartState::Outdated
                    if retirement.generation <= oldest_live_snapshot
                        && waited >= policy.in_flight_query_horizon_nanos =>
                {
                    debug_assert!(entry.part_state.can_transition_to(PartState::DeleteOnDestroy));
                    entry.part_state = PartState::DeleteOnDestroy;
                    *retirement = Retirement {
                        file_id: entry.file_id,
                        generation: next_id,
                        since_nanos: now,
                    };
                    report.unreferenced.push(entry.file_id);
                }
                PartState::DeleteOnDestroy if waited >= safety_window => report.deleted.push(entry.file_id),
                _ => {}
            }
        }
        if started_clocks.is_empty() && report.unreferenced.is_empty() && report.deleted.is_empty() {
            return Ok(report);
        }
        next.retirements.extend(started_clocks);

        let still_readable: HashSet<u128> = next
            .files
            .iter()
            .filter(|entry| matches!(entry.part_state, PartState::Active | PartState::Outdated))
            .map(|entry| entry.file_id)
            .collect();
        report.deleted.retain(|file_id| !still_readable.contains(file_id));
        for entry in next
            .files
            .iter()
            .filter(|entry| report.deleted.contains(&entry.file_id))
        {
            debug_assert!(entry.part_state.can_transition_to(PartState::Deleted));
            objects
                .delete(&hef_object_key(entry.tenant_id, entry.file_id))
                .map_err(PublishFailure::Storage)?;
        }
        next.files.retain(|entry| !report.deleted.contains(&entry.file_id));
        next.retirements
            .retain(|retirement| !report.deleted.contains(&retirement.file_id));

        if commit(published, head_id, next)? {
            report.generation = Some(next_id);
            return Ok(report);
        }
    }
    Err(PublishFailure::Publish(PublishError::CasLost {
        current_generation: head_id,
    }))
}

/// The one contiguous range the `inputs` cover together: each must be an `Active` file of `tenant_id`, and sorted by
/// sequence each must start right after the previous one ends.
fn merged_coverage(
    head: &ManifestGeneration,
    inputs: &[u128],
    tenant_id: TenantId,
) -> Result<SequenceRange, PublishFailure> {
    let mut ranges = inputs
        .iter()
        .map(|input| {
            head.files
                .iter()
                .find(|entry| {
                    entry.file_id == *input && entry.tenant_id == tenant_id && entry.part_state == PartState::Active
                })
                .map(|entry| entry.coverage)
                .ok_or(PublishFailure::Verification(
                    "a compaction input is not an active file of this tenant",
                ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    ranges.sort();
    let (first, rest) = ranges
        .split_first()
        .ok_or(PublishFailure::Verification("a compaction needs at least one input"))?;
    let mut merged = *first;
    for range in rest {
        if range.epoch != merged.epoch || merged.last_sequence.checked_add(1) != Some(range.first_sequence) {
            return Err(PublishFailure::Verification(
                "compaction inputs do not cover one contiguous range",
            ));
        }
        merged.last_sequence = range.last_sequence;
    }
    Ok(merged)
}

/// Writes `next` and moves the head to it from `head_id`. `Ok(true)` when this call published it; `Ok(false)` when the
/// head moved first and the caller must rebase. A generation id someone else already wrote - a crashed publisher's
/// stalled generation - is helped onto the head first, so it can never wedge this caller.
fn commit(published: &mut dyn PublishedSet, head_id: u64, next: ManifestGeneration) -> Result<bool, PublishFailure> {
    let next_id = next.generation;
    match published.put_generation(next) {
        Ok(()) => {}
        Err(PublishError::GenerationExists) => {
            return match published.advance_head(head_id, next_id) {
                Ok(()) | Err(PublishError::CasLost { .. }) => Ok(false),
                Err(error) => Err(PublishFailure::Publish(error)),
            };
        }
        Err(error) => return Err(PublishFailure::Publish(error)),
    }
    match published.advance_head(head_id, next_id) {
        Ok(()) => Ok(true),
        Err(PublishError::CasLost { .. }) => Ok(false),
        Err(error) => Err(PublishFailure::Publish(error)),
    }
}

#[cfg(test)]
#[path = "test/retire.rs"]
mod tests;
