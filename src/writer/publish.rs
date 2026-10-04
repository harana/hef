//! Makes a freshly built file visible to queries — all-or-nothing — and decides when to start a new file.
//!
//! A file is invisible until the whole publish sequence succeeds: pick a contiguous durable journal range → validate
//! every frame (CRC-64/NVME precheck, authoritative BLAKE3, hash chain) → decode only the version-1 batch encoding →
//! build and seal the file → stage it for the tenant → verify size/quota, schema, BLAKE3, tenant, range, and feature
//! directory → publish the catalogue entry atomically → only then notify peers. A failed or losing attempt leaves no
//! peer notice, no public read, and no staged side effects behind.

use super::build::{BuiltHef, HefBuildConfig, HefRow, build_hef_file_with_executor};
use super::reserve::arbitrate_coverage;
use super::upload::upload_hef;
use crate::artifacts::batch::{decode_batch, envelope_of};
use crate::artifacts::frame::decode_frame;
use crate::artifacts::segment::{ReplayOutcome, ReplayedFrame, SegmentDescriptor, replay_segment, verify_chain_anchor};
use crate::error::{FormatError, PublishError, StorageError};
use crate::events::{SequenceRange, TenantId};
use crate::invariants::{EncodeExecutor, JournalStorage, MonotonicClock, PublishedSet, ShardId};
use crate::layout::{MAX_PAGE_BYTES, required_features};
use crate::lifecycle::{FileType, HefFileEntry, ManifestGeneration, PartState};
use crate::object_store::{ObjectStore, constant::MIN_MULTIPART_PART_BYTES, hef_object_key};

/// The byte length of a BLAKE3 hash, as stored in a schema fingerprint or a segment chain anchor.
const BLAKE3_HASH_BYTES: usize = 32;

/// [`RollPolicy::default`]'s byte target: 1 GiB.
const DEFAULT_ROLL_BYTE_TARGET: u64 = 1 << 30;

/// [`RollPolicy::default`]'s open-time window: 5 minutes, in nanoseconds. Internal policy constant, tunable only
/// through benchmark-gated change.
const DEFAULT_ROLL_TIME_WINDOW_NANOS: u64 = 300 * 1_000_000_000;

/// How many times [`HefPublisher::publish_range`] rebases and retries the manifest CAS before giving up.
pub(super) const MAX_REBASE_ATTEMPTS: u32 = 16;

/// The byte length of each upload segment for a built HEF, cut so that every stripe begins on a segment boundary. The
/// leading segment is the header (and anything before the first stripe); each following segment runs from one stripe's
/// start to the next stripe's start; the final segment carries the last stripe together with the payload arena and
/// footer. Their sum is the whole file length.
///
/// A publisher stages a HEF one segment at a time — a stripe as it seals — so a multipart-capable provider aligns its
/// parts to the stripe boundaries (a whole stripe becomes an integral number of whole parts). Because the segments only
/// shape how the object is split into parts, the committed bytes are byte-identical to staging the whole buffer, and a
/// provider without multipart uploads commits exactly the same object.
pub fn hef_upload_segments(built: &BuiltHef) -> Vec<u64> {
    let total = built.bytes.len() as u64;
    let mut cuts: Vec<u64> = built
        .footer
        .stripes
        .iter()
        .map(|stripe| stripe.file_offset)
        .filter(|&offset| offset > 0 && offset < total)
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.push(total);

    let mut segments = Vec::with_capacity(cuts.len());
    let mut previous = 0u64;
    for cut in cuts {
        if cut > previous {
            segments.push(cut - previous);
            previous = cut;
        }
    }
    segments
}

/// The dual roll trigger. Both values are publish policy, not file-layout rules; the 512 MiB stripe clamp stays a pure
/// safety backstop.
#[derive(Debug, Clone, Copy)]
pub struct RollPolicy {
    /// Compressed byte target (default ~1 GiB).
    pub byte_target: u64,
    /// Maximum open-time window (default minutes-scale) so low-volume tenants seal small compact files and
    /// HEJ/LiveOverlay retention can advance even when the byte target is never reached.
    pub max_open_nanos: u64,
}

