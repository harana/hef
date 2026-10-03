use super::super::variant::{KeyDictionary, MAX_DEPTH, VariantValue, encode_value};
use super::{SourceFormat, transcode};
use std::collections::BTreeMap;

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
