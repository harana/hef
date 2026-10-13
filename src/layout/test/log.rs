use super::*;
use crate::benchmarks::{LogObjectMeasurement, log_object_gate_passes};
use crate::file::constant::TREE_TRAILER_MAGIC;
use crate::layout::constant::HEF_MAGIC;
use crate::layout::decode_header;
use crate::layout::reader::HefFile;
use std::time::Instant;

const PAYLOADS: [&[u8]; 4] = [b"first", b"", b"third record", b"4"];

fn sealed(payloads: &[&[u8]]) -> SealedLogObject {
    let mut writer = LogObjectWriter::new();
    for payload in payloads {
        writer.append(payload).unwrap();
    }
    writer.finish()
}

/// Reads every record, returning the payloads before the first error and that error, if any.
fn read_all(records: LogRecords<'_>) -> (Vec<Vec<u8>>, Option<LogReadError>) {
    let mut payloads = Vec::new();
    for record in records {
        match record {
            Ok(payload) => payloads.push(payload.to_vec()),
            Err(error) => return (payloads, Some(error)),
        }
    }
    (payloads, None)
}

fn owned(payloads: &[&[u8]]) -> Vec<Vec<u8>> {
    payloads.iter().map(|payload| payload.to_vec()).collect()
}

#[test]
fn records_round_trip_with_and_without_the_pinned_root() {
    let object = sealed(&PAYLOADS);
    assert_eq!(
        read_all(LogRecords::open(&object.bytes).unwrap()),
        (owned(&PAYLOADS), None)
    );
    let verified = LogRecords::open_verified(&object.bytes, &object.root).unwrap();
    assert_eq!(read_all(verified), (owned(&PAYLOADS), None));
    assert_eq!(object.root, *blake3::hash(&object.bytes).as_bytes());
}

#[test]
fn the_header_declares_the_log_profile_code_up_front() {
    let object = sealed(&[]);
    assert_eq!(&object.bytes[..4], &LOG_MAGIC);
    assert_eq!(object.bytes[4], LOG_FORMAT_VERSION);
    assert_eq!(object.bytes[5], Profile::Log.code());
    assert_eq!(&object.bytes[object.bytes.len() - 4..], &LOG_MAGIC);
}

#[test]
fn header_and_trailer_overhead_per_object_is_eighteen_bytes() {
    let empty = sealed(&[]);
    assert_eq!(empty.bytes.len(), 18);
    assert_eq!(empty.bytes.len(), LOG_HEADER_LEN + LOG_TRAILER_LEN);

    let object = sealed(&PAYLOADS);
    let record_bytes: usize = PAYLOADS
        .iter()
        .map(|payload| LOG_RECORD_FRAME_LEN + payload.len())
        .sum();
    let framing_bytes = (object.bytes.len() - record_bytes) as u64;
    assert_eq!(framing_bytes, 18);

    let mut records = LogRecords::open_verified(&object.bytes, &object.root).unwrap();
    records.next().unwrap().unwrap();
    let first_record_verified_bytes = records.verified_len();
    records.by_ref().for_each(|record| assert!(record.is_ok()));
    assert!(log_object_gate_passes(&LogObjectMeasurement {
        first_record_verified_bytes,
        framing_bytes,
        object_bytes: object.bytes.len() as u64,
        verified_bytes: records.verified_len(),
    }));
}

#[test]
fn every_truncation_reads_as_a_torn_tail_after_the_intact_records() {
    let object = sealed(&PAYLOADS);
    let mut record_ends = vec![LOG_HEADER_LEN];
    for payload in PAYLOADS {
        record_ends.push(record_ends.last().unwrap() + LOG_RECORD_FRAME_LEN + payload.len());
    }
    for cut in 0..object.bytes.len() {
        let torn = &object.bytes[..cut];
        let intact = record_ends.iter().filter(|end| **end <= cut).count().saturating_sub(1);
        let expected = LogReadError::TornTail {
            intact_len: if cut < LOG_HEADER_LEN {
                0
            } else {
                record_ends[intact] as u64
            },
            intact_records: intact as u64,
        };
        let (payloads, error) = match LogRecords::open(torn) {
            Ok(records) => read_all(records),
            Err(error) => (Vec::new(), Some(error)),
        };
        assert_eq!(payloads, owned(&PAYLOADS[..intact]), "cut at {cut}");
        assert_eq!(error, Some(expected), "cut at {cut}");
    }
}

