use super::*;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;

/// The HEF file id every test request names.
const HELD_FILE: u128 = 0xE3B;

#[test]
fn request_is_tenant_scoped_and_content_addressed() {
    let tenant = TenantId::new_test_id(42);
    let request = CacheRangeRequest {
        accepts: TransportCapabilities::inline_only(),
        file_blake3: [7u8; 32],
        file_id: HELD_FILE,
        range: ObjectRange { length: 16, offset: 0 },
        tenant_id: tenant,
    };
    assert_eq!(request.tenant_id, tenant);
    assert_eq!(request.file_blake3, [7u8; 32]);
    assert_eq!(request.range, ObjectRange { length: 16, offset: 0 });
}

#[test]
fn transport_capabilities_default_to_the_inline_floor() {
    // The safe default advertises nothing beyond the always-available inline path, so a node that probes nothing can
    // still fill from peers.
    assert_eq!(TransportCapabilities::default(), TransportCapabilities::inline_only());
    let floor = TransportCapabilities::inline_only();
    assert!(!floor.compressed_inline && !floor.nvme_over_fabrics && !floor.rdma_read);
    let everything = TransportCapabilities::all();
    assert!(everything.compressed_inline && everything.nvme_over_fabrics && everything.rdma_read);
}

#[test]
fn inline_encoding_defaults_to_identity() {
    assert_eq!(InlineEncoding::default(), InlineEncoding::Identity);
}

#[test]
fn peer_ids_have_a_total_order() {
    assert!(PeerId(1) < PeerId(2));
    assert_eq!(PeerId(5), PeerId(5));
}

#[test]
fn response_records_how_the_bytes_were_moved() {
    let inline = CacheRangeResponse {
        bytes: vec![1, 2, 3],
        transport: FillTransport::Inline,
        tree: None,
    };
    let rdma = CacheRangeResponse {
        bytes: vec![1, 2, 3],
        transport: FillTransport::RdmaRead,
        tree: None,
    };
    // Same bytes whichever transport moved them; only the recorded path differs, and an unset transport defaults to the
    // inline path.
    assert_eq!(inline.bytes, rdma.bytes);
    assert_ne!(inline.transport, rdma.transport);
    assert_eq!(FillTransport::default(), FillTransport::Inline);
}
