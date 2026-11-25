//! golem-db queries on the engine's native query form.
//!
//! golem-db's filter is a tree of `And` and `Or` over typed leaves. The
//! engine's query is an ordered flat DNF: a union of AND-groups of typed
//! terms, evaluated on the term index in record-id order with keyset
//! cursors, and a separate `count`. A filter is translated to one engine
//! query, and the page, the count and the total come from the engine:
//!
//! | golem-db | engine |
//! | --- | --- |
//! | `Eq` | `Term::Eq` |
//! | `In` | one group per value, each `Term::Eq` |
//! | `Range` | `Term::Range`; with both sides open, `Term::Exists` of the type |
//! | `Prefix` | `Term::StartsWith` |
//! | `Exists` with a type | `Term::Exists` |
//! | `Exists` with no type | one `Term::Exists` group per indexable type |
//! | `And` | the product of its children's groups |
//! | `Or` | the concatenation of its children's groups |
//! | no sort | the engine's order, record id ascending |
//! | `limit` | the engine page limit |
//! | `offset` | an id-only engine walk past the offset, then `cursor_after` |
//! | `with_total`, `count` | the engine's `count` |
//! | projection | the engine's named projection (all cells when `#meta` is wanted) |
//!
//! Reads opt in to the engine's planned evaluation and to read-side bounds
//! ([`BOUNDS`]), as the node's own SDK reads do. Nothing here runs in block
//! execution.
//!
//! What the engine cannot express, and what the adapter does instead (each
//! is counted in [`QueryStats`]):
//!
//! - **ORDER BY.** The engine's only order is the record id. The filter is
//!   still one engine query; the adapter reads the key and the sort cells of
//!   every match, keeps the first `offset + limit` in golem-db's order in a
//!   bounded heap, and reads that window's rows by key. golem-db's own
//!   strategy (`FetchAndSort`) and the node's `ORDER BY` do the same.
//! - **A DNF over the bounds.** A filter whose expansion has more than
//!   [`BOUNDS`]`.max_groups` groups or `.max_literals` literals in a group
//!   is evaluated node by node: every subtree that fits is one engine query
//!   walked to its end, and `And` / `Or` above it are set operations on
//!   record ids. An `In` with more values than groups is walked in chunks.
//! - **A `str` range bound longer than the indexed-string limit.** The
//!   engine refuses such a bound, and it cannot simply be dropped (a long
//!   upper bound still admits every shorter string). The leaf is answered
//!   by walking `Exists(name, str)` with the cell projected and comparing
//!   in the adapter.
//!
//! A leaf no record can match is answered without the engine: a name the
//! engine's name type cannot hold, an `Eq` / `In` value or a prefix longer
//! than the indexed-string limit, an `In` with no value, a range whose
//! bounds admit nothing.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering as Atomic};

use arkiv_db::{
    Branch, Budget, CellName as ArkivName, Cursor, Literal, Projection, Query, QueryLimits,
    QueryPage as EnginePage, RecordId, RecordKey as ArkivKey, Row, Snapshot, Term, TypeTag, Value,
};
use golemdb_cells::reserved;
use golemdb_index::{IndexTerm, RangeError, TermError, TermRange};
use real::{
    ApiError, Bounds, CellName, CellValue, CommitId, Direction, ExistsType, Filter, QueryPage,
    Record, RecordKey, Result, SortKey,
};

use crate::arkiv::{ArkivDb, internal, storage, to_record, to_value, type_tag};

/// golem-db's hardcoded query caps (`golemdb-api` `database.rs`).
const MAX_QUERY_DEPTH: u32 = 64;
const MAX_QUERY_LIMIT: u32 = 100;
const MAX_SORT_KEYS: u32 = 4;

/// Rows of one engine page of a walk (an offset, a sort, a set evaluation).
const WALK_PAGE: usize = 16_384;

