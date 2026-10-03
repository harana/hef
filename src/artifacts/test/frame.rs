use super::{FrameBuildInput, build_frame, decode_frame, precheck_header};
use crate::error::FormatError;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;

fn sample_input() -> FrameBuildInput {
    FrameBuildInput {
        committed_at_physical: 2,
        created_at_physical: 1,
        dictionary_generation_hint: 0,
        durable_batch_id: 0,
        epoch: 1,
        event_count: 1,
        first_sequence: 1,
        flags: 0,
        last_sequence: 1,
        schema_generation: 0,
        tenant_id: TenantId::new_test_id(7),
        writer_id: 0,
        writer_local_batch_id: 0,
    }
}

#[test]
fn crc64_nvme_variant_is_locked() {
    // The standard CRC-64/NVME check value for the "123456789" vector. This pins the exact variant so an accidental
    // swap to another CRC-64 (XZ, ECMA-182, GO-ISO, ...) — all of which would still type-check — is caught.
    assert_eq!(crc_fast::crc64_nvme(b"123456789"), 0xae8b_1486_0a79_9888);
}

#[test]
fn frame_round_trips_with_a_crc64_header() {
    let frame = build_frame(&sample_input(), b"hello").unwrap();
    let (header, payload) = decode_frame(&frame).unwrap();
    assert_eq!(payload, b"hello");
    // The header carries a real (non-zero) CRC-64 value, not the placeholder.
    assert_ne!(header.header_crc64, 0);
    // The stored CRC-64 is exactly the NVME checksum over the header with the crc field (8 bytes at offset 128) and the
    // BLAKE3 field zeroed.
    let mut scratch = frame[..192].to_vec();
    scratch[128..136].fill(0);
    scratch[136..168].fill(0);
    assert_eq!(crc_fast::crc64_nvme(&scratch), header.header_crc64);
}

#[test]
fn header_crc64_rejects_a_flipped_header_byte() {
    let mut frame = build_frame(&sample_input(), b"hello").unwrap();
    // Flip a byte inside first_sequence (offset 56); the fast precheck must reject it on the CRC alone, before the
    // authoritative BLAKE3 runs.
    frame[56] ^= 0xff;
    assert_eq!(precheck_header(&frame), Err(FormatError::HeaderCrcMismatch));
}
