//! The references an event declares about other events — its parent, its thread root, and anything it links to.
//!
//! A reference is declared by the *later* event about the *earlier* one: a reply names its parent and its thread root,
//! a follow-up names the original. The referenced event is sealed and immutable, so it is never rewritten to learn of
//! its referrers; "children of X" is an equality lookup over the referencing events instead. Nothing validates a
//! target at ingest — a reference whose target is missing, expired, or not yet ingested is stored as declared and
//! simply resolves to nothing at query time.
//!
//! A relationship asserts structure only — reply, grouping, reference — never that one event caused another.
//!
//! See: hef-logical-event-model/spec.md

use super::provenance::hex_lower_into;
use crate::error::RelationshipError;

/// Byte length of a target in the `event_id` space (the 128-bit envelope identity).
pub const EVENT_ID_TARGET_LEN: usize = 16;

/// Byte length of a target in the `protocol_event_id` space (a signed protocol's content-derived identifier).
pub const PROTOCOL_TARGET_LEN: usize = 32;

/// What a reference asserts about its target. The tag is registry-controlled: growth is by adding a variant here, and
/// a stored tag outside the registry never decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RelationshipKind {
    /// An explicit link to another event, with no reply or thread meaning.
    Link,
    /// The event this one directly replies to or continues. At most one per event.
    Parent,
    /// A loose association, weaker than a link. Never a causal claim.
    Related,
    /// The first event of the thread this one belongs to, denormalized so a whole conversation is one equality
    /// lookup. At most one per event; a thread's root carries none.
    Root,
}

impl RelationshipKind {
    /// Every kind, for iterating over the registry.
    pub const ALL: [RelationshipKind; 4] = [
        RelationshipKind::Link,
        RelationshipKind::Parent,
        RelationshipKind::Related,
        RelationshipKind::Root,
    ];

    /// The stored tag for this kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            RelationshipKind::Link => "link",
            RelationshipKind::Parent => "parent",
            RelationshipKind::Related => "related",
            RelationshipKind::Root => "root",
        }
    }

    /// The kind a stored tag names, or `None` when the tag is not in the registry.
    pub fn from_str(tag: &str) -> Option<Self> {
        match tag {
            "link" => Some(RelationshipKind::Link),
            "parent" => Some(RelationshipKind::Parent),
            "related" => Some(RelationshipKind::Related),
            "root" => Some(RelationshipKind::Root),
            _ => None,
        }
    }
}

/// Which identifier space a target is named in. Registry-controlled like the kind.
///
/// A declaring route names targets in the space it actually holds — internal producers hold envelope identity, a
/// signed protocol's events carry the protocol's own identifiers inside signed bytes that must stay byte-exact — and
/// nothing re-resolves them at ingest, so the append path never performs a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetIdSpace {
    /// The envelope's 128-bit `event_id`. Internal identity: never raw public output.
    EventId,
    /// A signed protocol's own content-derived event identifier (32 bytes today).
    ProtocolEventId,
}

impl TargetIdSpace {
    /// The stored tag for this space.
    pub const fn as_str(self) -> &'static str {
        match self {
            TargetIdSpace::EventId => "event_id",
            TargetIdSpace::ProtocolEventId => "protocol_event_id",
        }
    }

    /// The space a stored tag names, or `None` when the tag is not in the registry.
    pub fn from_str(tag: &str) -> Option<Self> {
        match tag {
            "event_id" => Some(TargetIdSpace::EventId),
            "protocol_event_id" => Some(TargetIdSpace::ProtocolEventId),
            _ => None,
        }
    }

    /// The exact byte length a target in this space must have.
    pub const fn target_len(self) -> usize {
        match self {
            TargetIdSpace::EventId => EVENT_ID_TARGET_LEN,
            TargetIdSpace::ProtocolEventId => PROTOCOL_TARGET_LEN,
        }
    }
}

/// One declared reference: what it asserts, which identifier space names the target, and the target's identifier
/// bytes exactly as the declaring route supplied them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelationshipRef {
    pub kind: RelationshipKind,
    pub space: TargetIdSpace,
    pub target_ref: Vec<u8>,
}

impl RelationshipRef {
    /// A reference whose target length matches its space, or an error — a target of the wrong width could never
    /// equal any stored identifier, so storing it would be a reference that lies about resolving.
    pub fn new(kind: RelationshipKind, space: TargetIdSpace, target_ref: Vec<u8>) -> Result<Self, RelationshipError> {
        if target_ref.len() != space.target_len() {
            return Err(RelationshipError::WrongTargetLength);
        }
        Ok(Self {
            kind,
            space,
            target_ref,
        })
    }

