use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotedColumn, PromotionPlan, column_ids};
use crate::encoding::{ColumnData, StringColumn};
use crate::events::provenance::{SignatureScheme, SignedEventProvenance, hex_bytes};
use crate::events::sim::{SimulatedEventAuthor, signed_event_payload};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, RelationshipKind, RelationshipRef, StreamId, TenantId};
use crate::layout::LayoutTargets;
use crate::layout::footer::{ColumnKind, GraphDirectory};
use crate::layout::graph::tests::room_graph;
use crate::layout::graph::{ChainLabel, ChainLink, UnresolvedEdge};
use crate::layout::reader::{HefFile, PayloadRead};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuildRow, HefBuildConfig, HefRow, build_hef_file};
use crate::writer::rewrite::{RewriteSegment, rewrite_segments};
use crate::writer::source_form::tests::{source_form_config, source_form_row};
use std::collections::BTreeMap;

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
        audit: None,
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration {
            fields: vec!["note".to_owned()],
        },
        freetext_row_offset_index: false,
        generation_id: 1,
        graph: None,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        profile: crate::layout::Profile::Event,
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
        time_series_bucket_nanos: None,
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

    let plan = plan_rewrite(&source, &replacement).unwrap();
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

    let plan = plan_rewrite(&source, &replacement).unwrap();

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

    let plan = plan_rewrite(&source, &replacement).unwrap();
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

    let plan = plan_rewrite(&source, &replacement).unwrap();
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
    let plan = plan_rewrite(&source, &replacement).unwrap();
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

// --- Graph rewrites ---------------------------------------------------------------------------------------------------

const SUBJECT: &[u8] = b"!room:example.org";

/// The parents of event `sn` in a forty-event room history: a prev chain, every fifth event from 15 on also naming
/// the event twelve back (so edges cross file boundaries), and every event but the first authorized by event 0 and,
/// for every fourth from 11 on, by the event five back.
fn truth_parents(sn: u64, kind: EdgeKind) -> Vec<u64> {
    let mut parents = Vec::new();
    match kind {
        EdgeKind::Prev => {
            parents.extend(sn.checked_sub(1));
            if sn >= 12 && sn.is_multiple_of(5) {
                parents.push(sn - 12);
            }
        }
        EdgeKind::Auth => {
            parents.extend((sn > 0).then_some(0));
            if sn >= 8 && sn % 4 == 3 {
                parents.push(sn - 5);
            }
        }
    }
    parents
}

/// Every child of event `sn` along `kind` edges among the forty events, ascending.
fn truth_children(sn: u64, kind: EdgeKind) -> Vec<u64> {
    (sn + 1..40)
        .filter(|&child| truth_parents(child, kind).contains(&sn))
        .collect()
}

fn graph_event_id(sn: u64) -> Vec<u8> {
    format!("${sn}:example.org").into_bytes()
}

fn graph_row(sn: u64, body: &str) -> BuildRow {
    let mut row = source_form_row(
        sn,
        VariantValue::Object(BTreeMap::from([(
            "body".to_owned(),
            VariantValue::String(body.to_owned()),
        )])),
    );
    row.external_id = Some(graph_event_id(sn));
    row
}

/// The graph of events `first..end`, with chain-cover labels, one link, a depth range, an unresolved parent on event
/// 15, and the inbound edges from the events of `inbound_from`, as a compaction that saw those events recorded them.
fn graph_input(first: u64, end: u64, inbound_from: std::ops::Range<u64>) -> GraphInput {
    let mut inbound = Vec::new();
    for child in inbound_from {
        for kind in [EdgeKind::Auth, EdgeKind::Prev] {
            for parent in truth_parents(child, kind) {
                if (first..end).contains(&parent) {
                    inbound.push(GraphEdge { child, kind, parent });
                }
            }
        }
    }
    GraphInput {
        chain_cover: Some(ChainCoverInput {
            labels: (first..end)
                .map(|sn| ChainLabel {
                    chain_id: 1 + (sn % 2) as u32,
                    seq: (sn / 2) as u32,
                })
                .collect(),
            links: vec![ChainLink {
                from_chain: 2,
                from_seq: (first / 2) as u32,
                to_chain: 1,
                to_seq: (first / 2) as u32,
            }],
        }),
        depth: Some(DepthRange {
            max: 100 + end - 1,
            min: 100 + first,
        }),
        first_sn: first,
        inbound,
        rows: (first..end)
            .map(|sn| RowEdges {
                auth: truth_parents(sn, EdgeKind::Auth),
                prev: truth_parents(sn, EdgeKind::Prev),
            })
            .collect(),
        subject: SUBJECT.to_vec(),
        unresolved: (first..end)
            .filter(|&sn| sn == 15)
            .map(|child| UnresolvedEdge {
                child,
                parent: RelationshipRef::to_external(RelationshipKind::Prev, b"$late:example.org").unwrap(),
            })
            .collect(),
    }
}