/// The read-side bounds the adapter's queries are admitted against, in place
/// of the engine genesis' `max_groups`, `max_literals` and `max_page`.
pub const BOUNDS: QueryLimits = QueryLimits {
    max_groups: 1024,
    max_literals: 256,
    max_page: WALK_PAGE,
};

/// The budget of every adapter read: unmetered, planned, admitted against
/// [`BOUNDS`].
pub(crate) const fn budget() -> Budget {
    Budget::unlimited().planned().with_query_limits(BOUNDS)
}

/// How the adapter answered its queries and counts so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStats {
    /// Answered by one translated engine query (pages, counts and totals).
    pub native: u64,
    /// Answered without the engine: the filter can match no record.
    pub empty: u64,
    /// The filter had no single engine form and was evaluated node by node.
    pub fallback: u64,
    /// Range leaves answered by a scan, for a bound the engine refuses.
    pub scans: u64,
    /// ORDER BY pages, ranked by the adapter.
    pub sorted: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    native: AtomicU64,
    empty: AtomicU64,
    fallback: AtomicU64,
    scans: AtomicU64,
    sorted: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> QueryStats {
        let read = |counter: &AtomicU64| counter.load(Atomic::Relaxed);
        QueryStats {
            native: read(&self.native),
            empty: read(&self.empty),
            fallback: read(&self.fallback),
            scans: read(&self.scans),
            sorted: read(&self.sorted),
        }
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Atomic::Relaxed);
}

// golem-db's query admission (`golemdb-query` `queries.rs`, `plan.rs` and
// `sorting`), in its order: the page size, the filter depth-first, then the
// sort keys. The leaf checks are golem-db's own (`golemdb-index`).

fn page_size(limit: Option<u64>) -> Result<usize> {
    match limit {
        Some(0) => Err(ApiError::InvalidQuery {
            reason: "`limit` must be at least 1",
        }),
        Some(limit) if limit > u64::from(MAX_QUERY_LIMIT) => Err(ApiError::LimitExceeded {
            limit: "query limit",
            max: MAX_QUERY_LIMIT,
        }),
        Some(limit) => Ok(limit as usize),
        None => Ok(MAX_QUERY_LIMIT as usize),
    }
}

fn invalid_field(field: &CellName, source: TermError) -> ApiError {
    ApiError::InvalidArgument {
        message: format!("invalid cell `{field}` in a filter or sort key"),
        source: Some(Box::new(source)),
    }
}

/// The terms a range leaf reads, as golem-db resolves them: `None` when the
/// bounds admit no value.
fn term_range(field: &CellName, bounds: &Bounds) -> Result<Option<TermRange>> {
    TermRange::between(
        field.as_view(),
        bounds.cell_type(),
        bounds.lower().map(CellValue::as_view),
        bounds.upper().map(CellValue::as_view),
    )
    .map_err(|error| match error {
        RangeError::Term(source) => invalid_field(field, source),
        error @ RangeError::TypeMismatch { .. } => ApiError::internal(error),
    })
}

fn admit_filter(filter: &Filter, depth: u32) -> Result<()> {
    let term = |field: &CellName, value: &CellValue| {
        IndexTerm::from_cell_name(field.as_view(), value.as_view())
            .map(drop)
            .map_err(|source| invalid_field(field, source))
    };
    let children = |children: &[Filter], empty: &'static str| {
        let depth = depth + 1;
        if depth > MAX_QUERY_DEPTH {
            return Err(ApiError::LimitExceeded {
                limit: "filter depth",
                max: MAX_QUERY_DEPTH,
            });
        }
        if children.is_empty() {
            return Err(ApiError::InvalidQuery { reason: empty });
        }
        children
            .iter()
            .try_for_each(|child| admit_filter(child, depth))
    };
    match filter {
        Filter::Eq { field, value } => term(field, value.as_cell()),
        Filter::In { field, values } => match values.as_slice() {
            [] => IndexTerm::check_cell_name(field.as_view())
                .map_err(|source| invalid_field(field, source)),
            values => values.iter().try_for_each(|value| term(field, value)),
        },
        Filter::Range { field, bounds } => term_range(field, bounds).map(drop),
        Filter::Prefix { field, prefix } => TermRange::with_prefix(field.as_view(), prefix)
            .map(drop)
            .map_err(|source| invalid_field(field, source)),
        Filter::Exists { field, ty } => {
            TermRange::of_name(field.as_view(), ty.map(ExistsType::cell_type))
                .map(drop)
                .map_err(|source| invalid_field(field, source))
        }
        Filter::And(nodes) => children(nodes, "`And` needs at least one child"),
        Filter::Or(nodes) => children(nodes, "`Or` needs at least one child"),
        _ => Err(ApiError::NotImplemented {
            operation: "filter operator",
        }),
    }
}

