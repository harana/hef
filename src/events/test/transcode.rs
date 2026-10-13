use super::super::constant::RKYV_MAX_SUBTREE_DEPTH;
use super::super::envelope::TimestampValue;
use super::super::variant::{KeyDictionary, MAX_DEPTH, VariantValue, encode_value};
use super::{SourceFormat, transcode};
use crate::error::FormatError;
use prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MapKey, MessageDescriptor, Value};
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    MessageOptions,
};
use std::collections::{BTreeMap, HashMap};

#[test]
fn json_null() {
    assert_eq!(transcode(SourceFormat::Json, b"null").unwrap(), VariantValue::Null);
}

#[test]
fn json_bool_true() {
    assert_eq!(
        transcode(SourceFormat::Json, b"true").unwrap(),
        VariantValue::Bool(true)
    );
}

#[test]
fn json_bool_false() {
    assert_eq!(
        transcode(SourceFormat::Json, b"false").unwrap(),
        VariantValue::Bool(false)
    );
}

#[test]
fn json_integer() {
    assert_eq!(transcode(SourceFormat::Json, b"42").unwrap(), VariantValue::Int(42));
}

#[test]
fn json_negative_integer() {
    assert_eq!(transcode(SourceFormat::Json, b"-7").unwrap(), VariantValue::Int(-7));
}

#[test]
fn json_large_u64_becomes_decimal() {
    // Values beyond i64::MAX cannot fit in VariantValue::Int, so they become a scale-0 decimal preserving the exact
    // value.
    let large = u64::MAX;
    let input = large.to_string();
    let result = transcode(SourceFormat::Json, input.as_bytes()).unwrap();
    assert_eq!(
        result,
        VariantValue::Decimal {
            unscaled: i128::from(large),
            scale: 0,
        }
    );
}

#[test]
fn json_float() {
    let result = transcode(SourceFormat::Json, b"1.5").unwrap();
    assert!(matches!(result, VariantValue::Double(v) if (v - 1.5_f64).abs() < f64::EPSILON));
}

#[test]
fn json_string() {
    assert_eq!(
        transcode(SourceFormat::Json, b"\"hello\"").unwrap(),
        VariantValue::String("hello".into()),
    );
}

#[test]
fn json_array() {
    let result = transcode(SourceFormat::Json, b"[1, \"two\", null]").unwrap();
    assert_eq!(
        result,
        VariantValue::Array(vec![
            VariantValue::Int(1),
            VariantValue::String("two".into()),
            VariantValue::Null
        ])
    );
}

#[test]
fn json_object() {
    let result = transcode(SourceFormat::Json, b"{\"a\": 1, \"b\": true}").unwrap();
    let mut expected = BTreeMap::new();
    expected.insert("a".into(), VariantValue::Int(1));
    expected.insert("b".into(), VariantValue::Bool(true));
    assert_eq!(result, VariantValue::Object(expected));
}

#[test]
fn json_nested() {
    let input = br#"{"user": {"id": 99, "active": false}, "tags": ["x", "y"]}"#;
    let result = transcode(SourceFormat::Json, input).unwrap();
    let mut user = BTreeMap::new();
    user.insert("active".into(), VariantValue::Bool(false));
    user.insert("id".into(), VariantValue::Int(99));
    let mut expected = BTreeMap::new();
    expected.insert(
        "tags".into(),
        VariantValue::Array(vec![VariantValue::String("x".into()), VariantValue::String("y".into())]),
    );
    expected.insert("user".into(), VariantValue::Object(user));
    assert_eq!(result, VariantValue::Object(expected));
}

#[test]
fn json_invalid_returns_error() {
    assert!(transcode(SourceFormat::Json, b"{not valid json}").is_err());
}

#[test]
fn deeply_nested_json_is_rejected_before_any_recursion() {
    // Without the depth bound this ~200 KB body drives one stack frame per level through the serde bridge and the
    // conversion, aborting the process; it must come back as an ordinary error instead.
    let mut body = vec![b'['; 100_000];
    body.push(b'1');
    body.extend(std::iter::repeat_n(b']', 100_000));
    assert!(transcode(SourceFormat::Json, &body).is_err());
}

