//! Requirement: Erasure as severance and de-identification, per jurisdiction.

use hef::deletes::{
    BusinessRecord, EntityDisposition, ErasureDisposition, SubjectId, de_identify_business_record,
    resolve_conflicting_dispositions,
};
use hef::typed_id::TypedIdTestExt;
use std::collections::BTreeMap;

/// conformance:
/// hef-deletes-and-corrections/erasure-as-severance-and-de-identification-per-jurisdiction/
/// transaction-de-identified-not-deleted
#[test]
fn transaction_de_identified_not_deleted() {
    // An invoice referencing a data subject under a tax-retention obligation.
    let mut personal_fields = BTreeMap::new();
    personal_fields.insert("email".to_owned(), "jane@example.com".to_owned());
    personal_fields.insert("name".to_owned(), "Jane Doe".to_owned());
    let mut invoice = BusinessRecord {
        amount: 4_200,
        personal_fields,
        subject_id: Some(SubjectId::new_test_id(1)),
    };

    de_identify_business_record(&mut invoice);

    // The record is retained: its personal foreign key is nulled and personal fields are redacted rather than the
    // record being deleted.
    assert_eq!(invoice.subject_id, None);
    assert_eq!(invoice.personal_fields.get("email").unwrap(), "[redacted]");
    assert_eq!(invoice.personal_fields.get("name").unwrap(), "[redacted]");

    // Revenue still aggregates de-identified: the amount survives untouched.
    assert_eq!(invoice.amount, 4_200);
}

/// conformance:
/// hef-deletes-and-corrections/erasure-as-severance-and-de-identification-per-jurisdiction/
/// conflicting-jurisdictions-resolved
#[test]
fn conflicting_jurisdictions_resolved() {
    // The subject falls under one regime mandating broad erasure (crypto-shred everything, no carve-out) and
    // another mandating retention of financial records (a non-overridable tax-retention carve-out).
    let broad_erasure = EntityDisposition {
        disposition: ErasureDisposition::CryptoShred,
        retention_carve_out: false,
    };
    let tax_retention = EntityDisposition {
        disposition: ErasureDisposition::Retain,
        retention_carve_out: true,
    };

    // The financial record's disposition: the retention carve-out is never overridden, even though the other
    // regime demands crypto-shredding.
    let financial_record_disposition = resolve_conflicting_dispositions(&[broad_erasure, tax_retention]);
    assert_eq!(financial_record_disposition, Some(tax_retention));

    // Everything else the subject owns has no competing carve-out, so the most-protective disposition wins: it is
    // shredded.
    let other_regime = EntityDisposition {
        disposition: ErasureDisposition::RestrictSuppress,
        retention_carve_out: false,
    };
    let everything_else_disposition = resolve_conflicting_dispositions(&[broad_erasure, other_regime]);
    assert_eq!(everything_else_disposition, Some(broad_erasure));
}
