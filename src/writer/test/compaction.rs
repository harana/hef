use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotedColumn, PromotionPlan, column_ids};
use crate::encoding::{ColumnData, StringColumn};
use crate::events::provenance::{SignatureScheme, SignedEventProvenance, hex_bytes};
use crate::events::sim::{SimulatedEventAuthor, signed_event_payload};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId};
use crate::layout::LayoutTargets;
use crate::layout::footer::ColumnKind;
use crate::layout::reader::{HefFile, PayloadRead};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};
use crate::writer::rewrite::{RewriteSegment, rewrite_segments};

fn column(name: &str, settle_horizon_nanos: i64) -> DerivedColumnEntry {
    DerivedColumnEntry {
        column_name: name.to_owned(),
        settle_horizon: TimestampValue::from_physical_nanos(settle_horizon_nanos),
    }
}

#[test]
fn settled_column_embedded_and_unsettled_tail_reemitted() {
    let sidecar = DerivedColumnsSidecar {
        columns: vec![column("cluster_id", 1_000), column("revenue_anomaly_score", 5_000)],
        row_count: 32,
    };

    let folded = fold_settled_derived_columns(&sidecar, TimestampValue::from_physical_nanos(2_000));

    assert_eq!(folded.embedded_columns, vec![column("cluster_id", 1_000)]);
    let tail = folded.unsettled_sidecar.unwrap();
    assert_eq!(tail.columns, vec![column("revenue_anomaly_score", 5_000)]);
    assert_eq!(
        tail.row_count, sidecar.row_count,
        "row alignment carries over to the tail"
    );
}

#[test]
fn every_column_settled_drops_the_sidecar() {
    let sidecar = DerivedColumnsSidecar {
        columns: vec![column("cluster_id", 1_000)],
        row_count: 8,
    };

    let folded = fold_settled_derived_columns(&sidecar, TimestampValue::from_physical_nanos(1_000));

    assert_eq!(folded.embedded_columns, vec![column("cluster_id", 1_000)]);
    assert!(folded.unsettled_sidecar.is_none(), "no columns remain unsettled");
}

#[test]
fn no_column_settled_leaves_the_sidecar_unchanged() {
    let sidecar = DerivedColumnsSidecar {
        columns: vec![column("cluster_id", 1_000)],
        row_count: 8,
    };

    let folded = fold_settled_derived_columns(&sidecar, TimestampValue::from_physical_nanos(0));

    assert!(folded.embedded_columns.is_empty());
    assert_eq!(folded.unsettled_sidecar.unwrap(), sidecar);
}

/// A contiguous stripe starting where the previous one ended, so a run of them forms an unbroken prefix.
fn stripe(stripe_id: u32, file_offset: u64, byte_len: u64) -> StripeEntry {
    StripeEntry {
        byte_len,
        file_offset,
        first_row_ordinal: u64::from(stripe_id) * 100,
        row_count: 100,
        stripe_id,
    }
}

fn unchanged(stripe: StripeEntry) -> StripeChange {
    StripeChange {
        changed: false,
        stripe,
        uncertain: false,
    }
}

#[test]
fn unchanged_stripes_are_reused() {
    let base = 4096;
    let changes = vec![
        unchanged(stripe(0, base, 1000)),
        unchanged(stripe(1, base + 1000, 2000)),
        unchanged(stripe(2, base + 3000, 500)),
    ];

    let plan = plan_stripe_reuse(&changes, base);

    assert!(plan.reuses_every_stripe());
    assert_eq!(plan.reused_bytes(), 3500);
    assert_eq!(
        plan.dispositions[1],
        StripeDisposition::Reuse {
            byte_len: 2000,
            source_offset: base + 1000,
        }
    );
}

#[test]
fn a_changed_or_uncertain_stripe_is_rebuilt_without_blocking_other_reuse() {
    let base = 4096;
    let mut second = unchanged(stripe(1, base + 1000, 2000));
    second.changed = true;
    let changes = vec![
        unchanged(stripe(0, base, 1000)),
        second,
        unchanged(stripe(2, base + 3000, 500)),
    ];

    let plan = plan_stripe_reuse(&changes, base);

    assert!(!plan.reuses_every_stripe());
    assert_eq!(plan.dispositions[1], StripeDisposition::Rebuild { stripe_id: 1 });
    // A stripe after the rebuilt one still reuses its bytes and its independently-addressed checksum leaf.
    assert_eq!(
        plan.dispositions[2],
        StripeDisposition::Reuse {
            byte_len: 500,
            source_offset: base + 3000,
        }
    );
    assert_eq!(plan.reused_bytes(), 1500);
}

