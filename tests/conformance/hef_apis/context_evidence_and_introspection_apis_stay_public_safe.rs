//! Checks that the introspection system tables stay public-safe: an operator reading `system.hef_files` must be
//! authorized (tenant or admin), and the rows returned omit object-store credentials, local paths, payload bytes, and
//! embedding values.

use hef::events::TenantId;
use hef::introspection::*;
use hef::typed_id::TypedIdTestExt;

fn record_with_secrets(tenant: TenantId) -> HefFileRecord {
    HefFileRecord {
        created_at: 1,
        embedding_sample: vec![0.5; 8],
        file_seal: [9; 32],
        file_id: 0xA,
        file_size: 8_192,
        generation_id: 3,
        granule_count: 5,
        local_path: "/var/lib/harana/hef/0xA.hef".to_owned(),
        object_store_credential: "AKIA-super-secret".to_owned(),
        payload_bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
        row_count: 1_000,
        tenant_id: tenant,
    }
}

/// conformance: hef-apis/context-evidence-and-introspection-apis-stay-public-safe/system-table-withholds-secrets
#[test]
fn system_table_withholds_secrets() {
    let records = [record_with_secrets(TenantId::new_test_id(7))];

    // Authorization is enforced: a public caller cannot read the system table.
    assert_eq!(
        system_hef_files(IntrospectionCaller::Public, &records),
        Err(IntrospectionError::Unauthorized)
    );

    // An authorized operator gets rows whose public column set omits every secret — credentials, local paths, payload
    // bytes, embeddings.
    let rows = system_hef_files(IntrospectionCaller::Admin, &records).unwrap();
    assert_eq!(rows.len(), 1);
    for withheld in WITHHELD_FROM_INTROSPECTION {
        assert!(
            !SystemHefFilesRow::COLUMN_NAMES.contains(&withheld),
            "system.hef_files must not expose `{withheld}`"
        );
    }
}