#[test]
fn damage_before_the_end_is_corruption_not_a_torn_tail() {
    let object = sealed(&PAYLOADS);
    let second_payload_at = LOG_HEADER_LEN + LOG_RECORD_FRAME_LEN + PAYLOADS[0].len() + 4 + 1;
    let mut flipped = object.bytes.clone();
    flipped[LOG_HEADER_LEN + 4] ^= 1;
    let (payloads, error) = read_all(LogRecords::open(&flipped).unwrap());
    assert!(payloads.is_empty());
    assert!(matches!(
        error,
        Some(LogReadError::Corrupt(FormatError::Structural { .. }))
    ));

    let mut wrong_count = object.bytes.clone();
    let count_at = wrong_count.len() - 8;
    wrong_count[count_at] ^= 1;
    let (payloads, error) = read_all(LogRecords::open(&wrong_count).unwrap());
    assert_eq!(payloads, owned(&PAYLOADS));
    assert!(matches!(error, Some(LogReadError::Corrupt(_))));

    let mut trailing = object.bytes.clone();
    trailing.push(0);
    assert!(matches!(
        read_all(LogRecords::open(&trailing).unwrap()).1,
        Some(LogReadError::Corrupt(_))
    ));

    // With a pinned root every damaged or missing byte is corruption, even a truncation.
    for at in [0, second_payload_at, object.bytes.len() - 1] {
        let mut damaged = object.bytes.clone();
        damaged[at] ^= 0x40;
        let error = match LogRecords::open_verified(&damaged, &object.root) {
            Ok(records) => read_all(records).1,
            Err(error) => Some(error),
        };
        assert!(matches!(error, Some(LogReadError::Corrupt(_))), "byte {at}");
    }
    let truncated = &object.bytes[..object.bytes.len() - 1];
    let records = LogRecords::open_verified(truncated, &object.root).unwrap_err();
    assert!(matches!(
        records,
        LogReadError::Corrupt(FormatError::Blake3Mismatch { .. })
    ));
}

#[test]
fn a_log_object_and_a_hef_file_never_open_as_each_other() {
    let object = sealed(&PAYLOADS);
    // A log object at least as long as a HEF header block still fails on its first four bytes.
    let large = sealed(&[&[0u8; 4096]]);
    for bytes in [&object.bytes, &large.bytes] {
        assert!(decode_header(bytes).is_err());
        assert!(HefFile::open(bytes.clone(), None).is_err());
    }
    assert_eq!(
        decode_header(&large.bytes).unwrap_err(),
        FormatError::BadMagic { expected: "HEF1" }
    );

    let mut hef = HEF_MAGIC.to_vec();
    hef.resize(4096, 0);
    assert_eq!(
        LogRecords::open(&hef).unwrap_err(),
        LogReadError::Corrupt(FormatError::BadMagic { expected: "HEFL" })
    );

    let mut other_profile = object.bytes.clone();
    other_profile[5] = Profile::Event.code();
    assert!(matches!(
        LogRecords::open(&other_profile).unwrap_err(),
        LogReadError::Corrupt(FormatError::Structural { .. })
    ));
    let mut other_version = object.bytes.clone();
    other_version[4] = 2;
    assert!(matches!(
        LogRecords::open(&other_version).unwrap_err(),
        LogReadError::Corrupt(FormatError::UnsupportedVersion { .. })
    ));
}

