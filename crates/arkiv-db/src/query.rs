//! The declared query subset: an ordered flat DNF over typed attribute terms,
//! record-id ordering, projection and snapshot-pinned keyset pagination.
//!
//! The engine evaluates a query exactly as submitted: AND-groups in order,
//! literals within a group in order, groups unioned in order. Nothing is
//! reordered or planned, so the charged cost and the abort point are a pure
//! function of the state and the query text. The only result order is the
//! record id (creation order within the namespace).
//!
//! # Planned evaluation
//!
//! A local read may opt in with [`Budget::planned`](crate::Budget::planned):
//! each AND-group is then driven by its smallest index source (by the
//! posting sizes stored in the term leaves), its other positive postings are
//! checked by id in a leapfrog intersection instead of on loaded rows, and a
//! count answers from stored sizes where they decide it. Rows, their order,
//! cursors, `next` and counts are exactly the written evaluation's; charges
//! remain a pure function of the state and the query, but no longer of the
//! literal order. Block execution never opts in. The rules are those of the
//! planner module in `typed`, summarized in `docs/snapshot-query-plan.md`.

use std::{collections::BTreeSet, fmt, ops::Bound, sync::Arc};

use crate::{
    engine::SnapshotId,
    namespace::NamespaceId,
    record::{CellName, RecordId, TypeTag, Value},
};

/// One typed predicate on a named attribute.
///
/// A term matches a live record only when the record has an *attribute*
/// (not a field) of that name whose type equals the term's type; a cell of
/// another type or kind counts as absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// The attribute equals `value`.
    Eq {
        /// Attribute name.
        name: CellName,
        /// Typed value; its type selects the index.
        value: Value,
    },
    /// The attribute has `value`'s type and a different value. A record
    /// lacking the attribute does not match.
    NotEq {
        /// Attribute name.
        name: CellName,
        /// Typed value.
        value: Value,
    },
    /// The attribute is an ordered value inside the bounds. At least one
    /// bound must be given; both must carry the same ordered type.
    Range {
        /// Attribute name.
        name: CellName,
        /// Lower bound.
        lower: Bound<Value>,
        /// Upper bound.
        upper: Bound<Value>,
    },
    /// The attribute is a `Str` starting with `prefix` (byte-wise on UTF-8).
    StartsWith {
        /// Attribute name.
        name: CellName,
        /// Prefix; empty matches every `Str` attribute of that name.
        prefix: Arc<str>,
    },
    /// The record has an attribute of that name and type.
    Exists {
        /// Attribute name.
        name: CellName,
        /// Required type.
        tag: TypeTag,
    },
}

impl Term {
    /// Equality on `name`.
    pub fn eq(name: CellName, value: Value) -> Self {
        Self::Eq { name, value }
    }

    /// Inequality on `name` within `value`'s type.
    pub fn not_eq(name: CellName, value: Value) -> Self {
        Self::NotEq { name, value }
    }

    /// A range on `name`.
    pub fn range(name: CellName, lower: Bound<Value>, upper: Bound<Value>) -> Self {
        Self::Range { name, lower, upper }
    }

    /// A prefix test on `name`.
    pub fn starts_with(name: CellName, prefix: &str) -> Self {
        Self::StartsWith {
            name,
            prefix: Arc::from(prefix),
        }
    }

    /// An existence test on `name` with type `tag`.
    pub fn exists(name: CellName, tag: TypeTag) -> Self {
        Self::Exists { name, tag }
    }

    /// The attribute name the term reads.
    pub fn name(&self) -> &CellName {
        match self {
            Self::Eq { name, .. }
            | Self::NotEq { name, .. }
            | Self::Range { name, .. }
            | Self::StartsWith { name, .. }
            | Self::Exists { name, .. } => name,
        }
    }
}

/// A term with a polarity. A negated literal matches every live record of
/// the namespace that the term does not match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Literal {
    /// The predicate.
    pub term: Term,
    /// Whether the literal is the complement of the term.
    pub negated: bool,
}

impl Literal {
    /// A positive literal.
    pub const fn of(term: Term) -> Self {
        Self {
            term,
            negated: false,
        }
    }

    /// A negated literal.
    pub const fn not(term: Term) -> Self {
        Self {
            term,
            negated: true,
        }
    }
}

/// An ordered flat DNF: the union, in order, of AND-groups, each the
/// conjunction, in order, of its literals. An empty group matches every live
/// record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// The AND-groups, evaluated and unioned in this order.
    pub groups: Vec<Vec<Literal>>,
}

impl Query {
    /// A single AND-group.
    pub fn all(literals: Vec<Literal>) -> Self {
        Self {
            groups: vec![literals],
        }
    }

    /// A query that matches every live record.
    pub fn match_all() -> Self {
        Self::all(Vec::new())
    }

    /// A single positive term.
    pub fn term(term: Term) -> Self {
        Self::all(vec![Literal::of(term)])
    }
}

