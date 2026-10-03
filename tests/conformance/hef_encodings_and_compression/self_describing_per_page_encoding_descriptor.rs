//! Checks that each encoded page exposes a compact descriptor — dictionary entries, base value, bit-width, ALP
//! exponent, run count — so a consumer can answer predicates from the encoded form and order predicate evaluation by
//! decode cost, without ever materialising a row's decoded value.

use hef::encoding::descriptor::{PageDescriptor, extract_descriptor};
use hef::encoding::predicate::{StringPredicate, filter_string_block};
use hef::encoding::{ColumnData, StringColumn, Transform, decode_block, encode_block};

fn status_values(count: usize) -> StringColumn {
    (0..count)
        .map(|i| match i % 4 {
            0 => None,
            1 => Some("lost"),
            2 => Some("pending"),
            _ => Some("won"),
        })
        .collect()
}

fn id_values(count: usize) -> StringColumn {
    (0..count)
        .map(|i| {
            if i % 13 == 0 {
                None
            } else {
                Some(format!("entity-{i:06}"))
            }
        })
        .collect()
}

/// conformance:
/// hef-encodings-and-compression/self-describing-per-page-encoding-descriptor/
/// predicate-evaluated-on-dictionary-codes-via-the-descriptor
#[test]
fn predicate_evaluated_on_dictionary_codes_via_the_descriptor() {
    // Enough rows that the dictionary form is smaller than the plain form. A much smaller page still pays for a whole
    // 1024-value vector of packed codes, loses the encoder's plain-form size check, and is stored plain instead.
    let values = status_values(200);
    let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
    assert_eq!(encoded.pipeline.transform().unwrap(), Transform::DictionaryString);

    // The descriptor surfaces the sorted dictionary entries without decoding any row's string value.
    let descriptor = extract_descriptor(encoded.pipeline, &encoded.bytes).unwrap();
    let PageDescriptor::Dictionary { ref entries } = descriptor else {
        panic!("expected Dictionary descriptor for low-cardinality strings");
    };

    // Translate 'won' into its code once by binary-searching the sorted dictionary.
    let won_code = entries
        .binary_search(&"won".to_owned())
        .expect("'won' must be present in the dictionary");

    // The dictionary is sorted, so code order mirrors value order: the consumer compares packed codes against won_code
    // without decoding any row.
    let lost_code = entries.binary_search(&"lost".to_owned()).unwrap();
    let pending_code = entries.binary_search(&"pending".to_owned()).unwrap();
    assert!(
        lost_code < pending_code && pending_code < won_code,
        "sorted dictionary must preserve value order so codes are comparable"
    );

    // The fast path answers the predicate from codes and must agree with a full decode row for row.
    let predicate = StringPredicate::Equals("won".to_owned());
    let mask = filter_string_block(encoded.pipeline, &encoded.bytes, &predicate)
        .unwrap()
        .expect("dictionary block must answer equality from codes");
    assert_eq!(mask, predicate.filter_decoded(&values));

    // Null rows never match.
    for (index, value) in values.iter().enumerate() {
        if value.is_none() {
            assert!(!mask[index], "null row {index} should not match");
        }
    }

    // Decoding must produce identical values whether or not the descriptor was read.
    assert_eq!(
        decode_block(encoded.pipeline, &encoded.bytes).unwrap(),
        ColumnData::Strings(values),
        "descriptor use must not change decoded output"
    );
}

/// conformance:
/// hef-encodings-and-compression/self-describing-per-page-encoding-descriptor/decode-cost-planning-from-the-descriptor
#[test]
fn decode_cost_planning_from_the_descriptor() {
    // A low-cardinality status column (dictionary-encoded) and a high-cardinality identifier column (FSST-encoded).
    // Their descriptors expose different cost profiles so a planner can order predicates cheapest-first.
    let status = status_values(200);
    let ids = id_values(200);

    let dict_block = encode_block(&ColumnData::Strings(status), true);
    let fsst_block = encode_block(&ColumnData::Strings(ids), true);

    assert_eq!(dict_block.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(fsst_block.pipeline.transform().unwrap(), Transform::FsstString);

    // Extract descriptors from the page headers.
    let dict_desc = extract_descriptor(dict_block.pipeline, &dict_block.bytes).unwrap();
    let fsst_desc = extract_descriptor(fsst_block.pipeline, &fsst_block.bytes).unwrap();

    // The dictionary column is cheaper to evaluate per row than the FSST column.
    assert!(
        dict_desc.decode_cost_per_row() < fsst_desc.decode_cost_per_row(),
        "dictionary cost ({}) must be cheaper than FSST cost ({})",
        dict_desc.decode_cost_per_row(),
        fsst_desc.decode_cost_per_row()
    );

    // A planner that sorts by ascending decode cost evaluates the cheaper (dictionary) predicate first, reducing the
    // working set before paying the per-row cost of the FSST column.
    let mut predicate_pages = [
        (fsst_desc, Transform::FsstString),
        (dict_desc, Transform::DictionaryString),
    ];
    predicate_pages.sort_by_key(|(desc, _)| desc.decode_cost_per_row());
    assert_eq!(
        predicate_pages[0].1,
        Transform::DictionaryString,
        "cheapest predicate (dictionary) must be ordered first"
    );
}