fn admit_sort(keys: &[SortKey]) -> Result<()> {
    if keys.len() > MAX_SORT_KEYS as usize {
        return Err(ApiError::LimitExceeded {
            limit: "sort key count",
            max: MAX_SORT_KEYS,
        });
    }
    keys.iter().try_for_each(|key| {
        IndexTerm::check_cell_name(key.field().as_view())
            .map_err(|source| invalid_field(key.field(), source))
    })
}

/// Why a filter, or a node of one, is not sent to the engine as one query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// Its DNF has more groups than [`BOUNDS`] admits.
    Groups,
    /// A group of its DNF has more literals than [`BOUNDS`] admits.
    Literals,
    /// A `str` range bound is longer than the engine's indexed-string limit.
    LongBound,
}

/// An admitted filter in the engine's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// No record can match.
    Empty,
    /// One engine query: the union of its groups.
    Native(Query),
    /// Evaluated node by node.
    Fallback(Fallback),
}

enum Dnf {
    Empty,
    Groups(Vec<Vec<Literal>>),
    Fallback(Fallback),
}

fn leaf(term: Term) -> Dnf {
    Dnf::Groups(vec![vec![Literal::of(term)]])
}

/// The engine name of a golem-db cell name; `None` for one the engine's
/// name type cannot hold, which therefore names no cell.
fn engine_name(field: &CellName) -> Option<ArkivName> {
    std::str::from_utf8(field.as_bytes())
        .ok()
        .and_then(|text| ArkivName::new(text).ok())
}

/// Translates admitted filters for one database.
#[derive(Clone, Copy, Debug)]
pub struct Translator {
    /// The engine's indexed-string limit: golem-db's `max_str_len`.
    pub max_str_len: usize,
}

impl Translator {
    /// The engine form of an admitted `filter`.
    pub fn plan(&self, filter: &Filter) -> Result<Plan> {
        Ok(match self.dnf(filter)? {
            Dnf::Empty => Plan::Empty,
            Dnf::Groups(groups) => Plan::Native(Query { groups }),
            Dnf::Fallback(reason) => Plan::Fallback(reason),
        })
    }

    fn fits(&self, value: &Value) -> bool {
        match value {
            Value::Str(text) => text.len() <= self.max_str_len,
            _ => true,
        }
    }

    /// An equality value the index can hold, or `None`.
    fn value(&self, value: &CellValue) -> Result<Option<Value>> {
        let value = to_value(value)?;
        Ok(self.fits(&value).then_some(value))
    }