/// Which cells a read returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellSelect {
    /// No cells.
    None,
    /// Every cell of the record.
    All,
    /// Only the named cells that the record has.
    Named(BTreeSet<CellName>),
}

/// What a read returns for each record. The id is always returned; the key
/// and the cells are opt-in, and only projected bytes are charged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    /// Return the key.
    pub key: bool,
    /// Which cells to return.
    pub cells: CellSelect,
}

impl Projection {
    /// Key and every cell.
    pub const ALL: Self = Self {
        key: true,
        cells: CellSelect::All,
    };

    /// Ids only.
    pub const ID_ONLY: Self = Self {
        key: false,
        cells: CellSelect::None,
    };

    /// The key and no cells.
    pub const KEY_ONLY: Self = Self {
        key: true,
        cells: CellSelect::None,
    };

    /// The key plus the named cells.
    pub fn named(key: bool, names: impl IntoIterator<Item = CellName>) -> Self {
        Self {
            key,
            cells: CellSelect::Named(names.into_iter().collect()),
        }
    }
}

/// Why a query was rejected at admission (spent 0), before any charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryFault {
    /// The query has no AND-group.
    NoGroups,
    /// More groups than `Limits::max_groups`.
    TooManyGroups,
    /// A group has more literals than `Limits::max_literals`.
    TooManyLiterals,
    /// The page limit is zero.
    ZeroLimit,
    /// The page limit exceeds `Limits::max_page`.
    LimitTooLarge,
    /// A term names the `Bytes` type, which has no index.
    NotIndexable,
    /// A `Str` value or prefix exceeds the configured or hard indexed limit.
    ValueTooLarge,
    /// A range has no bound on either side.
    UnboundedRange,
    /// A range's bounds carry different types.
    MixedBounds,
    /// A range on a type without order.
    NotOrdered,
    /// A named projection lists more names than `Limits::max_cells`.
    ProjectionTooLarge,
}

impl fmt::Display for QueryFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NoGroups => "query has no groups",
            Self::TooManyGroups => "too many groups",
            Self::TooManyLiterals => "too many literals in a group",
            Self::ZeroLimit => "page limit is zero",
            Self::LimitTooLarge => "page limit too large",
            Self::NotIndexable => "bytes values are not indexable",
            Self::ValueTooLarge => "value too large",
            Self::UnboundedRange => "range has no bound",
            Self::MixedBounds => "range bounds have different types",
            Self::NotOrdered => "range on an unordered type",
            Self::ProjectionTooLarge => "projection names too many cells",
        };
        formatter.write_str(text)
    }
}

/// Where a cursor was issued: one snapshot of one engine, or one branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorContext {
    /// A published snapshot of the engine instance.
    Snapshot {
        /// Engine instance.
        instance: u64,
        /// The pinned snapshot.
        id: SnapshotId,
    },
    /// The working state of a branch at one mutation count.
    Branch {
        /// The branch's process-wide token.
        token: u64,
        /// The branch's mutation serial when the cursor was issued; any
        /// applied write, rollback or pricing change since then invalidates
        /// the cursor.
        mutations: u64,
    },
}

/// A continuation position bound to its context.
///
/// A cursor is a value (the last emitted record id), never a count, so the
/// next page resumes exactly after that record whatever happened to it. It
/// binds the engine instance and snapshot (or the branch and its mutation
/// serial), the namespace, the query and the projection; continuing with
/// any of them changed is rejected with [`crate::Rejection::CursorMismatch`].
/// The page limit is not bound. It never re-resolves head: the caller pins
/// the snapshot the cursor names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub(crate) context: CursorContext,
    pub(crate) namespace: NamespaceId,
    pub(crate) query: Arc<Query>,
    pub(crate) projection: Arc<Projection>,
    pub(crate) after: RecordId,
}

impl Cursor {
    /// The last record id of the page that issued the cursor.
    pub const fn after(&self) -> RecordId {
        self.after
    }

    /// The snapshot the cursor is pinned to; `None` for a branch cursor.
    pub const fn snapshot(&self) -> Option<SnapshotId> {
        match self.context {
            CursorContext::Snapshot { id, .. } => Some(id),
            CursorContext::Branch { .. } => None,
        }
    }

    /// The namespace the cursor is bound to.
    pub const fn namespace(&self) -> NamespaceId {
        self.namespace
    }

    /// The query the cursor is bound to.
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The projection the cursor is bound to.
    pub fn projection(&self) -> &Projection {
        &self.projection
    }
}

/// One page of query results.
///
/// `next` is an exact claim: it is present if and only if at least one
/// further match exists after the page. A total count is a separate metered
/// read (`count`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPage {
    /// The page's rows in record-id order.
    pub rows: Vec<crate::record::Row>,
    /// The continuation, present only when a further match exists.
    pub next: Option<Cursor>,
}