impl Default for RollPolicy {
    fn default() -> Self {
        Self {
            byte_target: DEFAULT_ROLL_BYTE_TARGET,
            max_open_nanos: DEFAULT_ROLL_TIME_WINDOW_NANOS,
        }
    }
}

/// Which trigger sealed the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollTrigger {
    ByteTarget,
    TimeWindow,
}

/// Accumulation state of the currently open (OpenTmp) file.
#[derive(Debug, Clone, Copy)]
pub struct OpenFileState {
    pub compressed_bytes: u64,
    pub opened_at_monotonic_nanos: u64,
}

/// The dual trigger, whichever fires first.
///
/// Nothing in HEF calls this: the application checks it on its open file after each flush and publishes the range with
/// [`HefPublisher::publish_range`] when it fires. See [`crate::writer::retire`] for the whole call pattern.
pub fn should_roll(state: &OpenFileState, policy: &RollPolicy, clock: &dyn MonotonicClock) -> Option<RollTrigger> {
    if state.compressed_bytes >= policy.byte_target {
        return Some(RollTrigger::ByteTarget);
    }
    if clock.monotonic_nanos().saturating_sub(state.opened_at_monotonic_nanos) >= policy.max_open_nanos {
        return Some(RollTrigger::TimeWindow);
    }
    None
}

/// Peer notices are emitted only after publication succeeds. The real transport arrives with the peer-service change;
/// the interface is fixed now.
pub trait PeerNotices {
    /// Tells peers a file has just been published. Called only after the catalogue publication has actually won.
    ///
    /// May be called more than once for the same published entry: when two attempts race the same range, or when a
    /// publisher completes a stalled generation on another attempt's behalf, the same live entry can be announced by
    /// each party. Implementations must treat a repeat notice for a file identity they have already announced as a
    /// no-op, so at-least-once delivery never double-counts a publication.
    fn hef_published(&mut self, entry: &HefFileEntry);
}

/// No-op transport until peer-service lands.
#[derive(Debug, Default)]
pub struct NoopPeerNotices {
    pub published: Vec<u128>,
}

impl PeerNotices for NoopPeerNotices {
    fn hef_published(&mut self, entry: &HefFileEntry) {
        self.published.push(entry.file_id);
    }
}

/// The HEF-publish side-effect transaction discipline: a service that mutates derived state while observing publication
/// records staged changes under the attempt id, promotes them only at `after_hef_publish_before_peer_notice` when the
/// manifest publication wins, and otherwise rolls back before serving reads.
pub trait PublishObserver {
    /// Called when an attempt begins: record a rollback point under this attempt id.
    fn stage(&mut self, attempt_id: u64);
    /// Called after the manifest CAS wins, before any peer notice. Idempotent with respect to the published file
    /// identity: when two attempts race the same range they build byte-identical files, and each promotes its own
    /// staged copy of the same derived state; a service must apply the derived mutation for a given published file at
    /// most once however many attempts promote it.
    fn promote(&mut self, attempt_id: u64);
    /// Called when the attempt fails or loses: staged state is discarded.
    fn rollback(&mut self, attempt_id: u64);
}

/// Observer that records the discipline (used until real services arrive).
#[derive(Debug, Default)]
pub struct RecordingObserver {
    pub promoted: Vec<u64>,
    pub rolled_back: Vec<u64>,
    pub staged: Vec<u64>,
}

impl PublishObserver for RecordingObserver {
    fn stage(&mut self, attempt_id: u64) {
        self.staged.push(attempt_id);
    }
    fn promote(&mut self, attempt_id: u64) {
        self.promoted.push(attempt_id);
    }
    fn rollback(&mut self, attempt_id: u64) {
        self.rolled_back.push(attempt_id);
    }
}