fn built_graph(rows: Vec<BuildRow>, graph: GraphInput) -> BuiltHef {
    let mut config = source_form_config(4);
    config.graph = Some(graph);
    config.profile = crate::layout::Profile::Graph;
    build_hef_file(rows, &config).unwrap()
}

fn open_graph(built: &BuiltHef) -> HefFile {
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap()
}

fn graph_file(first: u64, end: u64, inbound_from: std::ops::Range<u64>) -> HefFile {
    let rows = (first..end).map(|sn| graph_row(sn, "hello")).collect();
    open_graph(&built_graph(rows, graph_input(first, end, inbound_from)))
}

/// How many edges of `file` (events `first..end`) run to a child at or past `end`, from its inbound tables.
fn inbound_count(file: &HefFile, first: u64, end: u64) -> usize {
    (first..end)
        .map(|sn| {
            let edges = file.granule_edges(file.graph_granule_of(sn).unwrap()).unwrap();
            [EdgeKind::Auth, EdgeKind::Prev]
                .into_iter()
                .map(|kind| {
                    edges
                        .children(sn, kind)
                        .into_iter()
                        .filter(|&child| child >= end)
                        .count()
                })
                .sum::<usize>()
        })
        .sum()
}

#[test]
fn merging_graph_files_keeps_every_edge_and_every_id_lookup() {
    // Files of events 0-9, 10-19 and 20-29 merge; 30-39 stays a later file. The first file was last rewritten while
    // 10-19 was its later file, so it already lists their edges into it as inbound.
    let sources = [
        graph_file(0, 10, 10..20),
        graph_file(10, 20, 0..0),
        graph_file(20, 30, 0..0),
    ];
    let later = [graph_file(30, 40, 0..0)];
    assert!(inbound_count(&sources[0], 0, 10) > 0);

    let graph = rebuild_graph_input(&sources, &later).unwrap();
    let file = open_graph(&built_graph((0..30).map(|sn| graph_row(sn, "hello")).collect(), graph));

    for sn in 0..30 {
        let edges = file.granule_edges(file.graph_granule_of(sn).unwrap()).unwrap();
        for kind in [EdgeKind::Auth, EdgeKind::Prev] {
            assert_eq!(edges.parents(sn, kind), truth_parents(sn, kind), "parents of {sn}");
            assert_eq!(edges.children(sn, kind), truth_children(sn, kind), "children of {sn}");
        }
        let source = &sources[(sn / 10) as usize];
        assert_eq!(source.graph_sn_of(&graph_event_id(sn)).unwrap(), Some(sn));
        assert_eq!(file.graph_sn_of(&graph_event_id(sn)).unwrap(), Some(sn));
    }
    assert_eq!(file.graph_sn_of(&graph_event_id(35)).unwrap(), None);
    // Every cross-file edge among the sources is in-file now, so the inbound edges are exactly the later file's.
    let from_later: usize = (30..40)
        .map(|child| {
            [EdgeKind::Auth, EdgeKind::Prev]
                .into_iter()
                .map(|kind| {
                    truth_parents(child, kind)
                        .into_iter()
                        .filter(|&parent| parent < 30)
                        .count()
                })
                .sum::<usize>()
        })
        .sum();
    assert!(from_later > 0);
    assert_eq!(inbound_count(&file, 0, 30), from_later);

    let sns: Vec<u64> = (0..30).collect();
    let labels: Vec<Option<ChainLabel>> = sns
        .iter()
        .map(|&sn| {
            Some(ChainLabel {
                chain_id: 1 + (sn % 2) as u32,
                seq: (sn / 2) as u32,
            })
        })
        .collect();
    assert_eq!(file.graph_chain_labels(&sns).unwrap(), labels);
    assert_eq!(file.graph_chain_links(&[2]).len(), 3, "one link from each source");
    assert_eq!(file.graph_unresolved_parents(15).len(), 1);
    assert_eq!(
        file.footer().graph.as_ref().unwrap().depth,
        Some(DepthRange { max: 129, min: 100 })
    );
}