#[test]
fn any_doubt_rebuilds_the_stripe() {
    let base = 4096;
    let mut doubtful = unchanged(stripe(0, base, 1000));
    doubtful.uncertain = true;
    let plan = plan_stripe_reuse(&[doubtful], base);
    assert_eq!(plan.dispositions[0], StripeDisposition::Rebuild { stripe_id: 0 });
}

// --- Relocated-stripe reuse -------------------------------------------------------------------------------------------

fn row(i: u64, note: &str) -> HefRow {
    let mut payload = std::collections::BTreeMap::new();
    payload.insert("kind".to_owned(), VariantValue::String(format!("k{}", i % 4)));
    payload.insert("amount".to_owned(), VariantValue::Int(1_000 + i as i64));
    payload.insert("note".to_owned(), VariantValue::String(note.to_owned()));
    if i.is_multiple_of(3) {
        payload.insert("rare".to_owned(), VariantValue::Bool(true));
    }
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xBEEF_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(7),
                stream_id: StreamId(2),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000_000 + i as i64 * 1000),
                ingested_at: TimestampValue::from_physical_nanos(2_000_000 + i as i64 * 1000),
                source: ["crm", "billing"][i as usize % 2].to_owned(),
                event_type: "deal.updated".to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: i,
                entity_id_hash_high: 1,
                entity_id: Some(format!("opp-{i}")),
                actor_id_hash_low: 3,
                actor_id: i.is_multiple_of(2).then(|| "actor-1".to_owned()),
                account_id_hash_low: 4,
                account_id: None,
                trace_id_hash_low: 5,
                dedupe_hash_low: 100 + i,
                dedupe_hash_high: 6,
                schema_version: 2,
                flags: EventFlags(0),
            },
            payload: PayloadInput::Variant(VariantValue::Object(payload)),
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: i,
            connector_delivery_hash_high: 0,
            provenance: None,
            relationships: None,
        },
    }
}

fn config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration {
            fields: vec!["note".to_owned()],
        },
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan {
            columns: vec![PromotedColumn {
                name: "kind".to_owned(),
                path: "kind".to_owned(),
                kind: ColumnKind::String,
                since_schema_version: 1,
                substring_searchable: false,
            }],
        },
        // One granule per stripe (a tiny stripe target), so changing one row shifts every later stripe to a new offset
        // without disturbing its bytes — the relocation the reuse path is built to exploit.
        targets: LayoutTargets {
            index_granularity: 8,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 1,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(7),
    }
}

fn build(rows: &[HefRow]) -> BuiltHef {
    build_hef_file(rows.to_vec(), &config()).unwrap()
}

/// A deterministic high-entropy string of `len` printable characters — incompressible enough that using it as a row's
/// free text visibly grows that row's stripe, so later stripes relocate to new offsets.
fn noisy(len: usize) -> String {
    (0..len as u64)
        .map(|k| char::from(b'!' + (k.wrapping_mul(2_654_435_761) ^ (k >> 3)).wrapping_rem(90) as u8))
        .collect()
}

/// Reassembles `replacement` from a plan against `source` the way an object store executes the spans: each reused
/// stripe copied from the source bytes, each fresh span taken from the replacement bytes.
fn splice(source: &BuiltHef, replacement: &BuiltHef, plan: &RewritePlan) -> Vec<u8> {
    let segments = rewrite_segments(plan, &replacement.footer.stripes, replacement.bytes.len() as u64, 0);
    let mut spliced = Vec::with_capacity(replacement.bytes.len());
    for segment in segments {
        match segment {
            RewriteSegment::CopyFromSource { len, source_offset } => {
                spliced.extend_from_slice(&source.bytes[source_offset as usize..(source_offset + len) as usize]);
            }
            RewriteSegment::Fresh { len } => {
                let offset = spliced.len();
                spliced.extend_from_slice(&replacement.bytes[offset..offset + len as usize]);
            }
        }
    }
    spliced
}

