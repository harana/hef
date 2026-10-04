use super::*;
use crate::typed_id::TypedIdTestExt;
use crate::writer::sim::SimSafeRetryStore;

fn receipt(tenant_id: TenantId, status_class: StatusClass) -> RetryReceipt {
    RetryReceipt {
        commit: SequencePoint { epoch: 1, sequence: 1 },
        dedupe: (100, 0),
        delivery_identity: (1, 0),
        expiry_physical_nanos: 1_000,
        frame_offset: 0,
        shard: ShardId(0),
        status_class,
        tenant_id,
    }
}

#[test]
fn duplicate_within_guard_follows_the_acknowledged_receipt() {
    let tenant = TenantId::new_test_id(9);
    let mut retry = SimSafeRetryStore::new();
    retry.record(receipt(tenant, StatusClass::Acknowledged));

    assert_eq!(
        retry.acknowledged_within_guard(tenant, (100, 0), 0),
        Some(receipt(tenant, StatusClass::Acknowledged)),
        "the original receipt comes back for a retry inside the window"
    );
    assert!(retry.duplicate_within_guard(tenant, (100, 0), 0));
    assert!(!retry.duplicate_within_guard(TenantId::new_test_id(10), (100, 0), 0));
    assert!(!retry.duplicate_within_guard(tenant, (100, 0), 1_000));
}

#[test]
fn an_indeterminate_receipt_is_not_a_duplicate() {
    let tenant = TenantId::new_test_id(9);
    let mut retry = SimSafeRetryStore::new();
    retry.record(receipt(tenant, StatusClass::Indeterminate));

    assert_eq!(retry.acknowledged_within_guard(tenant, (100, 0), 0), None);
    assert!(!retry.duplicate_within_guard(tenant, (100, 0), 0));
}