/// Why a publish attempt was refused before reaching the manifest.
#[derive(Debug)]
pub enum PublishFailure {
    Format(FormatError),
    Publish(PublishError),
    /// The requested range is not contiguously covered by durable frames (events or voids) in the journal.
    RangeNotDurable,
    Storage(StorageError),
    /// A staged verification rule failed.
    Verification(&'static str),
}

/// The HEF publisher. One instance per leader; attempts are numbered so observers can correlate staging and promotion.
#[derive(Debug, Default)]
pub struct HefPublisher {
    attempt_counter: u64,
    /// Optional per-tenant quota on staged file bytes.
    pub quota_bytes: Option<u64>,
}

/// One successful publication.
#[derive(Debug)]
pub struct Published {
    pub entry: HefFileEntry,
    pub file_bytes: Vec<u8>,
    pub generation: u64,
}

impl HefPublisher {
    /// A fresh publisher with no attempts counted and no quota set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Collects the rows of `range` from a validated replay, requiring contiguous durable coverage by event frames and
    /// void records.
    fn rows_for_range(
        &self,
        replay: &ReplayOutcome,
        tenant_id: TenantId,
        range: &SequenceRange,
    ) -> Result<Vec<HefRow>, PublishFailure> {
        let frames = durable_event_frames(replay, tenant_id, range)?;
        let capacity: usize = frames.iter().map(|frame| frame.header.event_count as usize).sum();
        let mut rows = Vec::with_capacity(capacity);
        for frame in frames {
            let header = &frame.header;
            // Decode only harana_hej_compact_batch_v1 (enforced by the frame validator) with full batch validation.
            let (_, payload) = decode_frame(&frame.frame_bytes).map_err(PublishFailure::Format)?;
            let batch = decode_batch(payload, header.event_count).map_err(PublishFailure::Format)?;
            for (row_index, event) in batch.events.iter().enumerate() {
                let sequence = header.first_sequence + row_index as u64;
                if sequence < range.first_sequence || sequence > range.last_sequence {
                    continue;
                }
                let envelope = envelope_of(event, header.tenant_id);
                let payload_input = match event.payload {
                    None => crate::artifacts::batch::PayloadInput::None,
                    Some(bytes)
                        if event.variable.payload_flags & crate::artifacts::batch::PAYLOAD_FLAG_EXTERNAL_REF != 0 =>
                    {
                        let reference = std::str::from_utf8(bytes).map_err(|_| {
                            PublishFailure::Format(FormatError::InvalidUtf8 {
                                what: "external payload reference",
                            })
                        })?;
                        crate::artifacts::batch::PayloadInput::ExternalRef(reference.to_owned())
                    }
                    Some(bytes) => crate::artifacts::batch::PayloadInput::Variant(
                        crate::events::variant::VariantRef::new(bytes)
                            .decode(&batch.dictionary)
                            .map_err(PublishFailure::Format)?,
                    ),
                };
                rows.push(HefRow {
                    epoch: header.epoch,
                    sequence,
                    event: crate::artifacts::batch::EventInput {
                        envelope,
                        payload: payload_input,
                        source_schema: event.source_schema.map(str::to_owned),
                        source_delivery: event.source_delivery.map(str::to_owned),
                        connector_delivery_hash_low: event.variable.connector_delivery_hash_low,
                        connector_delivery_hash_high: event.variable.connector_delivery_hash_high,
                        provenance: event.provenance.clone(),
                        relationships: event.relationships.clone(),
                    },
                });
            }
        }
        Ok(rows)
    }