#[test]
fn plan_rewrite_reuses_unchanged_stripes_of_a_real_file_byte_identically() {
    // A small append: the leading rows are unchanged, so their stripes are reused; only the appended tail (and the
    // header and footer) are rebuilt.
    let source = build(&(0..48).map(|i| row(i, "a short note")).collect::<Vec<_>>());
    let replacement = build(&(0..64).map(|i| row(i, "a short note")).collect::<Vec<_>>());

    let plan = plan_rewrite(&source, &replacement);
    assert!(
        plan.reused_bytes() > 0,
        "unchanged leading stripes are copied by reference"
    );
    assert!(!plan.reuses_every_stripe(), "the appended tail is rebuilt");

    let spliced = splice(&source, &replacement, &plan);
    assert_eq!(
        spliced, replacement.bytes,
        "the spliced file is byte-identical to a full rewrite"
    );

    // Reader-indistinguishable: it reopens as an ordinary HEF verified against its own authoritative segment seal.
    let reopened = HefFile::open(spliced, Some(&replacement.file_seal)).unwrap();
    assert_eq!(reopened.header().row_count, 64);
}

#[test]
fn plan_rewrite_reuses_a_relocated_stripe_from_its_new_offset() {
    // Enlarging an early row's free text grows its stripe, shifting every later stripe to a new offset while its rows —
    // and therefore its bytes and per-stripe BLAKE3 — stay identical. Stripe-relative addressing lets those relocated
    // stripes be copied by reference from where they live in the source into their new place in the replacement.
    let mut source_rows: Vec<HefRow> = (0..64).map(|i| row(i, "small")).collect();
    let replacement_rows: Vec<HefRow> = (0..64).map(|i| row(i, "small")).collect();
    source_rows[0] = row(0, "small"); // keep source as the baseline
    let source = build(&source_rows);
    // The replacement changes only row 0, enlarging the first stripe and pushing the rest downward.
    let mut changed_rows = replacement_rows;
    changed_rows[0] = row(0, &noisy(4_000));
    let replacement = build(&changed_rows);

    let plan = plan_rewrite(&source, &replacement);

    // At least one reused stripe is copied from a source offset different from where it lands in the replacement.
    let segments = rewrite_segments(&plan, &replacement.footer.stripes, replacement.bytes.len() as u64, 0);
    let mut dest_cursor = 0u64;
    let mut relocated_copies = 0;
    for segment in &segments {
        if let RewriteSegment::CopyFromSource { source_offset, len } = *segment {
            if source_offset != dest_cursor {
                relocated_copies += 1;
            }
            dest_cursor += len;
        } else if let RewriteSegment::Fresh { len } = *segment {
            dest_cursor += len;
        }
    }
    assert!(
        relocated_copies > 0,
        "a relocated unchanged stripe is copied by reference to its new offset"
    );

    // And the reassembled file is still byte-identical to a full rewrite and reopens cleanly.
    let spliced = splice(&source, &replacement, &plan);
    assert_eq!(spliced, replacement.bytes);
    HefFile::open(spliced, Some(&replacement.file_seal)).unwrap();
}

/// A no-op rewrite reuses every stripe and preserves the already-derived segment seal.
#[test]
fn plan_rewrite_of_an_identical_file_reuses_everything() {
    let rows: Vec<HefRow> = (0..64).map(|i| row(i, "same note")).collect();
    let source = build(&rows);
    let replacement = build(&rows);
    assert_eq!(
        source.bytes, replacement.bytes,
        "deterministic builds are byte-identical"
    );

    let plan = plan_rewrite(&source, &replacement);
    assert!(plan.reuses_every_stripe());
    assert_eq!(source.file_seal, replacement.file_seal);
}

#[test]
fn plan_rewrite_rebuilds_a_changed_stripe() {
    let source = build(&(0..32).map(|i| row(i, "before")).collect::<Vec<_>>());
    // Change a value in the first stripe's rows so that stripe cannot be reused.
    let mut rows: Vec<HefRow> = (0..32).map(|i| row(i, "before")).collect();
    rows[0] = row(0, "AFTER — a different note entirely");
    let replacement = build(&rows);

    let plan = plan_rewrite(&source, &replacement);
    assert!(
        plan.dispositions
            .iter()
            .any(|d| matches!(d, StripeDisposition::Rebuild { .. })),
        "the changed stripe is rebuilt"
    );
    assert_eq!(splice(&source, &replacement, &plan), replacement.bytes);
}

// --- Signed events survive compaction well enough to re-verify from the compacted file alone --------------------------

