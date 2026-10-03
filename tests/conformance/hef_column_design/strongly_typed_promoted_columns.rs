//! Checks that a field promoted to its own column gets a precise data type rather than a loose one. Money in particular
//! must be a fixed-scale decimal, never a floating-point number, so amounts stay exact.
use hef::columns::{PromotedColumn, PromotionPlan, validate_promotion_plan};
use hef::encoding::{ColumnData, Transform, encode_block};
use hef::layout::footer::ColumnKind;

/// conformance: hef-column-design/strongly-typed-promoted-columns/money-column-typing
#[test]
fn money_column_typing() {
    // A promoted amount column is fixed-scale decimal, never float: the plan validator rejects float money, and the
    // mandatory representation rule encodes decimals as decimal128.
    let float_money = PromotionPlan {
        columns: vec![PromotedColumn {
            name: "amount_decimal".to_owned(),
            path: "amount".to_owned(),
            kind: ColumnKind::F64,
            since_schema_version: 1,
            substring_searchable: false,
        }],
    };
    assert!(validate_promotion_plan(&float_money).is_err());
    let decimal_money = PromotionPlan {
        columns: vec![PromotedColumn {
            name: "amount_decimal".to_owned(),
            path: "amount".to_owned(),
            kind: ColumnKind::Decimal,
            since_schema_version: 1,
            substring_searchable: false,
        }],
    };
    assert!(validate_promotion_plan(&decimal_money).is_ok());
    let encoded = encode_block(
        &ColumnData::Decimal {
            values: vec![12_345, -6_789],
            scale: 2,
        },
        false,
    );
    assert_eq!(encoded.pipeline.transform().unwrap(), Transform::Decimal128);
}