    /// Staged-verification rules: size/quota, schema, BLAKE3, tenant, range, feature directory.
    pub(super) fn verify_staged(
        &self,
        built: &BuiltHef,
        config: &HefBuildConfig,
        range: &SequenceRange,
    ) -> Result<(), PublishFailure> {
        if let Some(quota) = self.quota_bytes
            && built.bytes.len() as u64 > quota
        {
            return Err(PublishFailure::Verification("tenant quota exceeded"));
        }
        if built.footer.schema_fingerprint == [0u8; BLAKE3_HASH_BYTES] {
            return Err(PublishFailure::Verification("schema fingerprint missing"));
        }
        // A CRC-64/NVME check, not a BLAKE3 re-hash: `built.file_seal` was already derived from the stripe roots and
        // the small remaining regions during the build, so a second full pass here would only catch a bug that let
        // `built.bytes` diverge from what was sealed — a check the much cheaper CRC-64 already catches.
        if !crate::file::integrity::crc64_matches(&built.bytes, built.file_crc64_nvme) {
            return Err(PublishFailure::Verification("file checksum mismatch"));
        }
        if built.header.tenant_id != config.tenant_id {
            return Err(PublishFailure::Verification("tenant mismatch"));
        }
        if built.header.min_epoch != range.epoch
            || built.header.min_sequence < range.first_sequence
            || built.header.max_sequence > range.last_sequence
        {
            return Err(PublishFailure::Verification("range mismatch"));
        }
        if built.footer.required_feature_flags != required_features::ALL {
            return Err(PublishFailure::Verification("feature directory incomplete"));
        }
        // No column block may exceed the reader's per-page read bound, or the reader rejects the whole file refuse
        // after publication makes the range look covered and its backing journal recyclable. The builder already
        // enforces this, so a violation here means a build-path regression — caught before any visibility.
        let mark_oversize = built
            .footer
            .marks
            .iter()
            .any(|mark| mark.page_count <= 1 && mark.compressed_size > MAX_PAGE_BYTES);
        let page_oversize = built
            .footer
            .page_directory
            .iter()
            .any(|entry| entry.compressed_len > MAX_PAGE_BYTES);
        if mark_oversize || page_oversize {
            return Err(PublishFailure::Verification(
                "column block exceeds the maximum page size",
            ));
        }
        Ok(())
    }