/// `row(i, ..)` with its payload replaced by the content and tags the author actually hashed, plus the provenance that
/// attests them.
fn signed_row(author: &SimulatedEventAuthor, i: u64, note: &str) -> HefRow {
    let payload = signed_event_payload(note, &[&["e", "root"], &["p", "peer"]]);
    let provenance = author
        .sign(1, 1_700_000_000 + i as i64, &payload)
        .expect("the payload is well-shaped");
    let mut row = row(i, note);
    row.event.payload = PayloadInput::Variant(payload);
    row.event.provenance = Some(provenance);
    row
}

/// Rebuilds the provenance of every row of `file` from the columns it stores, in row order.
fn stored_provenance(file: &HefFile) -> Vec<SignedEventProvenance> {
    let mut out = Vec::new();
    for granule in &file.footer().granules {
        let read = |column_id: u32| {
            file.read_column(column_id, granule.granule_id)
                .expect("the provenance family is materialized")
                .data
        };
        let (
            ColumnData::Strings(pubkeys),
            ColumnData::Strings(signatures),
            ColumnData::Strings(ids),
            ColumnData::Strings(schemes),
            ColumnData::I64(kinds),
            ColumnData::I64(claimed),
        ) = (
            read(column_ids::AUTHOR_PUBKEY),
            read(column_ids::SIGNATURE),
            read(column_ids::PROTOCOL_EVENT_ID),
            read(column_ids::SIGNATURE_SCHEME),
            read(column_ids::PROTOCOL_KIND),
            read(column_ids::CLAIMED_AT),
        )
        else {
            panic!("provenance columns keep their declared types");
        };
        for index in 0..granule.row_count as usize {
            let text = |column: &StringColumn| {
                column
                    .get(index)
                    .flatten()
                    .expect("a stored provenance value")
                    .to_owned()
            };
            out.push(SignedEventProvenance {
                author_pubkey: hex_bytes(&text(&pubkeys)).expect("lowercase hex"),
                claimed_at: TimestampValue::from_physical_nanos(*claimed.get(index).expect("a claimed_at")),
                protocol_event_id: hex_bytes(&text(&ids)).expect("lowercase hex"),
                protocol_kind: u32::try_from(*kinds.get(index).expect("a kind")).expect("kinds are small"),
                scheme: SignatureScheme::from_str(&text(&schemes)).expect("the tag is in the registry"),
                signature: hex_bytes(&text(&signatures)).expect("lowercase hex"),
            });
        }
    }
    out
}

#[test]
fn a_signed_event_re_verifies_offline_from_a_compacted_file_that_reused_its_stripes() {
    // The whole archive path in one test: sign events, seal and publish a HEF, compact it into a larger file by
    // splicing the published stripes forward, then read the compacted file back knowing nothing but its bytes —
    // rebuild the canonical serialization, recompute the protocol id, and check the signature. No wire bytes anywhere.
    let author = SimulatedEventAuthor::from_seed(6);
    let published: Vec<HefRow> = (0..24).map(|i| signed_row(&author, i, "a short note")).collect();
    let compacted_rows: Vec<HefRow> = (0..40).map(|i| signed_row(&author, i, "a short note")).collect();

    let source = build(&published);
    let replacement = build(&compacted_rows);
    let plan = plan_rewrite(&source, &replacement);
    assert!(
        plan.reused_bytes() > 0,
        "the published stripes carry forward into the compacted file by reference"
    );

    let compacted = splice(&source, &replacement, &plan);
    assert_eq!(
        compacted, replacement.bytes,
        "a spliced compaction is byte-identical to a full rewrite"
    );
    let file = HefFile::open(compacted, Some(&replacement.file_seal)).expect("the compacted file opens");

    let stored = stored_provenance(&file);
    assert_eq!(stored.len(), compacted_rows.len());
    for (index, row) in compacted_rows.iter().enumerate() {
        let provenance = stored.get(index).expect("one provenance per row");
        assert_eq!(
            Some(provenance),
            row.event.provenance.as_ref(),
            "row {index} survives compaction byte-exactly"
        );

        let PayloadRead::Value(payload) = file.payload(index as u64).expect("the payload reads back") else {
            panic!("a signed event carries an inline payload");
        };
        let canonical = provenance.canonical_bytes(&payload).expect("well-shaped payload");
        assert_eq!(
            SignedEventProvenance::recompute_protocol_event_id(&canonical),
            provenance.protocol_event_id,
            "row {index}'s id recomputes from the compacted bytes"
        );
        provenance
            .verify(&payload)
            .unwrap_or_else(|error| panic!("compacted row {index} must re-verify offline: {error}"));
    }
}