#[test]
fn a_graph_rewrite_refuses_sources_that_do_not_join_into_one_subject() {
    let refused = |sources: &[HefFile], later: &[HefFile]| {
        matches!(rebuild_graph_input(sources, later), Err(FormatError::Structural { .. }))
    };
    // Events 10-19, changed by `edit`.
    let second = |edit: fn(&mut GraphInput)| {
        let mut graph = graph_input(10, 20, 0..0);
        edit(&mut graph);
        open_graph(&built_graph((10..20).map(|sn| graph_row(sn, "hello")).collect(), graph))
    };
    let other_subject: fn(&mut GraphInput) = |graph| graph.subject = b"!other:example.org".to_vec();
    assert!(refused(&[], &[]), "no sources");
    assert!(
        refused(&[graph_file(0, 10, 0..0), graph_file(20, 30, 0..0)], &[]),
        "a gap between sources"
    );
    assert!(
        refused(&[graph_file(20, 30, 0..0)], &[graph_file(0, 10, 0..0)]),
        "a later file that starts inside"
    );
    assert!(
        refused(&[graph_file(0, 10, 0..0), second(other_subject)], &[]),
        "two subjects"
    );
    assert!(
        refused(&[graph_file(0, 10, 0..0)], &[second(other_subject)]),
        "a later file of another subject"
    );
    assert!(
        refused(
            &[graph_file(0, 10, 0..0), second(|graph| graph.chain_cover = None)],
            &[]
        ),
        "a chain cover on only some sources"
    );
    assert!(rebuild_graph_input(&[graph_file(0, 10, 0..0), second(|_| {})], &[]).is_ok());
}

/// The bytes of each of a built graph file's edge blocks, in directory order.
fn edge_blocks(built: &BuiltHef) -> Vec<Vec<u8>> {
    let footer = &built.footer;
    footer
        .graph
        .as_ref()
        .unwrap()
        .blocks
        .iter()
        .map(|block| {
            let granule = footer
                .granules
                .iter()
                .find(|granule| granule.granule_id == block.granule_id)
                .unwrap();
            let base = footer
                .stripes
                .iter()
                .find(|stripe| stripe.stripe_id == granule.stripe_id)
                .unwrap()
                .file_offset;
            let start = (base + block.offset) as usize;
            built.bytes[start..start + block.len as usize].to_vec()
        })
        .collect()
}