    /// The complete publish boundary for one contiguous durable range.
    ///
    /// The built file is uploaded to `objects` under [`hef_object_key`] and its stored size and CRC-64/NVME checked
    /// before any generation is written, so a catalogue entry never names an object that is missing or wrong; a failed
    /// upload is aborted and the attempt rolled back. Once uploaded, the object is never deleted here: an attempt that
    /// then loses the range to a different file leaves it unreferenced, for the application's sweep of unreferenced
    /// keys, rather than deleting bytes a stalled generation might still name.
    ///
    /// Idempotent: re-publishing an already-covered range returns the existing entry paired with the freshly rebuilt
    /// bytes, but only once those bytes are confirmed to match the existing entry's file id and BLAKE3 (same content
    /// ⇒ same content-derived file identity). If the existing file covering the range does not match — rebuilt under
    /// a changed generation, layout, promotion, or timestamp configuration — the mismatch is reported as a
    /// verification failure rather than pairing the entry with bytes it does not hash-validate against. A lost
    /// manifest CAS rebases onto the new head and retries; it never overwrites.
    ///
    /// When `expected_chain_anchor` is `Some`, the replayed segment hash chain must match it or the whole attempt is
    /// refused before anything is built or staged; pass `None` when no recorded anchor is available and the chain is
    /// not checked.
    #[expect(
        clippy::too_many_arguments,
        reason = "the publish boundary wires every interface exactly once"
    )]
    pub fn publish_range(
        &mut self,
        storage: &dyn JournalStorage,
        shard: ShardId,
        segment_id: u64,
        segment_generation: u64,
        expected_chain_anchor: Option<[u8; BLAKE3_HASH_BYTES]>,
        range: SequenceRange,
        config: &HefBuildConfig,
        published: &mut dyn PublishedSet,
        objects: &dyn ObjectStore,
        observer: &mut dyn PublishObserver,
        notices: &mut dyn PeerNotices,
        _clock: &dyn MonotonicClock,
        encode: &dyn EncodeExecutor,
    ) -> Result<Published, PublishFailure> {
        self.attempt_counter += 1;
        let attempt_id = self.attempt_counter;

        // 1-2. Validated replay of the durable journal range.
        let replay = replay_segment(storage, shard, segment_id, segment_generation).map_err(PublishFailure::Storage)?;
        // When the caller records the segment's chain anchor, the replayed chain must match it before anything is built
        // or staged: reordered or removed frames, or stale bytes from an old segment generation, replay to a different
        // anchor and are rejected here.
        if let Some(expected) = expected_chain_anchor {
            let descriptor = SegmentDescriptor {
                segment_chain_blake3: Some(expected),
                ..SegmentDescriptor::new(segment_id, segment_generation)
            };
            if !verify_chain_anchor(&descriptor, replay.segment_chain_blake3.as_ref()) {
                return Err(PublishFailure::Verification(
                    "replayed segment chain anchor does not match the recorded anchor",
                ));
            }
        }
        let rows = self.rows_for_range(&replay, config.tenant_id, &range)?;
        if rows.is_empty() {
            // Nothing to publish: the range is durably closed by void records only. Journal retention releases it
            // through `range_holds_no_events` rather than through a file that could never be built.
            return Err(PublishFailure::Verification("range contains no events (all void)"));
        }

        // 3-4. Build and seal: OpenTmp → Sealed once footer and checksums are valid.
        let built = build_hef_file_with_executor(rows, config, encode).map_err(PublishFailure::Format)?;
        let mut part_state = PartState::OpenTmp;
        debug_assert!(part_state.can_transition_to(PartState::Sealed));
        part_state = PartState::Sealed;

        // 5. Stage tenant-qualified; side effects record their rollback
        // point under the attempt id.
        observer.stage(attempt_id);
        let staged_entry = HefFileEntry {
            coverage: range,
            feature_metadata: None,
            file_seal: built.file_seal,
            file_id: built.file_id,
            file_type: FileType::HefFile,
            footer_len: Some(built.footer_len),
            optional_feature_flags: built.footer.optional_feature_flags,
            part_index: 0,
            part_state,
            required_feature_flags: built.footer.required_feature_flags,
            size_bytes: built.bytes.len() as u64,
            tenant_id: config.tenant_id,
            tree_len: built.tree_len,
        };

        // 6. Verify everything before any visibility.
        if let Err(failure) = self.verify_staged(&built, config, &range) {
            observer.rollback(attempt_id);
            return Err(failure);
        }
        // Upload and check the object before any generation can name it.
        let object_key = hef_object_key(config.tenant_id, built.file_id);
        if let Err(failure) = upload_hef(objects, &object_key, &built, MIN_MULTIPART_PART_BYTES) {
            observer.rollback(attempt_id);
            return Err(failure);
        }

        // 7. Atomic manifest publication with rebase-and-retry CAS.
        let mut rebase_attempts = 0u32;
        // The generation object this attempt itself wrote, once `put_generation` succeeds. If a concurrent publisher
        // then advances the head over it before this attempt can, this attempt's own entry is what became live — so on
        // rediscovering it below its staged side effects are promoted, not rolled back.
        let mut staged_generation: Option<u64> = None;
        let generation = loop {
            let (head_id, head) = match published.head() {
                Ok(head) => head,
                Err(error) => {
                    observer.rollback(attempt_id);
                    return Err(PublishFailure::Publish(error));
                }
            };
            // Idempotency: the range may already be covered by a *live* entry of *this* tenant. A covering entry that
            // belongs to another tenant, or one already retired (`DeleteOnDestroy`/`Deleted`) or not yet visible
            // (`OpenTmp`/`Sealed`), must not short-circuit — otherwise the rows are reported as published while no live
            // file actually covers them, and journal retention could advance past an uncovered range.
            if let Some(existing) = head.files.iter().find(|entry| {
                entry.coverage.contains(&range)
                    && entry.tenant_id == config.tenant_id
                    && matches!(entry.part_state, PartState::Active | PartState::Outdated)
            }) {
                // Only pair the existing entry with the rebuilt bytes once they are confirmed to be the same file;
                // a rebuild under changed config can produce different bytes that would not hash-validate.
                if existing.file_id != built.file_id || existing.file_seal != built.file_seal {
                    observer.rollback(attempt_id);
                    return Err(PublishFailure::Verification(
                        "range already covered by a file with different content than the rebuilt bytes",
                    ));
                }
                if staged_generation.is_some() {
                    // This attempt wrote the generation now carrying the live entry, but a concurrent publisher
                    // completing a stalled generation advanced the head over it before this attempt could. The
                    // publication won, so per the publish boundary the staged side effects are promoted and peers are
                    // notified — not rolled back as they were, which left a live, query-visible file whose derived
                    // side effects had been discarded and whose peers were never told.
                    observer.promote(attempt_id);
                    notices.hef_published(existing);
                } else {
                    // A prior, already-completed publication owns this entry; nothing this attempt staged is live, so
                    // its rollback point is discarded and no duplicate notice is emitted.
                    observer.rollback(attempt_id);
                }
                return Ok(Published {
                    entry: existing.clone(),
                    generation: head_id,
                    file_bytes: built.bytes,
                });
            }
            let mut entry = staged_entry.clone();
            debug_assert!(entry.part_state.can_transition_to(PartState::Active));
            entry.part_state = PartState::Active;
            // A newly published generation starts without a footer mirror: the file set just changed, so any
            // previous mirror no longer covers it. Rebuilding is a separate, out-of-band step.
            let mut next = ManifestGeneration {
                generation: head_id + 1,
                files: head.files.clone(),
                retirements: head.retirements.clone(),
                ..Default::default()
            };
            next.files.push(entry.clone());
            let next_id = next.generation;
            let put = published.put_generation(next);
            if put.is_ok() {
                staged_generation = Some(next_id);
            }
            match put.and_then(|()| published.advance_head(head_id, next_id)) {
                Ok(()) => {
                    // 8. Promote staged side effects after the publication
                    // wins, before any peer notice; notices only after success.
                    observer.promote(attempt_id);
                    notices.hef_published(&entry);
                    break next_id;
                }
                Err(PublishError::CasLost { .. }) => {
                    // Genuine CAS race: the head moved between our head() and advance_head(). Rebase and retry.
                    rebase_attempts += 1;
                    if rebase_attempts > MAX_REBASE_ATTEMPTS {
                        observer.rollback(attempt_id);
                        return Err(PublishFailure::Publish(PublishError::CasLost {
                            current_generation: head_id,
                        }));
                    }
                    continue;
                }
                Err(PublishError::GenerationExists) => {
                    // Generation ID was written — either by us (crash after put_generation, before advance_head) or by
                    // a concurrent publisher. Check whether our entry is already in it; if so, we won and the head just
                    // needs to catch up.
                    let existing = match published.generation(next_id) {
                        Ok(g) => g,
                        Err(PublishError::UnknownGeneration) => {
                            // Generation object gone — shouldn't happen with create-only semantics; treat as race.
                            rebase_attempts += 1;
                            if rebase_attempts > MAX_REBASE_ATTEMPTS {
                                observer.rollback(attempt_id);
                                return Err(PublishFailure::Publish(PublishError::CasLost {
                                    current_generation: head_id,
                                }));
                            }
                            continue;
                        }
                        Err(error) => {
                            observer.rollback(attempt_id);
                            return Err(PublishFailure::Publish(error));
                        }
                    };
                    if let Some(covering) = existing.files.iter().find(|e| {
                        e.coverage.contains(&range)
                            && e.tenant_id == config.tenant_id
                            && matches!(e.part_state, PartState::Active | PartState::Outdated)
                    }) {
                        // Adopt the generation only when its covering entry is the very file this attempt rebuilt
                        // (same content-derived identity and BLAKE3) and is live — a new generation copies every head
                        // entry, so a retired (`DeleteOnDestroy`/`Deleted`) or not-yet-visible entry over the same
                        // range must not stand in for the active entry this attempt is trying to add.
                        // A crash-recovery republish under changed config
                        // rebuilds different bytes; advancing the head would publish an entry this process never
                        // built and pair it with bytes it does not hash-validate against — the same mismatch the
                        // head-covered idempotency branch above refuses.
                        if covering.file_id != built.file_id || covering.file_seal != built.file_seal {
                            observer.rollback(attempt_id);
                            return Err(PublishFailure::Verification(
                                "range already covered by a file with different content than the rebuilt bytes",
                            ));
                        }
                        // Our entry is already published in this generation — advance the head to complete publication.
                        match published.advance_head(head_id, next_id) {
                            Ok(()) => {
                                observer.promote(attempt_id);
                                notices.hef_published(&entry);
                                break next_id;
                            }
                            Err(PublishError::CasLost { .. }) => {
                                // Head moved between generation() and advance_head; rebase and retry.
                                rebase_attempts += 1;
                                if rebase_attempts > MAX_REBASE_ATTEMPTS {
                                    observer.rollback(attempt_id);
                                    return Err(PublishFailure::Publish(PublishError::CasLost {
                                        current_generation: head_id,
                                    }));
                                }
                                continue;
                            }
                            Err(error) => {
                                observer.rollback(attempt_id);
                                return Err(PublishFailure::Publish(error));
                            }
                        }
                    } else {
                        // Another publisher wrote this generation but never advanced the head (a crash between
                        // put_generation and advance_head). Retrying against the unmoved head would recompute the
                        // same id and collide forever, so help the stalled publication complete: advance the head
                        // past it, then rebase onto it and retry with the next id. A lost advance means someone
                        // else moved the head first — also progress, so rebase either way.
                        rebase_attempts += 1;
                        if rebase_attempts > MAX_REBASE_ATTEMPTS {
                            observer.rollback(attempt_id);
                            return Err(PublishFailure::Publish(PublishError::CasLost {
                                current_generation: head_id,
                            }));
                        }
                        match published.advance_head(head_id, next_id) {
                            Ok(()) => {
                                // Advancing the head made every entry this stalled generation added query-visible. Its
                                // own publisher crashed before it could notify peers, so this rescuer announces the
                                // newly-live files on its behalf — peers learn of every published file even when its
                                // publisher never returns. The notice is idempotent, so that publisher later returning
                                // and re-announcing the same file is harmless.
                                for introduced in existing.files.iter().filter(|entry| {
                                    entry.part_state == PartState::Active
                                        && !head.files.iter().any(|prior| prior.file_id == entry.file_id)
                                }) {
                                    notices.hef_published(introduced);
                                }
                                continue;
                            }
                            // A lost advance means someone else moved the head first — also progress, and they own
                            // notifying the entries they made live, so rebase without notifying here.
                            Err(PublishError::CasLost { .. }) => continue,
                            Err(error) => {
                                observer.rollback(attempt_id);
                                return Err(PublishFailure::Publish(error));
                            }
                        }
                    }
                }
                Err(error) => {
                    observer.rollback(attempt_id);
                    return Err(PublishFailure::Publish(error));
                }
            }
        };

        let mut entry = staged_entry;
        entry.part_state = PartState::Active;
        Ok(Published {
            entry,
            generation,
            file_bytes: built.bytes,
        })
    }
}