#[test]
fn json_nesting_boundary_matches_the_variant_depth_limit() {
    // A scalar under MAX_DEPTH containers is the deepest value the variant decoder accepts: it must transcode and
    // encode. One container more must be rejected.
    let mut body = vec![b'['; MAX_DEPTH];
    body.push(b'1');
    body.extend(std::iter::repeat_n(b']', MAX_DEPTH));
    let value = transcode(SourceFormat::Json, &body).unwrap();
    assert!(encode_value(&value, &KeyDictionary::default()).is_ok());

    let mut too_deep = vec![b'['; MAX_DEPTH + 1];
    too_deep.push(b'1');
    too_deep.extend(std::iter::repeat_n(b']', MAX_DEPTH + 1));
    assert!(transcode(SourceFormat::Json, &too_deep).is_err());
}

#[test]
fn brackets_inside_json_strings_do_not_count_toward_depth() {
    let mut body = b"{\"a\": \"".to_vec();
    body.extend(std::iter::repeat_n(b'[', 500));
    body.extend_from_slice(b"\\\"");
    body.extend(std::iter::repeat_n(b'{', 500));
    body.extend_from_slice(b"\"}");
    let mut expected = BTreeMap::new();
    let mut text = String::new();
    text.extend(std::iter::repeat_n('[', 500));
    text.push('"');
    text.extend(std::iter::repeat_n('{', 500));
    expected.insert("a".into(), VariantValue::String(text));
    assert_eq!(
        transcode(SourceFormat::Json, &body).unwrap(),
        VariantValue::Object(expected)
    );
}

#[test]
fn raw_bytes_stored_as_binary() {
    let body = b"\x00\x01\x02\xff";
    assert_eq!(
        transcode(SourceFormat::RawBytes, body).unwrap(),
        VariantValue::Binary(body.to_vec()),
    );
}

#[test]
fn json_original_buffer_unchanged() {
    // transcode must not modify the caller's slice (simd-json operates on an internal copy, not the original).
    let original = b"{\"k\": 1}";
    let body: Vec<u8> = original.to_vec();
    let _ = transcode(SourceFormat::Json, &body).unwrap();
    assert_eq!(&body[..], original);
}