    fn dnf(&self, filter: &Filter) -> Result<Dnf> {
        let field = match filter {
            Filter::Eq { field, .. }
            | Filter::In { field, .. }
            | Filter::Range { field, .. }
            | Filter::Prefix { field, .. }
            | Filter::Exists { field, .. } => field,
            Filter::And(children) => return self.and(children),
            Filter::Or(children) => return self.or(children),
            _ => {
                return Err(ApiError::NotImplemented {
                    operation: "filter operator",
                });
            }
        };
        let Some(name) = engine_name(field) else {
            return Ok(Dnf::Empty);
        };
        Ok(match filter {
            Filter::Eq { value, .. } => match self.value(value.as_cell())? {
                Some(value) => leaf(Term::eq(name, value)),
                None => Dnf::Empty,
            },
            Filter::In { values, .. } => {
                let mut groups = Vec::new();
                for value in values.as_slice() {
                    if let Some(value) = self.value(value)? {
                        groups.push(vec![Literal::of(Term::eq(name.clone(), value))]);
                    }
                }
                if groups.is_empty() {
                    Dnf::Empty
                } else if groups.len() > BOUNDS.max_groups {
                    Dnf::Fallback(Fallback::Groups)
                } else {
                    Dnf::Groups(groups)
                }
            }
            Filter::Range { bounds, .. } => {
                if term_range(field, bounds)?.is_none() {
                    return Ok(Dnf::Empty);
                }
                let bound = |bound: Bound<&CellValue>| -> Result<Bound<Value>> {
                    Ok(match bound {
                        Bound::Included(value) => Bound::Included(to_value(value)?),
                        Bound::Excluded(value) => Bound::Excluded(to_value(value)?),
                        Bound::Unbounded => Bound::Unbounded,
                    })
                };
                let (lower, upper) = (bound(bounds.lower())?, bound(bounds.upper())?);
                let fits = |bound: &Bound<Value>| match bound {
                    Bound::Included(value) | Bound::Excluded(value) => self.fits(value),
                    Bound::Unbounded => true,
                };
                if !fits(&lower) || !fits(&upper) {
                    return Ok(Dnf::Fallback(Fallback::LongBound));
                }
                match (&lower, &upper) {
                    // The engine's range needs a bound; the whole run of a
                    // type is its existence test.
                    (Bound::Unbounded, Bound::Unbounded) => {
                        leaf(Term::exists(name, type_tag(bounds.cell_type())?))
                    }
                    _ => leaf(Term::range(name, lower, upper)),
                }
            }
            Filter::Prefix { prefix, .. } => {
                if prefix.len() > self.max_str_len {
                    Dnf::Empty
                } else {
                    leaf(Term::starts_with(name, prefix))
                }
            }
            Filter::Exists { ty, .. } => match ty {
                Some(ty) => leaf(Term::exists(name, type_tag(ty.cell_type())?)),
                None => Dnf::Groups(
                    golem_tags()
                        .map(|tag| vec![Literal::of(Term::exists(name.clone(), tag))])
                        .collect(),
                ),
            },
            _ => unreachable!("node filters returned above"),
        })
    }

    /// The product of the children's groups, in the written order. An empty
    /// child empties the node whatever its siblings are.
    fn and(&self, children: &[Filter]) -> Result<Dnf> {
        let mut product: Vec<Vec<Literal>> = vec![Vec::new()];
        let mut fallback = None;
        for child in children {
            match self.dnf(child)? {
                Dnf::Empty => return Ok(Dnf::Empty),
                Dnf::Fallback(reason) => fallback = fallback.or(Some(reason)),
                Dnf::Groups(_) if fallback.is_some() => {}
                Dnf::Groups(groups) => {
                    if product.len() * groups.len() > BOUNDS.max_groups {
                        fallback = Some(Fallback::Groups);
                        continue;
                    }
                    let longest = |groups: &[Vec<Literal>]| {
                        groups.iter().map(Vec::len).max().unwrap_or_default()
                    };
                    if longest(&product) + longest(&groups) > BOUNDS.max_literals {
                        fallback = Some(Fallback::Literals);
                        continue;
                    }
                    product = product
                        .iter()
                        .flat_map(|left| {
                            groups.iter().map(move |right| {
                                left.iter().chain(right).cloned().collect::<Vec<_>>()
                            })
                        })
                        .collect();
                }
            }
        }
        Ok(match fallback {
            Some(reason) => Dnf::Fallback(reason),
            None => Dnf::Groups(product),
        })
    }