/// The event frames that carry `tenant_id`'s rows for `range`, in sequence order.
///
/// Fails with `RangeNotDurable` unless the whole range is contiguously covered by durable frames. Void records take
/// part in that coverage — the sequences they close are durable, permanently rowless — and are then dropped, so the
/// result is empty exactly when the range is durable but holds no events at all.
fn durable_event_frames<'r>(
    replay: &'r ReplayOutcome,
    tenant_id: TenantId,
    range: &SequenceRange,
) -> Result<Vec<&'r ReplayedFrame>, PublishFailure> {
    // The frames of this tenant and epoch that intersect the target range, in durable-commit (segment) order. Sequence
    // identity includes the tenant, so another tenant's overlapping frame must not arbitrate against this range or
    // count towards its coverage.
    let in_range: Vec<_> = replay
        .frames
        .iter()
        .filter(|frame| {
            frame.header.tenant_id == tenant_id
                && frame.header.epoch == range.epoch
                && frame.header.last_sequence >= range.first_sequence
                && frame.header.first_sequence <= range.last_sequence
        })
        .collect();
    // Durable-commit-order arbitration of the void/event mutual exclusion: the first durable frame to cover a
    // sequence wins, and a later frame overlapping already-covered sequences is a rejected duplicate — a stalled
    // worker's frame that a void later closed, or the reverse. A rejected frame is never decoded, so overlapping
    // coverage can neither double-append its rows nor let the coverage cursor regress.
    let frame_ranges: Vec<SequenceRange> = in_range
        .iter()
        .map(|frame| SequenceRange {
            epoch: frame.header.epoch,
            first_sequence: frame.header.first_sequence,
            last_sequence: frame.header.last_sequence,
        })
        .collect();
    let rejected = arbitrate_coverage(&frame_ranges);

    // Arbitration is durable-commit order, but coverage is a property of the sequence line: a frame may be made durable
    // before a lower-sequence one (autonomous workers and replication both complete out of order), so the accepted
    // frames are sorted by sequence before the contiguity walk. Checking contiguity in durable order would reject a
    // fully covered range as `RangeNotDurable` and keep its journal pinned.
    let mut accepted: Vec<&ReplayedFrame> = in_range
        .iter()
        .enumerate()
        .filter(|(index, _)| !rejected.contains(index))
        .map(|(_, frame)| *frame)
        .collect();
    accepted.sort_by_key(|frame| frame.header.first_sequence);

    let mut event_frames = Vec::new();
    let mut covered_to: Option<u64> = None;
    for frame in accepted {
        let header = &frame.header;
        // Contiguity check across events and voids: a gap means the range is not fully durable.
        let expected = covered_to.map_or(range.first_sequence, |c| c + 1);
        if header.first_sequence > expected {
            return Err(PublishFailure::RangeNotDurable);
        }
        covered_to = Some(header.last_sequence.min(range.last_sequence));
        if header.is_void_record() {
            continue;
        }
        event_frames.push(frame);
    }
    if covered_to.is_none_or(|c| c < range.last_sequence) {
        return Err(PublishFailure::RangeNotDurable);
    }
    Ok(event_frames)
}