#[test]
fn a_large_object_streams_one_chunk_group_at_a_time_through_its_proof_tree() {
    let payload = vec![7u8; 64 * 1024];
    let payloads: Vec<&[u8]> = (0..40).map(|_| payload.as_slice()).collect();
    let object = sealed(&payloads);
    assert_eq!(&object.bytes[object.bytes.len() - 4..], &TREE_TRAILER_MAGIC);
    let content_len = LOG_HEADER_LEN + 40 * (LOG_RECORD_FRAME_LEN + payload.len()) + LOG_TRAILER_LEN;
    assert!(content_len > 2 * CHUNK_GROUP_BYTES);

    let mut records = LogRecords::open_verified(&object.bytes, &object.root).unwrap();
    assert_eq!(records.next().unwrap().unwrap(), payload.as_slice());
    let first_record_verified_bytes = records.verified_len();
    assert_eq!(first_record_verified_bytes, CHUNK_GROUP_BYTES as u64);
    assert_eq!(records.by_ref().filter(|record| record.is_ok()).count(), 39);
    assert_eq!(records.verified_len(), content_len as u64);
    assert!(log_object_gate_passes(&LogObjectMeasurement {
        first_record_verified_bytes,
        framing_bytes: (LOG_HEADER_LEN + LOG_TRAILER_LEN) as u64,
        object_bytes: object.bytes.len() as u64,
        verified_bytes: records.verified_len(),
    }));
    assert_eq!(read_all(LogRecords::open(&object.bytes).unwrap()).0.len(), 40);

    // Damage in the last chunk group: the records before it are still served, then the read fails as corruption.
    let mut damaged = object.bytes.clone();
    damaged[content_len - LOG_TRAILER_LEN - 100] ^= 1;
    let (payloads, error) = read_all(LogRecords::open_verified(&damaged, &object.root).unwrap());
    assert!(payloads.len() >= 16 && payloads.len() < 40);
    assert!(matches!(error, Some(LogReadError::Corrupt(_))));
}

/// Writes `objects` log objects of `records` copies of `payload`, then reads each back verified against its root,
/// seven times, and prints the median write (append and finish) and read throughput. Returns one of the objects.
fn report_log_throughput(label: &str, payload: &[u8], records: usize, objects: usize) -> SealedLogObject {
    let mut write_samples = Vec::new();
    let mut read_samples = Vec::new();
    let mut sealed = Vec::new();
    for _ in 0..7 {
        sealed = Vec::with_capacity(objects);
        let started = Instant::now();
        for _ in 0..objects {
            let mut writer = LogObjectWriter::new();
            for _ in 0..records {
                writer.append(payload).unwrap();
            }
            sealed.push(writer.finish());
        }
        write_samples.push(started.elapsed().as_secs_f64());

        let started = Instant::now();
        let mut read = 0;
        for object in &sealed {
            for record in LogRecords::open_verified(&object.bytes, &object.root).unwrap() {
                std::hint::black_box(record.unwrap());
                read += 1;
            }
        }
        read_samples.push(started.elapsed().as_secs_f64());
        assert_eq!(read, records * objects);
    }
    write_samples.sort_unstable_by(f64::total_cmp);
    read_samples.sort_unstable_by(f64::total_cmp);
    let total_records = (records * objects) as f64;
    let total_bytes = sealed.iter().map(|object| object.bytes.len()).sum::<usize>() as f64;
    for (phase, median) in [("write", write_samples[3]), ("verified read", read_samples[3])] {
        println!(
            "log {label}, {phase}: median {:.1} ms, {:.0} records/s, {:.1} MB/s",
            median * 1e3,
            total_records / median,
            total_bytes / median / 1e6
        );
    }
    sealed.swap_remove(0)
}

/// Times writing and verified reading of log objects in two shapes - 20,000 KiB-range objects of 64 records of 64
/// bytes, and one 64 MiB object of 4 KiB records read through its proof tree - and prints records and megabytes per
/// second for each, and the bytes the large read verifies before its first record. Run by hand:
/// `cargo test --release --all-features --lib log_object_throughput -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode log object benchmark"]
fn log_object_throughput() {
    report_log_throughput("64 x 64 B objects", &[0x5A; 64], 64, 20_000);
    let large = report_log_throughput("64 MiB object of 4 KiB records", &[0xA5; 4096], 16_384, 1);

    let mut records = LogRecords::open_verified(&large.bytes, &large.root).unwrap();
    records.next().unwrap().unwrap();
    let first_record_verified_bytes = records.verified_len();
    records.by_ref().for_each(|record| assert!(record.is_ok()));
    let measurement = LogObjectMeasurement {
        first_record_verified_bytes,
        framing_bytes: (LOG_HEADER_LEN + LOG_TRAILER_LEN) as u64,
        object_bytes: large.bytes.len() as u64,
        verified_bytes: records.verified_len(),
    };
    println!("log 64 MiB object: {measurement:?}");
    assert!(log_object_gate_passes(&measurement));
}