    /// The children's groups one after another, in the written order.
    fn or(&self, children: &[Filter]) -> Result<Dnf> {
        let mut union = Vec::new();
        let mut fallback = None;
        for child in children {
            match self.dnf(child)? {
                Dnf::Empty => {}
                Dnf::Fallback(reason) => fallback = fallback.or(Some(reason)),
                Dnf::Groups(groups) => union.extend(groups),
            }
        }
        Ok(match fallback {
            Some(reason) => Dnf::Fallback(reason),
            None if union.is_empty() => Dnf::Empty,
            None if union.len() > BOUNDS.max_groups => Dnf::Fallback(Fallback::Groups),
            None => Dnf::Groups(union),
        })
    }
}

/// The engine types a golem-db attribute can have: every indexable type
/// except the engine's own `Key`.
fn golem_tags() -> impl Iterator<Item = TypeTag> {
    TypeTag::ALL
        .into_iter()
        .filter(|tag| tag.is_indexable() && *tag != TypeTag::Key)
}

/// One sort key in the engine's terms.
struct Level {
    /// `None` when no cell can have the name: every value is absent.
    name: Option<ArkivName>,
    tag: TypeTag,
    descending: bool,
}

impl Level {
    /// The value `row` sorts by: its cell of that name and type, whatever
    /// its kind; `None` when absent or of another type.
    fn value(&self, row: &Row) -> Option<Value> {
        let name = self.name.as_ref()?;
        row.cells
            .iter()
            .find(|(cell, _)| cell == name)
            .map(|(_, cell)| &cell.value)
            .filter(|value| value.tag() == self.tag)
            .cloned()
    }
}

fn levels(keys: &[SortKey]) -> Result<Vec<Level>> {
    keys.iter()
        .map(|key| {
            Ok(Level {
                name: engine_name(key.field()),
                tag: type_tag(key.cell_type())?,
                descending: key.direction() == Direction::Desc,
            })
        })
        .collect()
}

/// A match in golem-db's query order: each key in turn, an absent value
/// below every present one and the comparison reversed by `Desc`, then the
/// record id ascending in either direction.
struct Ranked<'a> {
    order: &'a [Level],
    values: Vec<Option<Value>>,
    id: RecordId,
    key: ArkivKey,
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        for (level, (left, right)) in self.order.iter().zip(self.values.iter().zip(&other.values)) {
            let by_value = match (left, right) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(left), Some(right)) => left.cmp_ordered(right).unwrap_or(Ordering::Equal),
            };
            let by_value = if level.descending {
                by_value.reverse()
            } else {
                by_value
            };
            if by_value.is_ne() {
                return by_value;
            }
        }
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Ranked<'_> {}

/// The first `keep` of the pushed matches, in order.
struct Top<'a> {
    keep: usize,
    heap: BinaryHeap<Ranked<'a>>,
}

impl<'a> Top<'a> {
    fn new(keep: usize) -> Self {
        Self {
            keep,
            heap: BinaryHeap::new(),
        }
    }

    fn push(&mut self, order: &'a [Level], row: &Row) -> Result<()> {
        let key = row
            .key
            .clone()
            .ok_or_else(|| internal("a matched row has no key"))?;
        self.heap.push(Ranked {
            order,
            values: order.iter().map(|level| level.value(row)).collect(),
            id: row.id,
            key,
        });
        if self.heap.len() > self.keep {
            self.heap.pop();
        }
        Ok(())
    }

    /// The keys of the window past the first `offset`, in order.
    fn window(self, offset: usize) -> Vec<ArkivKey> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .skip(offset)
            .map(|ranked| ranked.key)
            .collect()
    }
}

/// What the engine is asked for to build golem-db records projected to
/// `names`: every cell when the record is read in full or `#meta` (derived
/// from all of them) is wanted, otherwise the named cells.
pub(crate) fn projection(names: Option<&[CellName]>, max_names: usize) -> Projection {
    let Some(names) = names else {
        return Projection::ALL;
    };
    let meta = reserved::META.as_bytes();
    if names.iter().any(|name| name.as_bytes() == meta) || names.len() > max_names {
        return Projection::ALL;
    }
    Projection::named(true, names.iter().filter_map(engine_name))
}

