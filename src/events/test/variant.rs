use super::*;

fn dict_for(value: &VariantValue) -> KeyDictionary {
    let mut keys = std::collections::BTreeSet::new();
    value.collect_keys(&mut keys);
    KeyDictionary::build(keys.into_iter().map(str::to_owned))
}

fn sample_object() -> VariantValue {
    let mut inner = BTreeMap::new();
    inner.insert("city".to_owned(), VariantValue::String("Oslo".to_owned()));
    inner.insert("zip".to_owned(), VariantValue::Int(1234));
    let mut fields = BTreeMap::new();
    fields.insert("address".to_owned(), VariantValue::Object(inner));
    fields.insert(
        "amount".to_owned(),
        VariantValue::Decimal {
            unscaled: 123_456_789_012_345,
            scale: 2,
        },
    );
    fields.insert("active".to_owned(), VariantValue::Bool(true));
    fields.insert("count".to_owned(), VariantValue::Int(70_000));
    fields.insert(
        "tags".to_owned(),
        VariantValue::Array(vec![
            VariantValue::String("a".to_owned()),
            VariantValue::Int(-3),
            VariantValue::Null,
        ]),
    );
    fields.insert("long_text".to_owned(), VariantValue::String("x".repeat(100)));
    fields.insert("ratio".to_owned(), VariantValue::Double(0.25));
    VariantValue::Object(fields)
}

#[test]
fn transcoding_into_a_wider_dictionary_matches_a_direct_encode() {
    // A payload moving into a home governed by a wider dictionary is rewritten straight from its bytes. The result
    // must equal what encoding the value against that dictionary produces, including the id and offset table widths
    // the wider ids force.
    let value = sample_object();
    let narrow = dict_for(&value);
    // A dictionary wide enough to push field ids past a single byte, so the object tables must widen.
    let wide = KeyDictionary::build(
        narrow
            .keys()
            .map(str::to_owned)
            .chain((0..400).map(|i| format!("aa_filler_{i:04}"))),
    );

    let encoded = encode_value(&value, &narrow).unwrap();
    let mut transcoded = Writer::new();
    transcode_value_into(VariantRef::new(&encoded), &narrow, &wide, &mut transcoded).unwrap();
    assert_eq!(transcoded.into_bytes(), encode_value(&value, &wide).unwrap());
}

#[test]
fn transcoding_refuses_a_key_the_target_dictionary_does_not_hold() {
    let value = sample_object();
    let narrow = dict_for(&value);
    let encoded = encode_value(&value, &narrow).unwrap();
    let mut out = Writer::new();
    assert!(
        transcode_value_into(
            VariantRef::new(&encoded),
            &narrow,
            &KeyDictionary::build(["unrelated".to_owned()]),
            &mut out
        )
        .is_err()
    );
}