/// Builds the test schema: an `Event` message with one field of each kind the converter maps, plus a nested message,
/// a map, an enum, and a `google.protobuf.Timestamp`.
fn protobuf_event_descriptor() -> MessageDescriptor {
    fn field(name: &str, number: i32, kind: Type, label: Label, type_name: Option<&str>) -> FieldDescriptorProto {
        FieldDescriptorProto {
            label: Some(label as i32),
            name: Some(name.to_owned()),
            number: Some(number),
            r#type: Some(kind as i32),
            type_name: type_name.map(str::to_owned),
            ..Default::default()
        }
    }
    let one = Label::Optional;
    let file = FileDescriptorProto {
        dependency: vec!["google/protobuf/timestamp.proto".to_owned()],
        enum_type: vec![EnumDescriptorProto {
            name: Some("Kind".to_owned()),
            value: vec![
                EnumValueDescriptorProto {
                    name: Some("KIND_UNSPECIFIED".to_owned()),
                    number: Some(0),
                    ..Default::default()
                },
                EnumValueDescriptorProto {
                    name: Some("KIND_WON".to_owned()),
                    number: Some(1),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        message_type: vec![
            DescriptorProto {
                name: Some("Inner".to_owned()),
                field: vec![
                    field("child", 1, Type::Message, one, Some(".test.Inner")),
                    field("note", 2, Type::String, one, None),
                ],
                ..Default::default()
            },
            DescriptorProto {
                field: vec![
                    field("amount", 1, Type::Int64, one, None),
                    field("big", 2, Type::Uint64, one, None),
                    field("blob", 3, Type::Bytes, one, None),
                    field("flag", 4, Type::Bool, one, None),
                    field("inner", 5, Type::Message, one, Some(".test.Inner")),
                    field("kind", 6, Type::Enum, one, Some(".test.Kind")),
                    field(
                        "labels",
                        7,
                        Type::Message,
                        Label::Repeated,
                        Some(".test.Event.LabelsEntry"),
                    ),
                    field("ratio", 8, Type::Double, one, None),
                    field("scores", 9, Type::Sint32, Label::Repeated, None),
                    field("seen_at", 10, Type::Message, one, Some(".google.protobuf.Timestamp")),
                    field("small", 11, Type::Float, one, None),
                ],
                name: Some("Event".to_owned()),
                nested_type: vec![DescriptorProto {
                    field: vec![
                        field("key", 1, Type::String, one, None),
                        field("value", 2, Type::Enum, one, Some(".test.Kind")),
                    ],
                    name: Some("LabelsEntry".to_owned()),
                    options: Some(MessageOptions {
                        map_entry: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        name: Some("test/event.proto".to_owned()),
        package: Some("test".to_owned()),
        syntax: Some("proto3".to_owned()),
        ..Default::default()
    };
    let mut pool = DescriptorPool::global();
    pool.add_file_descriptor_proto(file).unwrap();
    pool.get_message_by_name("test.Event").unwrap()
}

fn protobuf_body(descriptor: &MessageDescriptor, fill: impl FnOnce(&mut DynamicMessage)) -> Vec<u8> {
    let mut message = DynamicMessage::new(descriptor.clone());
    fill(&mut message);
    message.encode_to_vec()
}

#[test]
fn protobuf_maps_every_field_kind() {
    let descriptor = protobuf_event_descriptor();
    let pool = descriptor.parent_pool().clone();
    let body = protobuf_body(&descriptor, |message| {
        message.set_field_by_name("amount", Value::I64(-12));
        message.set_field_by_name("big", Value::U64(u64::MAX));
        message.set_field_by_name("blob", Value::Bytes(vec![0, 255].into()));
        message.set_field_by_name("flag", Value::Bool(true));
        let mut inner = DynamicMessage::new(pool.get_message_by_name("test.Inner").unwrap());
        inner.set_field_by_name("note", Value::String("hi".to_owned()));
        message.set_field_by_name("inner", Value::Message(inner));
        message.set_field_by_name("kind", Value::EnumNumber(1));
        message.set_field_by_name(
            "labels",
            Value::Map(HashMap::from([
                (MapKey::String("a".to_owned()), Value::EnumNumber(1)),
                (MapKey::String("b".to_owned()), Value::EnumNumber(7)),
            ])),
        );
        message.set_field_by_name("ratio", Value::F64(0.25));
        message.set_field_by_name("scores", Value::List(vec![Value::I32(-1), Value::I32(2)]));
        let mut seen_at = DynamicMessage::new(pool.get_message_by_name("google.protobuf.Timestamp").unwrap());
        seen_at.set_field_by_name("seconds", Value::I64(2));
        seen_at.set_field_by_name("nanos", Value::I32(5));
        message.set_field_by_name("seen_at", Value::Message(seen_at));
        message.set_field_by_name("small", Value::F32(1.5));
    });
    let expected = VariantValue::Object(BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(-12)),
        (
            "big".to_owned(),
            VariantValue::Decimal {
                unscaled: i128::from(u64::MAX),
                scale: 0,
            },
        ),
        ("blob".to_owned(), VariantValue::Binary(vec![0, 255])),
        ("flag".to_owned(), VariantValue::Bool(true)),
        (
            "inner".to_owned(),
            VariantValue::Object(BTreeMap::from([(
                "note".to_owned(),
                VariantValue::String("hi".to_owned()),
            )])),
        ),
        ("kind".to_owned(), VariantValue::String("KIND_WON".to_owned())),
        (
            "labels".to_owned(),
            VariantValue::Object(BTreeMap::from([
                ("a".to_owned(), VariantValue::String("KIND_WON".to_owned())),
                ("b".to_owned(), VariantValue::Int(7)),
            ])),
        ),
        ("ratio".to_owned(), VariantValue::Double(0.25)),
        (
            "scores".to_owned(),
            VariantValue::Array(vec![VariantValue::Int(-1), VariantValue::Int(2)]),
        ),
        (
            "seen_at".to_owned(),
            VariantValue::Timestamp(TimestampValue::from_physical_nanos(2_000_000_005)),
        ),
        ("small".to_owned(), VariantValue::Float(1.5)),
    ]));
    assert_eq!(transcode(SourceFormat::Protobuf(&descriptor), &body).unwrap(), expected);
}

#[test]
fn protobuf_omits_unset_fields_and_drops_unknown_ones() {
    let descriptor = protobuf_event_descriptor();
    let mut body = protobuf_body(&descriptor, |message| {
        message.set_field_by_name("amount", Value::I64(3));
    });
    // Field 99, varint 1: not declared by the schema.
    body.extend_from_slice(&[0x98, 0x06, 0x01]);
    assert_eq!(
        transcode(SourceFormat::Protobuf(&descriptor), &body).unwrap(),
        VariantValue::Object(BTreeMap::from([("amount".to_owned(), VariantValue::Int(3))]))
    );
}

#[test]
fn protobuf_rejects_a_body_that_does_not_match_the_schema() {
    let descriptor = protobuf_event_descriptor();
    // Field 1 declared as a length-delimited string run that ends early.
    let error = transcode(SourceFormat::Protobuf(&descriptor), &[0x0a, 0x05, b'a']).unwrap_err();
    assert!(matches!(error, FormatError::Structural { .. }));
}

#[test]
fn protobuf_rejects_a_timestamp_outside_the_representable_range() {
    let descriptor = protobuf_event_descriptor();
    let pool = descriptor.parent_pool().clone();
    let body = protobuf_body(&descriptor, |message| {
        let mut seen_at = DynamicMessage::new(pool.get_message_by_name("google.protobuf.Timestamp").unwrap());
        seen_at.set_field_by_name("seconds", Value::I64(i64::MAX));
        message.set_field_by_name("seen_at", Value::Message(seen_at));
    });
    let error = transcode(SourceFormat::Protobuf(&descriptor), &body).unwrap_err();
    assert!(matches!(error, FormatError::Structural { .. }));
}

#[test]
fn protobuf_keeps_deeply_nested_messages() {
    let descriptor = protobuf_event_descriptor();
    let inner = descriptor.parent_pool().get_message_by_name("test.Inner").unwrap();
    let mut nested = DynamicMessage::new(inner.clone());
    for _ in 0..90 {
        let mut parent = DynamicMessage::new(inner.clone());
        parent.set_field_by_name("child", Value::Message(nested));
        nested = parent;
    }
    let body = protobuf_body(&descriptor, |message| {
        message.set_field_by_name("inner", Value::Message(nested));
    });
    let value = transcode(SourceFormat::Protobuf(&descriptor), &body).unwrap();
    let mut depth = 0;
    let mut cursor = &value;
    while let VariantValue::Object(fields) = cursor {
        depth += 1;
        match fields.get("inner").or_else(|| fields.get("child")) {
            Some(next) => cursor = next,
            None => break,
        }
    }
    assert_eq!(depth, 92);
}

fn nested_arrays(depth: usize) -> VariantValue {
    let mut value = VariantValue::Int(1);
    for _ in 0..depth {
        value = VariantValue::Array(vec![value]);
    }
    value
}

fn rkyv_bytes(value: &VariantValue) -> Vec<u8> {
    rkyv::to_bytes::<rkyv::rancor::Error>(value).unwrap().to_vec()
}

#[test]
fn rkyv_body_reads_back_every_value_kind() {
    let mut fields = BTreeMap::new();
    fields.insert(
        "array".to_owned(),
        VariantValue::Array(vec![VariantValue::Null, VariantValue::Bool(true)]),
    );
    fields.insert("binary".to_owned(), VariantValue::Binary(vec![0, 1, 0xff]));
    fields.insert(
        "decimal".to_owned(),
        VariantValue::Decimal {
            unscaled: i128::MIN,
            scale: 7,
        },
    );
    fields.insert("double".to_owned(), VariantValue::Double(-2.5));
    fields.insert("float".to_owned(), VariantValue::Float(1.25));
    fields.insert("int".to_owned(), VariantValue::Int(i64::MAX));
    fields.insert(
        "string".to_owned(),
        VariantValue::String("a string longer than the inline limit".to_owned()),
    );
    fields.insert(
        "timestamp".to_owned(),
        VariantValue::Timestamp(TimestampValue::from_physical_nanos(-1)),
    );
    fields.insert("uuid".to_owned(), VariantValue::Uuid(u128::MAX - 1));
    let value = VariantValue::Object(fields);
    let body = rkyv_bytes(&value);
    assert_eq!(transcode(SourceFormat::Rkyv, &body).unwrap(), value);

    // The caller's bytes need not be aligned: start them one byte into a buffer.
    let mut shifted = vec![0];
    shifted.extend_from_slice(&body);
    assert_eq!(transcode(SourceFormat::Rkyv, shifted.get(1..).unwrap()).unwrap(), value);
}

#[test]
fn rkyv_body_that_does_not_validate_is_rejected() {
    let error = transcode(SourceFormat::Rkyv, b"not an archive").unwrap_err();
    assert!(matches!(
        error,
        FormatError::Structural {
            rule: "declared-rkyv ingest body is not a valid archived variant value"
        }
    ));
    let mut body = rkyv_bytes(&VariantValue::String(
        "a string longer than the inline limit".to_owned(),
    ));
    let text = body.iter().position(|&byte| byte == b'a').unwrap();
    body[text] = 0xff;
    assert!(transcode(SourceFormat::Rkyv, &body).is_err());
}

#[test]
fn rkyv_nesting_boundary_matches_the_variant_depth_limit() {
    // Wide objects with long keys take several validation hops per level; MAX_DEPTH of them must still fit.
    let mut value = VariantValue::Int(1);
    for _ in 0..MAX_DEPTH {
        let mut fields = BTreeMap::new();
        for key in 0..200 {
            fields.insert(
                format!("a key longer than the inline limit {key:03}"),
                VariantValue::Null,
            );
        }
        fields.insert("next".to_owned(), value);
        value = VariantValue::Object(fields);
    }
    let accepted = transcode(SourceFormat::Rkyv, &rkyv_bytes(&value)).unwrap();
    let mut keys = std::collections::BTreeSet::new();
    accepted.collect_keys(&mut keys);
    let dictionary = KeyDictionary::build(keys.into_iter().map(str::to_owned));
    assert!(encode_value(&accepted, &dictionary).is_ok());

    let error = transcode(SourceFormat::Rkyv, &rkyv_bytes(&nested_arrays(MAX_DEPTH + 1))).unwrap_err();
    assert!(matches!(
        error,
        FormatError::Structural {
            rule: "declared-rkyv ingest body exceeds the maximum nesting depth"
        }
    ));
}

#[test]
fn deeply_nested_rkyv_body_is_rejected_without_exhausting_the_stack() {
    // Just under the validation bound: validated and read back on the test thread's stack, then refused for depth.
    let near_bound = rkyv_bytes(&nested_arrays(RKYV_MAX_SUBTREE_DEPTH - 2));
    assert!(transcode(SourceFormat::Rkyv, &near_bound).is_err());

    // Far past it: building the body needs a deep stack, refusing it does not.
    let hostile = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            let value = nested_arrays(20 * RKYV_MAX_SUBTREE_DEPTH);
            let body = rkyv_bytes(&value);
            drop(value);
            body
        })
        .unwrap()
        .join()
        .unwrap();
    let error = transcode(SourceFormat::Rkyv, &hostile).unwrap_err();
    assert!(matches!(
        error,
        FormatError::Structural {
            rule: "declared-rkyv ingest body is not a valid archived variant value"
        }
    ));
}
