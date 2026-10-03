use super::*;
use crate::writer::compaction::{StripeChange, plan_stripe_reuse};

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
fn rewrite_segments_reuses_unchanged_stripes_and_uploads_the_rest() {
    let base = 6u64;
    let stripes = vec![stripe(0, base, 4), stripe(1, base + 4, 4)];
    let mut changes = vec![unchanged(stripes[0]), unchanged(stripes[1])];
    changes[1].changed = true;
    let plan = plan_stripe_reuse(&changes, base);

    let file_len = base + 8 + 6; // header 6 + two 4-byte stripes + footer 6
    let segments = rewrite_segments(&plan, &stripes, file_len, 0);

    assert_eq!(
        segments,
        vec![
            RewriteSegment::Fresh { len: 6 },
            RewriteSegment::CopyFromSource {
                len: 4,
                source_offset: base,
            },
            RewriteSegment::Fresh { len: 4 },
            RewriteSegment::Fresh { len: 6 },
        ]
    );
}

#[test]
fn a_sub_minimum_reused_stripe_is_downgraded_to_a_rebuild() {
    let base = 6u64;
    let stripes = vec![stripe(0, base, 4)];
    let plan = plan_stripe_reuse(&[unchanged(stripes[0])], base);

    // A 4-byte stripe below an 8-byte minimum part size cannot be a whole copy part, so it is rebuilt instead.
    let downgraded = rewrite_segments(&plan, &stripes, base + 4, 8);
    assert_eq!(
        downgraded,
        vec![RewriteSegment::Fresh { len: 6 }, RewriteSegment::Fresh { len: 4 }]
    );
    // With no minimum, the same stripe is copied by reference.
    let copied = rewrite_segments(&plan, &stripes, base + 4, 0);
    assert_eq!(
        copied[1],
        RewriteSegment::CopyFromSource {
            len: 4,
            source_offset: base,
        }
    );
}