#[test]
fn round_trip_preserves_value() {
    let value = sample_object();
    let dictionary = dict_for(&value);
    let bytes = encode_value(&value, &dictionary).unwrap();
    let decoded = VariantRef::new(&bytes).decode(&dictionary).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn one_field_objects_round_trip_every_scalar_width() {
    let scalars = [
        VariantValue::Null,
        VariantValue::Bool(true),
        VariantValue::Int(i8::MAX as i64),
        VariantValue::Int(i16::MAX as i64),
        VariantValue::Int(i32::MAX as i64),
        VariantValue::Int(i64::MAX),
        VariantValue::Float(0.5),
        VariantValue::Double(0.25),
        VariantValue::Decimal {
            unscaled: i32::MAX as i128,
            scale: 2,
        },
        VariantValue::Decimal {
            unscaled: i64::MAX as i128,
            scale: 2,
        },
        VariantValue::Decimal {
            unscaled: i128::MAX,
            scale: 2,
        },
        VariantValue::String("short".to_owned()),
        VariantValue::String("long".repeat(32)),
        VariantValue::Binary(vec![1, 2, 3]),
        VariantValue::Timestamp(TimestampValue::from_physical_nanos(123)),
        VariantValue::Uuid(u128::MAX),
    ];
    for scalar in scalars {
        let value = VariantValue::Object(BTreeMap::from([("value".to_owned(), scalar)]));
        let dictionary = dict_for(&value);
        let bytes = encode_value(&value, &dictionary).unwrap();
        VariantRef::new(&bytes).validate(&dictionary).unwrap();
        assert_eq!(VariantRef::new(&bytes).decode(&dictionary).unwrap(), value);
    }
}

#[test]
fn small_multi_field_objects_directly_encode_every_scalar_width() {
    let scalars = [
        VariantValue::Null,
        VariantValue::Bool(false),
        VariantValue::Int(i8::MIN as i64),
        VariantValue::Int(i16::MIN as i64),
        VariantValue::Int(i32::MIN as i64),
        VariantValue::Int(i64::MIN),
        VariantValue::Float(-0.5),
        VariantValue::Double(-0.25),
        VariantValue::Decimal {
            unscaled: i32::MIN as i128,
            scale: 2,
        },
        VariantValue::Decimal {
            unscaled: i64::MIN as i128,
            scale: 2,
        },
        VariantValue::Decimal {
            unscaled: i128::MIN,
            scale: 2,
        },
        VariantValue::String("x".repeat(63)),
        VariantValue::String("x".repeat(64)),
        VariantValue::Binary(vec![1, 2, 3]),
        VariantValue::Timestamp(TimestampValue::from_physical_nanos(-123)),
        VariantValue::Uuid(u128::MAX),
    ];

    for scalar in scalars {
        let value = VariantValue::Object(BTreeMap::from([
            ("first".to_owned(), VariantValue::Int(7)),
            ("value".to_owned(), scalar),
        ]));
        let dictionary = dict_for(&value);
        let VariantValue::Object(fields) = &value else {
            unreachable!();
        };
        let iter = fields.iter().map(|(key, value)| (key.as_str(), value));

        let mut direct = Writer::new();
        encode_object_into(
            iter.clone(),
            fields.len(),
            &dictionary,
            &mut EncodeScratch::new(),
            &mut direct,
            0,
        )
        .unwrap();
        let mut buffered = Writer::new();
        encode_object_buffered(
            iter,
            fields.len(),
            &dictionary,
            &mut EncodeScratch::new(),
            &mut buffered,
            0,
        )
        .unwrap();

        assert_eq!(direct.bytes(), buffered.bytes(), "encoding differs for {value:?}");
        VariantRef::new(direct.bytes()).validate(&dictionary).unwrap();
        assert_eq!(VariantRef::new(direct.bytes()).decode(&dictionary).unwrap(), value);
    }
}

#[test]
fn direct_object_header_matches_buffered_at_every_id_and_offset_width() {
    for max_id in [0xFF, 0x100, 0xFFFF, 0x1_0000, 0xFF_FFFF, 0x100_0000] {
        for data_len in [0xFF, 0x100, 0xFFFF, 0x1_0000, 0xFF_FFFF, 0x100_0000] {
            let placed = [(0, 0), (max_id, data_len as u32)];
            let mut direct = Writer::new();
            write_object_header(&placed, data_len, &mut direct);

            let mut body = Writer::with_capacity(data_len);
            body.reserve_bytes(data_len);
            let mut buffered = Writer::new();
            write_object(&placed, &body, &mut buffered);

            assert_eq!(direct.bytes(), &buffered.bytes()[..direct.len()]);
        }
    }
}

#[test]
fn nested_and_wide_objects_fall_back_to_the_buffered_encoding() {
    let nested = VariantValue::Object(BTreeMap::from([("inner".to_owned(), VariantValue::Int(1))]));
    let values = [
        VariantValue::Object(BTreeMap::from([
            ("a".to_owned(), VariantValue::Int(1)),
            ("b".to_owned(), nested),
        ])),
        VariantValue::Object(
            (0..=DIRECT_SCALAR_OBJECT_MAX_FIELDS)
                .map(|index| (format!("field_{index}"), VariantValue::Int(index as i64)))
                .collect(),
        ),
    ];
    for value in values {
        let dictionary = dict_for(&value);
        let VariantValue::Object(fields) = &value else {
            unreachable!();
        };
        let iter = fields.iter().map(|(key, value)| (key.as_str(), value));
        let mut encoded = Writer::new();
        encode_object_into(
            iter.clone(),
            fields.len(),
            &dictionary,
            &mut EncodeScratch::new(),
            &mut encoded,
            0,
        )
        .unwrap();
        let mut buffered = Writer::new();
        encode_object_buffered(
            iter,
            fields.len(),
            &dictionary,
            &mut EncodeScratch::new(),
            &mut buffered,
            0,
        )
        .unwrap();
        assert_eq!(encoded.bytes(), buffered.bytes());
    }
}

#[test]
fn direct_object_preflight_leaves_destination_untouched_on_missing_key() {
    let fields = BTreeMap::from([
        ("a".to_owned(), VariantValue::Int(1)),
        ("missing".to_owned(), VariantValue::Int(2)),
    ]);
    let dictionary = KeyDictionary::build(["a".to_owned()]);
    let mut out = Writer::new();
    out.put_u8(0xA5);
    let result = encode_object_into(
        fields.iter().map(|(key, value)| (key.as_str(), value)),
        fields.len(),
        &dictionary,
        &mut EncodeScratch::new(),
        &mut out,
        0,
    );
    assert!(result.is_err());
    assert_eq!(out.bytes(), &[0xA5]);
}

#[test]
#[ignore = "release-mode IMP-005 residual-encoding benchmark"]
fn benchmark_direct_small_objects_against_buffered_encoding() {
    for string_len in [8, 32, 64, 128, 256, 1024] {
        let fields = BTreeMap::from([
            ("active".to_owned(), VariantValue::Bool(true)),
            ("count".to_owned(), VariantValue::Int(70_000)),
            ("label".to_owned(), VariantValue::String("x".repeat(string_len))),
            ("source".to_owned(), VariantValue::String("y".repeat(string_len))),
        ]);
        let value = VariantValue::Object(fields.clone());
        let dictionary = dict_for(&value);
        let mut direct_samples = Vec::new();
        let mut buffered_samples = Vec::new();

        for iteration in 0..7 {
            let run_direct = || {
                let mut scratch = EncodeScratch::new();
                let mut out = Writer::with_capacity(string_len * 2 + 128);
                let started = std::time::Instant::now();
                for _ in 0..100_000 {
                    out.clear();
                    encode_object_into(
                        fields.iter().map(|(key, value)| (key.as_str(), value)),
                        fields.len(),
                        &dictionary,
                        &mut scratch,
                        &mut out,
                        0,
                    )
                    .unwrap();
                    std::hint::black_box(out.bytes());
                }
                started.elapsed().as_nanos()
            };
            let run_buffered = || {
                let mut scratch = EncodeScratch::new();
                let mut out = Writer::with_capacity(string_len * 2 + 128);
                let started = std::time::Instant::now();
                for _ in 0..100_000 {
                    out.clear();
                    encode_object_buffered(
                        fields.iter().map(|(key, value)| (key.as_str(), value)),
                        fields.len(),
                        &dictionary,
                        &mut scratch,
                        &mut out,
                        0,
                    )
                    .unwrap();
                    std::hint::black_box(out.bytes());
                }
                started.elapsed().as_nanos()
            };
            if iteration % 2 == 0 {
                direct_samples.push(run_direct());
                buffered_samples.push(run_buffered());
            } else {
                buffered_samples.push(run_buffered());
                direct_samples.push(run_direct());
            }
        }
        direct_samples.sort_unstable();
        buffered_samples.sort_unstable();
        let direct_median = direct_samples.get(3).copied().unwrap_or_default();
        let buffered_median = buffered_samples.get(3).copied().unwrap_or_default();
        eprintln!(
            "IMP-005 body={}B: median buffered={}ns direct={}ns reduction={:.1}%",
            scalar_encoded_len(fields.get("active").unwrap()).unwrap()
                + scalar_encoded_len(fields.get("count").unwrap()).unwrap()
                + scalar_encoded_len(fields.get("label").unwrap()).unwrap()
                + scalar_encoded_len(fields.get("source").unwrap()).unwrap(),
            buffered_median,
            direct_median,
            100.0 * (1.0 - direct_median as f64 / buffered_median as f64)
        );
    }
}

#[test]
fn single_path_extraction_without_sibling_decode() {
    let value = sample_object();
    let dictionary = dict_for(&value);
    let bytes = encode_value(&value, &dictionary).unwrap();
    let target = VariantRef::new(&bytes)
        .get_path(
            &dictionary,
            &[PathSegment::Field("address"), PathSegment::Field("city")],
        )
        .unwrap()
        .expect("path present");
    assert_eq!(
        target.decode(&dictionary).unwrap(),
        VariantValue::String("Oslo".to_owned())
    );
    let absent = VariantRef::new(&bytes)
        .get_path(&dictionary, &[PathSegment::Field("missing")])
        .unwrap();
    assert!(absent.is_none());
    let array_item = VariantRef::new(&bytes)
        .get_path(&dictionary, &[PathSegment::Field("tags"), PathSegment::Index(1)])
        .unwrap()
        .expect("index present");
    assert_eq!(array_item.decode(&dictionary).unwrap(), VariantValue::Int(-3));
}

#[test]
fn out_of_range_field_id_refuses() {
    let value = sample_object();
    let dictionary = dict_for(&value);
    let bytes = encode_value(&value, &dictionary).unwrap();
    // Validate against a smaller dictionary: ids escape it.
    let small = KeyDictionary::build(["address".to_owned()]);
    assert!(VariantRef::new(&bytes).validate(&small).is_err());
}

#[test]
fn unsorted_field_ids_refuse_on_point_lookup() {
    // Two fields with ids [0, 1]; the point-lookup fast path must reject an object whose ids are not strictly ascending
    // rather than binary-searching past the corruption and reporting the field absent.
    let mut fields = BTreeMap::new();
    fields.insert("a".to_owned(), VariantValue::Int(1));
    fields.insert("b".to_owned(), VariantValue::Int(2));
    let value = VariantValue::Object(fields);
    let dictionary = dict_for(&value);
    let mut bytes = encode_value(&value, &dictionary).unwrap();
    // Layout for a small object: [metadata, count=2, id0, id1, ...]; swap the two ids to make them descending.
    bytes.swap(2, 3);
    let corrupt = VariantRef::new(&bytes);
    let via_path = corrupt.get_path(&dictionary, &[PathSegment::Field("a")]);
    assert!(
        matches!(
            via_path,
            Err(FormatError::Structural {
                rule: "object field ids must be strictly ascending"
            })
        ),
        "unsorted ids must refuse, got {via_path:?}"
    );
    // The full decode already rejects this; the fast path now matches it.
    assert!(corrupt.decode(&dictionary).is_err());
}

#[test]
fn every_field_of_a_wide_object_resolves_by_binary_search() {
    // The point-lookup path checks each probed id against its neighbours rather than walking the whole id table, so a
    // wide object exercises probes at the start, the middle and the end of the table. Every field must still be found.
    let mut fields = BTreeMap::new();
    for index in 0..64u32 {
        fields.insert(format!("field_{index:03}"), VariantValue::Int(i64::from(index)));
    }
    let value = VariantValue::Object(fields);
    let dictionary = dict_for(&value);
    let bytes = encode_value(&value, &dictionary).unwrap();
    let object = VariantRef::new(&bytes);
    for index in 0..64u32 {
        let key = format!("field_{index:03}");
        let found = object
            .get_path(&dictionary, &[PathSegment::Field(&key)])
            .unwrap()
            .unwrap_or_else(|| panic!("{key} present"));
        assert_eq!(found.decode(&dictionary).unwrap(), VariantValue::Int(i64::from(index)));
    }
    assert!(
        object
            .get_path(&dictionary, &[PathSegment::Field("field_064")])
            .unwrap()
            .is_none()
    );
}

#[test]
fn encode_rejects_nesting_deeper_than_the_decoder_accepts() {
    let dictionary = KeyDictionary::default();
    // A scalar under MAX_DEPTH containers sits exactly at the decoder's limit and must round-trip.
    let mut at_limit = VariantValue::Int(1);
    for _ in 0..MAX_DEPTH {
        at_limit = VariantValue::Array(vec![at_limit]);
    }
    let bytes = encode_value(&at_limit, &dictionary).unwrap();
    assert!(VariantRef::new(&bytes).validate(&dictionary).is_ok());
    assert_eq!(VariantRef::new(&bytes).decode(&dictionary).unwrap(), at_limit);
    // One level deeper: encode must fail instead of producing bytes the decoder rejects.
    let over_limit = VariantValue::Array(vec![at_limit]);
    assert!(encode_value(&over_limit, &dictionary).is_err());
}

#[test]
fn trailing_bytes_after_short_strings_and_containers_fail_validation() {
    // The primitive path already rejects trailing bytes; short strings and containers must too, or a forged payload
    // could hide data after the encoded value.
    let mut fields = BTreeMap::new();
    fields.insert("k".to_owned(), VariantValue::Int(1));
    let values = [
        VariantValue::String("hi".to_owned()),
        VariantValue::Array(vec![VariantValue::Int(1)]),
        VariantValue::Object(fields),
    ];
    for value in values {
        let dictionary = dict_for(&value);
        let mut bytes = encode_value(&value, &dictionary).unwrap();
        assert!(VariantRef::new(&bytes).validate(&dictionary).is_ok());
        bytes.push(0xAB);
        assert!(
            VariantRef::new(&bytes).validate(&dictionary).is_err(),
            "trailing garbage after {value:?} must not validate"
        );
    }
}

#[test]
fn decode_rejects_trailing_bytes_after_short_strings_and_containers() {
    // decode is an untrusted-bytes path too: on-disk residual payloads are decoded without a prior validate, so it must
    // refuse on hidden tail bytes after a short string or container exactly like validate does.
    let mut fields = BTreeMap::new();
    fields.insert("k".to_owned(), VariantValue::Int(1));
    let values = [
        VariantValue::String("hi".to_owned()),
        VariantValue::Array(vec![VariantValue::Int(1)]),
        VariantValue::Object(fields),
    ];
    for value in values {
        let dictionary = dict_for(&value);
        let mut bytes = encode_value(&value, &dictionary).unwrap();
        assert!(VariantRef::new(&bytes).decode(&dictionary).is_ok());
        bytes.push(0xAB);
        assert!(
            VariantRef::new(&bytes).decode(&dictionary).is_err(),
            "trailing garbage after {value:?} must not decode"
        );
    }
}

#[test]
fn truncated_value_refuses() {
    let value = sample_object();
    let dictionary = dict_for(&value);
    let bytes = encode_value(&value, &dictionary).unwrap();
    for cut in [0usize, 1, 2, bytes.len() / 2, bytes.len() - 1] {
        let slice = &bytes[..cut];
        assert!(
            VariantRef::new(slice).validate(&dictionary).is_err(),
            "cut at {cut} must not validate"
        );
    }
}

#[test]
fn borrowed_fields_encode_exactly_as_the_owned_object() {
    let value = sample_object();
    let dictionary = dict_for(&value);
    let VariantValue::Object(fields) = &value else {
        panic!("the sample is an object");
    };
    let borrowed: Vec<(&str, &VariantValue)> = fields.iter().map(|(key, value)| (key.as_str(), value)).collect();
    let mut out = Writer::new();
    encode_object_fields_into(&borrowed, &dictionary, &mut out).unwrap();
    assert_eq!(out.into_bytes(), encode_value(&value, &dictionary).unwrap());
}

#[test]
fn values_appended_to_one_arena_match_their_standalone_encodings() {
    let value = sample_object();
    let dictionary = dict_for(&value);
    let standalone = encode_value(&value, &dictionary).unwrap();
    let mut arena = Writer::new();
    for _ in 0..3 {
        encode_value_into(&value, &dictionary, &mut arena).unwrap();
    }
    assert_eq!(arena.into_bytes(), standalone.repeat(3));
}