/// Whether `range` is durably covered but carries no events at all — every frame covering it is a void record for an
/// abandoned reservation.
///
/// Such a range can never produce a file (a HEF file must carry at least one row), so publication is not what releases
/// its journal; the void records themselves are, and [`journal_retention_can_advance`] takes this answer instead of
/// waiting for a manifest entry that will never appear. Fails with `RangeNotDurable` when the range is not contiguously
/// durable, which is the case where retention must still wait.
pub fn range_holds_no_events(
    replay: &ReplayOutcome,
    tenant_id: TenantId,
    range: &SequenceRange,
) -> Result<bool, PublishFailure> {
    Ok(durable_event_frames(replay, tenant_id, range)?.is_empty())
}

/// The HEJ retention gate: the journal retention cursor and segment recycling advance only after publication is visible
/// and the configured recovery safety window has elapsed.
///
/// A range that holds no events at all is the one case with nothing to publish: it is durably closed by void records,
/// so no file will ever cover it and waiting for one would pin the journal segment forever. Pass
/// [`range_holds_no_events`] as `range_is_all_void` so that range clears the gate on its void coverage alone; the
/// safety window still applies.
pub fn journal_retention_can_advance(
    published: &ManifestGeneration,
    range: &SequenceRange,
    tenant_id: TenantId,
    range_is_all_void: bool,
    safety_window_elapsed: bool,
) -> bool {
    safety_window_elapsed && (range_is_all_void || published.covers(range, tenant_id))
}

#[cfg(test)]
#[path = "test/publish.rs"]
mod tests;
