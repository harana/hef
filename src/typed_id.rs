//! The ids HEF stamps on tenants, events, subjects, and keys: each a prefixed, time-sortable value whose type the
//! compiler keeps apart from every other id.
//!
//! Every id is a [`TypeSafeId`], the Rust implementation of the TypeID spec (a UUIDv7 under the hood, displayed as a
//! prefixed string such as `tenant_01h2xcejqtf2nbrexx3vqjhp41`), tagged with a marker type so two ids of the same
//! width can never be swapped by accident.
//!
//! See: hef-logical-event-model/spec.md

pub use type_safe_id::{StaticType, TypeSafeId};

/// Builds a small, deterministic, distinct id by hand, e.g. `TenantId::new_test_id(1)` and `TenantId::new_test_id(2)`
/// for two tenants a test needs to tell apart by eye. Never produces a real (random, time-sortable) id; reach for
/// `XxxId::new()` outside tests. Implemented for every typed id so callers never need `uuid` as a direct dependency just
/// to build test fixtures.
pub trait TypedIdTestExt {
    /// The id whose underlying UUID is the 128-bit value `n`.
    fn new_test_id(n: u128) -> Self;
}

impl<T: StaticType> TypedIdTestExt for TypeSafeId<T> {
    fn new_test_id(n: u128) -> Self {
        Self::from_uuid(uuid::Uuid::from_u128(n))
    }
}

/// Declares one typed id: a public type alias `$name` backed by a [`TypeSafeId`], tagged with a marker type `$tag` that
/// carries the `$prefix` string stamped on every value of that id. An optional leading doc comment is attached to the
/// generated `$name` alias.
///
/// `$prefix` must be lowercase ASCII letters and underscores only, and not start or end with an underscore, the same
/// requirement [`StaticType`] enforces.
macro_rules! define_typed_id {
    ($(#[$doc:meta])* $name:ident, $tag:ident, $prefix:literal) => {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $tag;

        impl $crate::typed_id::StaticType for $tag {
            const TYPE: &'static str = $prefix;
        }

        $(#[$doc])*
        pub type $name = $crate::typed_id::TypeSafeId<$tag>;
    };
}

pub(crate) use define_typed_id;