/// A golem-db `query`, as its builder describes it.
pub(crate) struct Request<'a> {
    pub filter: &'a Filter,
    pub sort: &'a [SortKey],
    pub projection: Option<&'a [CellName]>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    pub with_total: bool,
}

impl ArkivDb {
    /// How this database's queries and counts were answered so far.
    pub fn query_stats(&self) -> QueryStats {
        self.counters.snapshot()
    }

    /// The translator of this database's filters.
    pub fn translator(&self) -> Translator {
        Translator {
            max_str_len: self.genesis().cell_limits.max_str_len as usize,
        }
    }

    fn engine_page(
        &self,
        snapshot: &Snapshot,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<EnginePage> {
        snapshot
            .try_query(self.namespace, query, projection, limit, cursor, budget())
            .map_err(storage)?
            .map(|page| page.value)
            .map_err(|rejected| internal(format!("engine query: {}", rejected.reason)))
    }

    fn engine_count(&self, snapshot: &Snapshot, query: &Query) -> Result<u64> {
        snapshot
            .try_count(self.namespace, query, budget())
            .map_err(storage)?
            .map(|count| count.value)
            .map_err(|rejected| internal(format!("engine count: {}", rejected.reason)))
    }

    fn engine_row(
        &self,
        snapshot: &Snapshot,
        key: &ArkivKey,
        projection: &Projection,
    ) -> Result<Row> {
        snapshot
            .try_get_record(self.namespace, key, projection, budget())
            .map_err(storage)?
            .map_err(|rejected| internal(format!("engine get_record: {}", rejected.reason)))?
            .value
            .ok_or_else(|| internal("a matched record is missing"))
    }

    /// Every match of `query`, in record-id order, read with `projection`.
    fn walk(
        &self,
        snapshot: &Snapshot,
        query: &Query,
        projection: &Projection,
        mut each: impl FnMut(Row) -> Result<()>,
    ) -> Result<()> {
        let mut cursor = None;
        loop {
            let page = self.engine_page(snapshot, query, projection, WALK_PAGE, cursor.as_ref())?;
            for row in page.rows {
                each(row)?;
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(()),
            }
        }
    }

    /// Every live record of a committed state, in record-id order.
    pub(crate) fn snapshot_keys(&self, snapshot: &Snapshot) -> Result<Vec<(u64, ArkivKey)>> {
        let mut keys = Vec::new();
        self.walk(
            snapshot,
            &Query::match_all(),
            &Projection::KEY_ONLY,
            |row| {
                keys.push((row.id.ordinal(), row.key.expect("projected")));
                Ok(())
            },
        )?;
        Ok(keys)
    }

    /// Every live record of a branch's working state, in record-id order.
    pub(crate) fn branch_keys(&self, branch: &Branch) -> Result<Vec<(u64, ArkivKey)>> {
        let query = Query::match_all();
        let mut keys = Vec::new();
        let mut cursor = None;
        loop {
            let page = branch
                .try_query(
                    self.namespace,
                    &query,
                    &Projection::KEY_ONLY,
                    WALK_PAGE,
                    cursor.as_ref(),
                    budget(),
                )
                .map_err(storage)?
                .map_err(|rejected| internal(format!("engine query: {}", rejected.reason)))?
                .value;
            for row in page.rows {
                keys.push((row.id.ordinal(), row.key.expect("projected")));
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(keys),
            }
        }
    }

    /// An unsorted page from the engine: an id-only walk past `offset`, then
    /// one page of at most `size` rows read with `projection`.
    fn native_page(
        &self,
        snapshot: &Snapshot,
        query: &Query,
        projection: &Projection,
        offset: u64,
        size: usize,
    ) -> Result<Vec<Row>> {
        let mut skip = offset;
        let mut after = None;
        let mut cursor = None;
        while skip > 0 {
            let step = usize::try_from(skip).map_or(WALK_PAGE, |skip| skip.min(WALK_PAGE));
            let page =
                self.engine_page(snapshot, query, &Projection::ID_ONLY, step, cursor.as_ref())?;
            skip -= page.rows.len() as u64;
            after = page.rows.last().map(|row| row.id).or(after);
            match page.next {
                Some(next) => cursor = Some(next),
                // The offset is at or past the last match.
                None => return Ok(Vec::new()),
            }
        }
        // A cursor is a position: read on after the last skipped match with
        // the projection the page returns.
        let cursor = after.map(|id| snapshot.cursor_after(self.namespace, query, projection, id));
        Ok(self
            .engine_page(snapshot, query, projection, size, cursor.as_ref())?
            .rows)
    }

    /// The match set of a filter with no single engine form: record id to
    /// key. Every node that has an engine form is one engine query.
    fn match_set(&self, snapshot: &Snapshot, filter: &Filter) -> Result<BTreeMap<u64, ArkivKey>> {
        let mut set = BTreeMap::new();
        let mut collect = |query: &Query| {
            self.walk(snapshot, query, &Projection::KEY_ONLY, |row| {
                set.insert(row.id.ordinal(), row.key.expect("projected"));
                Ok(())
            })
        };
        let translator = self.translator();
        match translator.dnf(filter)? {
            Dnf::Empty => {}
            Dnf::Groups(groups) => collect(&Query { groups })?,
            Dnf::Fallback(_) => match filter {
                Filter::And(children) => {
                    let mut children = children.iter();
                    let first = children.next().expect("admitted: at least one child");
                    set = self.match_set(snapshot, first)?;
                    for child in children {
                        if set.is_empty() {
                            break;
                        }
                        let other = self.match_set(snapshot, child)?;
                        set.retain(|id, _| other.contains_key(id));
                    }
                }
                Filter::Or(children) => {
                    for child in children {
                        set.extend(self.match_set(snapshot, child)?);
                    }
                }
                Filter::In { field, values } => {
                    let name = engine_name(field).expect("a fallback leaf has an engine name");
                    let mut groups = Vec::new();
                    for value in values.as_slice() {
                        if let Some(value) = translator.value(value)? {
                            groups.push(vec![Literal::of(Term::eq(name.clone(), value))]);
                        }
                    }
                    for chunk in groups.chunks(BOUNDS.max_groups) {
                        collect(&Query {
                            groups: chunk.to_vec(),
                        })?;
                    }
                }
                Filter::Range { field, bounds } => {
                    set = self.scan_range(snapshot, field, bounds)?;
                }
                _ => return Err(internal("a filter leaf without an engine form")),
            },
        }
        Ok(set)
    }

    /// A `str` range with a bound the engine refuses: every `str` attribute
    /// of the name, compared with the bounds here.
    fn scan_range(
        &self,
        snapshot: &Snapshot,
        field: &CellName,
        bounds: &Bounds,
    ) -> Result<BTreeMap<u64, ArkivKey>> {
        bump(&self.counters.scans);
        let name = engine_name(field).expect("a fallback leaf has an engine name");
        let (lower, upper) = (
            bounds.lower().map(CellValue::value),
            bounds.upper().map(CellValue::value),
        );
        let inside = |text: &[u8]| {
            (match lower {
                Bound::Included(bound) => text >= bound,
                Bound::Excluded(bound) => text > bound,
                Bound::Unbounded => true,
            }) && (match upper {
                Bound::Included(bound) => text <= bound,
                Bound::Excluded(bound) => text < bound,
                Bound::Unbounded => true,
            })
        };
        let mut set = BTreeMap::new();
        self.walk(
            snapshot,
            &Query::term(Term::exists(name.clone(), TypeTag::Str)),
            &Projection::named(true, [name.clone()]),
            |row| {
                let matches = row.cells.iter().any(|(cell, value)| {
                    *cell == name
                        && matches!(&value.value, Value::Str(text) if inside(text.as_bytes()))
                });
                if matches {
                    set.insert(row.id.ordinal(), row.key.expect("projected"));
                }
                Ok(())
            },
        )?;
        Ok(set)
    }

    /// golem-db's `query` at a committed state.
    pub(crate) fn run_query(
        &self,
        commit: Option<CommitId>,
        request: &Request<'_>,
        priced_at: &mut Option<CommitId>,
    ) -> Result<QueryPage> {
        let (snapshot, at) = self.committed(commit, priced_at)?;
        let size = page_size(request.limit)?;
        admit_filter(request.filter, 0)?;
        admit_sort(request.sort)?;
        let order = levels(request.sort)?;
        let offset = request.offset.unwrap_or(0);
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let shape = projection(request.projection, self.engine_max_cells());
        // The key and the sort cells, which is all a ranking reads.
        let sort_cells = Projection::named(true, order.iter().filter_map(|l| l.name.clone()));
        let window = |top: Top<'_>| -> Result<Vec<Row>> {
            top.window(start)
                .iter()
                .map(|key| self.engine_row(&snapshot, key, &shape))
                .collect()
        };
        let (rows, total) = match self.translator().plan(request.filter)? {
            Plan::Empty => {
                bump(&self.counters.empty);
                (Vec::new(), 0)
            }
            Plan::Native(query) => {
                bump(&self.counters.native);
                let rows = if order.is_empty() {
                    self.native_page(&snapshot, &query, &shape, offset, size)?
                } else {
                    bump(&self.counters.sorted);
                    let mut top = Top::new(start.saturating_add(size));
                    self.walk(&snapshot, &query, &sort_cells, |row| top.push(&order, &row))?;
                    window(top)?
                };
                let total = match request.with_total {
                    true => self.engine_count(&snapshot, &query)?,
                    false => 0,
                };
                (rows, total)
            }
            Plan::Fallback(_) => {
                bump(&self.counters.fallback);
                let matches = self.match_set(&snapshot, request.filter)?;
                let rows = if order.is_empty() {
                    matches
                        .values()
                        .skip(start)
                        .take(size)
                        .map(|key| self.engine_row(&snapshot, key, &shape))
                        .collect::<Result<_>>()?
                } else {
                    bump(&self.counters.sorted);
                    let mut top = Top::new(start.saturating_add(size));
                    for key in matches.values() {
                        top.push(&order, &self.engine_row(&snapshot, key, &sort_cells)?)?;
                    }
                    window(top)?
                };
                (rows, matches.len() as u64)
            }
        };
        let records = rows
            .iter()
            .map(|row| self.record(row, request.projection))
            .collect::<Result<_>>()?;
        Ok(QueryPage {
            records,
            total_matched: request.with_total.then_some(total),
            evaluated_at: at,
        })
    }

    /// golem-db's `count` at a committed state.
    pub(crate) fn run_count(
        &self,
        commit: Option<CommitId>,
        filter: &Filter,
        priced_at: &mut Option<CommitId>,
    ) -> Result<u64> {
        let (snapshot, _) = self.committed(commit, priced_at)?;
        admit_filter(filter, 0)?;
        match self.translator().plan(filter)? {
            Plan::Empty => {
                bump(&self.counters.empty);
                Ok(0)
            }
            Plan::Native(query) => {
                bump(&self.counters.native);
                self.engine_count(&snapshot, &query)
            }
            Plan::Fallback(_) => {
                bump(&self.counters.fallback);
                Ok(self.match_set(&snapshot, filter)?.len() as u64)
            }
        }
    }

    /// A golem-db record from an engine row that carries its key.
    fn record(&self, row: &Row, names: Option<&[CellName]>) -> Result<Record> {
        let key = row
            .key
            .as_ref()
            .and_then(|key| <[u8; 32]>::try_from(key.as_bytes()).ok())
            .ok_or_else(|| internal("a matched row has no 32-byte key"))?;
        to_record(RecordKey(key), row, names)
    }
}