    /// A reference to an event by its envelope identity.
    pub fn to_event(kind: RelationshipKind, event_id: u128) -> Self {
        Self {
            kind,
            space: TargetIdSpace::EventId,
            target_ref: event_id.to_be_bytes().to_vec(),
        }
    }

    /// A reference to a signed protocol event by the protocol's own identifier.
    pub fn to_protocol(kind: RelationshipKind, protocol_event_id: [u8; 32]) -> Self {
        Self {
            kind,
            space: TargetIdSpace::ProtocolEventId,
            target_ref: protocol_event_id.to_vec(),
        }
    }

    /// The form a relationship column stores and an equality filter compares: `<space>:<lowercase hex>`. One string
    /// per reference, so a lookup for a target is a plain string equality.
    pub fn column_value(&self) -> String {
        let mut out = String::new();
        self.column_value_into(&mut out);
        out
    }

    /// Appends [`column_value`](Self::column_value)'s text to `out` instead of allocating a fresh `String` — for a
    /// caller building many references' column values in a row through one reused scratch buffer.
    pub fn column_value_into(&self, out: &mut String) {
        out.push_str(self.space.as_str());
        out.push(':');
        hex_lower_into(&self.target_ref, out);
    }

    /// Reads one stored column value back into a reference of `kind`.
    pub fn parse_column_value(kind: RelationshipKind, value: &str) -> Result<Self, RelationshipError> {
        let (space_tag, hex) = value.split_once(':').ok_or(RelationshipError::MalformedColumnValue)?;
        let space = TargetIdSpace::from_str(space_tag).ok_or(RelationshipError::UnknownSpace)?;
        if hex.len() != space.target_len() * 2 || hex.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(RelationshipError::MalformedColumnValue);
        }
        let target_ref = hex_simd::decode_to_vec(hex).map_err(|_| RelationshipError::MalformedColumnValue)?;
        Self::new(kind, space, target_ref)
    }
}

/// Everything one event declares about other events. Absent for the overwhelming majority of events — a stream that
/// declares no relationships materializes no relationship columns at all.
///
/// At most one `parent` and at most one `root` are allowed, because those two kinds are what thread reconstruction
/// filters on; `link` and `related` may repeat freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRelationships {
    refs: Vec<RelationshipRef>,
}

impl EventRelationships {
    /// Validates and wraps a set of declared references.
    pub fn new(refs: Vec<RelationshipRef>) -> Result<Self, RelationshipError> {
        let parents = refs.iter().filter(|r| r.kind == RelationshipKind::Parent).count();
        if parents > 1 {
            return Err(RelationshipError::MoreThanOneParent);
        }
        let roots = refs.iter().filter(|r| r.kind == RelationshipKind::Root).count();
        if roots > 1 {
            return Err(RelationshipError::MoreThanOneRoot);
        }
        Ok(Self { refs })
    }

    /// Every declared reference, in declaration order.
    pub fn refs(&self) -> &[RelationshipRef] {
        &self.refs
    }

    /// The `parent` reference, when this event declared one.
    pub fn parent(&self) -> Option<&RelationshipRef> {
        self.refs.iter().find(|r| r.kind == RelationshipKind::Parent)
    }

    /// The `root` reference, when this event declared one. Absence marks a thread root or an unthreaded event; no
    /// event stores a reference to itself.
    pub fn root(&self) -> Option<&RelationshipRef> {
        self.refs.iter().find(|r| r.kind == RelationshipKind::Root)
    }

    /// The stored column text for `kind`: one `column_value` per reference of that kind, space-separated in
    /// declaration order, or `None` when the event declares none of that kind.
    pub fn column_text(&self, kind: RelationshipKind) -> Option<String> {
        let mut out = String::new();
        self.column_text_into(kind, &mut out).then_some(out)
    }

    /// Writes [`column_text`](Self::column_text)'s text for `kind` into `out` (first clearing it) instead of
    /// building an intermediate `Vec<String>` and joining it — for a caller building every kind's column text for
    /// many rows through one reused scratch buffer. Returns whether this event declared any reference of `kind`;
    /// `out` is left empty when it did not.
    pub fn column_text_into(&self, kind: RelationshipKind, out: &mut String) -> bool {
        out.clear();
        for reference in self.refs.iter().filter(|r| r.kind == kind) {
            if !out.is_empty() {
                out.push(' ');
            }
            reference.column_value_into(out);
        }
        !out.is_empty()
    }

    /// Reads a stored column's text back into the references it encodes.
    pub fn parse_column_text(kind: RelationshipKind, text: &str) -> Result<Vec<RelationshipRef>, RelationshipError> {
        text.split(' ')
            .filter(|value| !value.is_empty())
            .map(|value| RelationshipRef::parse_column_value(kind, value))
            .collect()
    }
}

#[cfg(test)]
#[path = "test/relationships.rs"]
mod tests;
