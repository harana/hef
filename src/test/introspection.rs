//! Unit tests for the introspection system tables: authorization is enforced and the public column set carries no
//! secrets.

use super::*;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;

fn record(file_id: u128, tenant: TenantId) -> HefFileRecord {
    HefFileRecord {
        created_at: 1,
        embedding_sample: vec![0.1, 0.2],
        file_seal: [7; 32],
        file_id,
        file_size: 4_096,
        generation_id: 2,
        granule_count: 3,
        local_path: "/var/lib/harana/hef/0xA.hef".to_owned(),
        object_store_credential: "AKIA-secret".to_owned(),
        payload_bytes: vec![1, 2, 3],
        row_count: 100,
        tenant_id: tenant,
    }
}

#[test]
fn public_callers_cannot_read_system_tables() {
    let records = [record(0xA, TenantId::new_test_id(7))];
    assert_eq!(
        system_hef_files(IntrospectionCaller::Public, &records),
        Err(IntrospectionError::Unauthorized)
    );
}

#[test]
fn tenant_sees_only_its_own_files_admin_sees_all() {
    let records = [
        record(0xA, TenantId::new_test_id(7)),
        record(0xB, TenantId::new_test_id(8)),
    ];
    let tenant_rows = system_hef_files(IntrospectionCaller::Tenant(TenantId::new_test_id(7)), &records).unwrap();
    assert_eq!(tenant_rows.len(), 1);
    assert_eq!(tenant_rows[0].file_id, 0xA);

    let admin_rows = system_hef_files(IntrospectionCaller::Admin, &records).unwrap();
    assert_eq!(admin_rows.len(), 2);
}

#[test]
fn debug_output_withholds_every_secret_field() {
    // A `{:?}` of the internal record — a debug log, a panic message, a failed assertion — must not print the
    // object-store credential, local path, payload bytes, or embedding sample it carries.
    let record = record(0xA, TenantId::new_test_id(7));
    let printed = format!("{record:?}");

    assert!(!printed.contains("AKIA-secret"), "credential value leaked: {printed}");
    assert!(!printed.contains("/var/lib/harana"), "local path leaked: {printed}");
    for withheld in WITHHELD_FROM_INTROSPECTION {
        assert!(
            !printed.contains(withheld),
            "Debug output must omit the `{withheld}` field entirely: {printed}"
        );
    }
    assert!(
        !printed.contains("embedding_sample"),
        "embedding field leaked: {printed}"
    );
    // The public-safe fields still print, so the record stays useful in diagnostics.
    assert!(
        printed.contains("file_id"),
        "public metadata should still print: {printed}"
    );
}

#[test]
fn public_columns_carry_no_secrets() {
    for withheld in WITHHELD_FROM_INTROSPECTION {
        assert!(
            !SystemHefFilesRow::COLUMN_NAMES.contains(&withheld),
            "system.hef_files must not expose `{withheld}`"
        );
    }
}
