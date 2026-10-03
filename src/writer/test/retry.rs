use super::*;
use crate::typed_id::TypedIdTestExt;

fn receipt(
    tenant_id: TenantId,
    delivery_identity: (u64, u64),
    dedupe: (u64, u64),
    expiry_physical_nanos: i64,
) -> RetryReceipt {
    RetryReceipt {
        commit: SequencePoint { epoch: 1, sequence: 1 },
        dedupe,
        delivery_identity,
        expiry_physical_nanos,
        frame_offset: 0,
        shard: ShardId(0),
        status_class: StatusClass::Acknowledged,
        tenant_id,
    }
}

#[test]
fn expired_receipts_are_evicted_and_stop_suppressing() {
    let tenant = TenantId::new_test_id(9);
    let mut retry = SafeRetryStore::new();
    retry.record(receipt(tenant, (1, 0), (100, 0), 1_000));
    retry.record(receipt(tenant, (2, 0), (200, 0), 5_000));

    // Within the window both suppress and nothing is evicted.
    assert!(retry.duplicate_within_guard(tenant, (100, 0), 500));
    assert!(retry.duplicate_within_guard(tenant, (200, 0), 500));
    assert_eq!(retry.len(), 2);

    // Past the first receipt's window it stops suppressing and its row is dropped; the unexpired one stays.
    assert!(!retry.duplicate_within_guard(tenant, (100, 0), 2_000));
    assert_eq!(retry.len(), 1);
    assert!(retry.lookup(tenant, (1, 0)).is_none());
    assert!(retry.lookup(tenant, (2, 0)).is_some());

    // A record-only owner bounds the store with the explicit sweep.
    retry.evict_expired(10_000);
    assert!(retry.is_empty());
}

#[test]
fn overwriting_a_receipt_reindexes_its_dedupe_identity() {
    let tenant = TenantId::new_test_id(9);
    let mut retry = SafeRetryStore::new();
    retry.record(receipt(tenant, (1, 0), (100, 0), 1_000));
    // The same delivery identity re-records under a different dedupe identity.
    retry.record(receipt(tenant, (1, 0), (200, 0), 1_000));

    assert_eq!(retry.len(), 1);
    assert!(
        !retry.duplicate_within_guard(tenant, (100, 0), 0),
        "the overwritten dedupe identity must no longer suppress"
    );
    assert!(retry.duplicate_within_guard(tenant, (200, 0), 0));
}

#[test]
fn rejected_receipts_never_suppress() {
    let tenant = TenantId::new_test_id(9);
    let mut retry = SafeRetryStore::new();
    let mut rejected = receipt(tenant, (1, 0), (100, 0), 1_000);
    rejected.status_class = StatusClass::Rejected;
    retry.record(rejected);
    assert!(!retry.duplicate_within_guard(tenant, (100, 0), 0));
}

#[test]
fn a_refreshed_receipt_outlives_its_stale_queue_entry() {
    let tenant = TenantId::new_test_id(9);
    let mut retry = SafeRetryStore::new();
    retry.record(receipt(tenant, (1, 0), (100, 0), 1_000));
    // The same delivery re-records with a later expiry; the eviction queue still holds the stale 1_000 entry.
    retry.record(receipt(tenant, (1, 0), (100, 0), 5_000));

    retry.evict_expired(2_000);
    assert_eq!(retry.len(), 1, "the refreshed row survives the stale queue entry");
    assert!(retry.duplicate_within_guard(tenant, (100, 0), 2_000));
}