/// A body of 600 characters no other event's body shares, so it does not compress away.
fn distinct_body(sn: u64) -> String {
    (0..600u64)
        .map(|k| char::from(b'!' + ((k ^ (sn << 10)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as u8))
        .collect()
}

#[test]
fn redacting_bodies_leaves_the_graph_byte_identical() {
    let original = built_graph(
        (10..30).map(|sn| graph_row(sn, &distinct_body(sn))).collect(),
        graph_input(10, 30, 30..40),
    );

    // Redaction strips the bodies of events 12, 17 and 25; their edges, labels and ids stay.
    let redacted = built_graph(
        (10..30)
            .map(|sn| {
                let body = if [12, 17, 25].contains(&sn) {
                    String::new()
                } else {
                    distinct_body(sn)
                };
                graph_row(sn, &body)
            })
            .collect(),
        rebuild_graph_input(&[open_graph(&original)], &[]).unwrap(),
    );
    let offsets = |built: &BuiltHef| -> Vec<u64> {
        built
            .footer
            .graph
            .as_ref()
            .unwrap()
            .blocks
            .iter()
            .map(|block| block.offset)
            .collect()
    };
    assert_ne!(
        offsets(&redacted),
        offsets(&original),
        "the stripped bodies moved the edge blocks that follow them"
    );

    assert_eq!(edge_blocks(&redacted), edge_blocks(&original));
    let (before, after) = (
        original.footer.graph.as_ref().unwrap(),
        redacted.footer.graph.as_ref().unwrap(),
    );
    assert_eq!(after.chain_cover, before.chain_cover);
    assert_eq!(after.depth, before.depth);
    assert_eq!(after.event_ids, before.event_ids);
    assert_eq!(after.first_sn, before.first_sn);
    assert_eq!(after.subject, before.subject);
    assert_eq!(after.unresolved, before.unresolved);
    let shape = |directory: &GraphDirectory| -> Vec<(u32, u64)> {
        directory
            .blocks
            .iter()
            .map(|block| (block.granule_id, block.len))
            .collect()
    };
    assert_eq!(
        shape(after),
        shape(before),
        "each edge block keeps its granule and length; only where it sits moves"
    );
}

/// Seals a 100,000-event room-shaped graph into per-subject files of 4096 events, then compacts them all into one
/// file and prints the edge-block bytes per edge before and after, the whole output's bytes per event, and how long
/// the rebuild and the build took. Run by hand:
/// `cargo test --release --features write --lib graph_compaction_of_a_room -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode graph compaction benchmark"]
fn graph_compaction_of_a_room() {
    let events = 100_000u64;
    let seal = 4096u64;
    let room = room_graph(events);
    let edges: u64 = room
        .rows
        .iter()
        .map(|row| (row.auth.len() + row.prev.len()) as u64)
        .sum();
    let rows_of = |first: u64, end: u64| -> Vec<BuildRow> { (first..end).map(|sn| graph_row(sn, "hi")).collect() };
    let build = |rows: Vec<BuildRow>, graph: GraphInput| {
        let mut config = source_form_config(8192);
        config.graph = Some(graph);
        config.profile = crate::layout::Profile::Graph;
        build_hef_file(rows, &config).unwrap()
    };
    let edge_bytes = |built: &BuiltHef| -> usize { edge_blocks(built).iter().map(Vec::len).sum() };

    let sealed: Vec<BuiltHef> = (0..events.div_ceil(seal))
        .map(|index| {
            let (first, end) = (index * seal, ((index + 1) * seal).min(events));
            let graph = GraphInput {
                first_sn: first,
                rows: room.rows[first as usize..end as usize].to_vec(),
                subject: room.subject.clone(),
                ..GraphInput::default()
            };
            build(rows_of(first, end), graph)
        })
        .collect();
    let sources: Vec<HefFile> = sealed.iter().map(open_graph).collect();
    assert!(sources.len() >= 8);

    let started = std::time::Instant::now();
    let graph = rebuild_graph_input(&sources, &[]).unwrap();
    let rebuilt = started.elapsed();
    let rows = rows_of(0, events);
    let started = std::time::Instant::now();
    let merged = build(rows, graph);
    let built = started.elapsed();

    let file = open_graph(&merged);
    for sn in (0..events).step_by(997) {
        let edges = file.granule_edges(file.graph_granule_of(sn).unwrap()).unwrap();
        let row = &room.rows[sn as usize];
        assert_eq!(edges.parents(sn, EdgeKind::Prev), row.prev);
        assert_eq!(edges.parents(sn, EdgeKind::Auth), row.auth);
        assert_eq!(file.graph_sn_of(&graph_event_id(sn)).unwrap(), Some(sn));
    }
    let sealed_edge_bytes: usize = sealed.iter().map(edge_bytes).sum();
    let sealed_bytes: usize = sealed.iter().map(|built| built.bytes.len()).sum();
    println!(
        "graph compaction: {} events, {edges} edges, {} sealed files -> 1",
        events,
        sealed.len()
    );
    println!(
        "  edge blocks: {:.2} bytes/edge sealed, {:.2} bytes/edge compacted",
        sealed_edge_bytes as f64 / edges as f64,
        edge_bytes(&merged) as f64 / edges as f64
    );
    println!(
        "  whole files: {:.1} bytes/event sealed, {:.1} bytes/event compacted ({} bytes)",
        sealed_bytes as f64 / events as f64,
        merged.bytes.len() as f64 / events as f64,
        merged.bytes.len()
    );
    println!(
        "  rebuild_graph_input {:.1} ms, build {:.1} ms",
        rebuilt.as_secs_f64() * 1e3,
        built.as_secs_f64() * 1e3
    );
}
