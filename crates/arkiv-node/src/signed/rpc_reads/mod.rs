//! Unverified SDK JSON-RPC reads, with bounded native index execution.
//!
//! Cursors retain native snapshot cursors in a bounded process-local cache. They
//! are not proofs and expire after restart or eviction; callers can restart a
//! query at its explicitly retained block height.
//!
//! `arkiv_query`, `arkiv_getEntity` and `arkiv_getEntityCount` are [planned](plan) under the node
//! lock, which checks heights and pins the snapshot, and [run](Planned::run)
//! after it is released (see [`super::reads`]). [`dispatch`] does both.
//!
//! All three evaluate with planned budgets ([`Budget::planned`]): each
//! AND-group is driven by its cheapest index source and counts use stored
//! posting sizes, so the clause order of a query does not decide its cost,
//! and the rows, cursors and counts are exactly the written evaluation's. Their
//! group and literal bounds are the genesis limits unless the node's
//! [`ReadLimits`] (`--read-max-groups`, `--read-max-literals`) raise them;
//! a query beyond them still takes the bounded scan fallback (pages) or is
//! refused (counts). Block execution evaluates no SDK query, so neither
//! choice reaches consensus.
//!
//! Entities follow the user-defined-columns model: every user attribute is
//! returned, indexed or not, but a predicate on a user name only ever
//! matches an indexed one ([`arkiv_db::CellKind::Attribute`]). Refused
//! queries answer with the SDK's code table ([`QueryErrorKind`]).
//!
//! # Order, offset and count
//!
//! Without `ORDER BY` a page lists its matches in record-id order, which is
//! creation order: by block, then transaction, then operation. An `offset`
//! first walks past that many matches reading ids only, then reads on from
//! the last of them ([`Snapshot::cursor_after`]); it costs the walk, metered
//! against the read budget like any other read.
//!
//! Offset and sort walks read engine pages of up to
//! [`ReadLimits::WALK_PAGE`] rows, so a group is planned once per such page
//! rather than once per genesis page. A walk the read budget cannot finish is
//! refused up front, before any of it runs (`unaffordable`): an offset past
//! more matches, or a sort of more matches, than the budget pays for at the
//! least a match can be charged. The match count comes from a small probe
//! (posting sizes usually decide it); when the probe cannot, the walk runs
//! under the budget as before. Only reads that the budget would have ended
//! anyway are refused, and sooner.
//!
//! `ORDER BY name:type [ASC|DESC]` sorts by one attribute, read as one type
//! whether or not it is indexed; an entity without it, or holding another
//! type there, sorts below every value, and ties come in record-id order in
//! either direction. A sorted page is a fetch-and-sort, as upstream's: it
//! reads the sort cell (and the key) of every match at the pinned snapshot,
//! keeps the first `offset + limit` in order in a bounded heap, then reads the
//! `[offset, offset + limit)` window's rows in full. The walk is charged to the
//! read budget, so a match set too large to sort within it is refused as
//! `limits`. Sorted pages have no cursor: they page by `offset`, pinned with
//! `atBlock` for a consistent order, and a `cursor` beside `ORDER BY` is
//! refused. `select.count` adds the match set's size from the planned count
//! path.

mod error;
mod syntax;
#[cfg(test)]
mod tests;

pub use error::{QueryError, QueryErrorKind, ReadError};

use super::{Error, ErrorCode, Result, host::Host, reads::ReadLimits};
use crate::{arkiv, entity_json::scalar, text::hex};
use alloy_primitives::keccak256;
use arkiv_db::{
    Budget, CellName, Cursor, Limits, OperationClass, PriceSchedule, Projection, Query,
    QueryLimits, QueryPage, RecordId, RecordKey, Row, Snapshot, Term, TypeTag, Value,
};
use error::{Read, cursor_mismatch, limits, unknown_block};
use serde_json::{Value as Json, json};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BinaryHeap},
    ops::Bound,
    sync::{Mutex, OnceLock},
};
use syntax::parse_query;

pub(super) const READ_BUDGET: u64 = 25_000_000;
const RESPONSE_CAP: usize = 8 * 1024 * 1024;
/// The bytes a page's entities may take: the response cap less room for the
/// page envelope, its cursor and the JSON-RPC envelope around it.
const PAGE_BYTES: usize = RESPONSE_CAP - 4096;
const CURSOR_CAP: usize = 1024;
const GROUP_CAP: usize = 64;
const LITERAL_CAP: usize = 256;
/// The most rows the scan fallback examines for one request.
const SCAN_ROWS: usize = 4096;
/// The budget of the count that decides whether a walk is affordable
/// (`unaffordable`): enough for a count answered from posting sizes, far
/// below the read budget.
const COUNT_PROBE: u64 = 20_000;
/// The bytes of an entity key, which a sort walk reads for every match.
const KEY_BYTES: u64 = 32;
/// The `select` keys, in response order. Matches the SDK's `RpcSelect`.
const FIELDS: &[&str] = &[
    "key",
    "owner",
    "creator",
    "createdAt",
    "updatedAt",
    "expiresAt",
    "creationFlags",
    "attributeSchema",
    "attributes",
];

/// A query's `ORDER BY`: one attribute, read as one type.
#[derive(Clone, Debug, PartialEq)]
struct Order {
    name: CellName,
    tag: TypeTag,
    descending: bool,
}

impl Order {
    /// The value `row` sorts by: its cell of that name, indexed or not, when
    /// it holds the sort's type.
    fn value(&self, row: &Row) -> Option<Value> {
        row.cells
            .iter()
            .find(|(name, _)| name == &self.name)
            .map(|(_, cell)| &cell.value)
            .filter(|value| value.tag() == self.tag)
            .cloned()
    }

    /// The order of two matches: by value, a missing one below every value
    /// and the whole comparison reversed by `DESC`, then by record id
    /// ascending in either direction, which makes the order total.
    fn cmp(&self, a: (Option<&Value>, RecordId), b: (Option<&Value>, RecordId)) -> Ordering {
        let by_value = match (a.0, b.0) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => x.cmp_ordered(y).unwrap_or(Ordering::Equal),
        };
        let by_value = if self.descending {
            by_value.reverse()
        } else {
            by_value
        };
        by_value.then(a.1.cmp(&b.1))
    }
}

/// A parsed `arkiv_query` string: the filter and its optional order.
#[derive(Clone, Debug, PartialEq)]
struct Statement {
    query: Query,
    order: Option<Order>,
}

#[derive(Clone)]
struct CachedCursor {
    binding: [u8; 32],
    cursor: Cursor,
}

#[derive(Default)]
struct Cursors {
    serial: u64,
    entries: BTreeMap<String, CachedCursor>,
    order: std::collections::VecDeque<String>,
}

static CURSORS: OnceLock<Mutex<Cursors>> = OnceLock::new();

/// Plans and runs an SDK read under the default read limits, or `None` for
/// another method.
pub fn dispatch(host: &Host, method: &str, params: &Json) -> Option<Read<Json>> {
    plan(host, method, params, ReadLimits::default()).map(|planned| planned.and_then(Planned::run))
}

/// The methods whose evaluation runs after the node lock is released.
pub(super) fn detached(method: &str) -> bool {
    matches!(
        method,
        "arkiv_query" | "arkiv_getEntity" | "arkiv_getEntityCount"
    )
}

/// An SDK read with every check against the node done: what is left needs
/// only the pinned snapshot it holds.
pub enum Planned {
    Ready(Json),
    Count {
        snapshot: Snapshot,
        query: Query,
        bounds: QueryLimits,
    },
    Page(Box<PageRead>),
    /// `arkiv_getEntity`: a one-row page whose row, or `null`, is the result.
    Entity(Box<PageRead>),
}

pub struct PageRead {
    snapshot: Snapshot,
    query: Query,
    order: Option<Order>,
    limits: Limits,
    bounds: QueryLimits,
    limit: usize,
    /// Matches to skip before the page; zero on a cursor page.
    offset: u64,
    cursor: Option<Cursor>,
    selection: Selection,
    binding: [u8; 32],
    height: u64,
}

/// Validates an SDK read and pins its snapshot, or `None` for another method.
/// `reads` supplies the node's query bounds.
pub fn plan(host: &Host, method: &str, params: &Json, reads: ReadLimits) -> Option<Read<Planned>> {
    let bounds = reads.query_limits(&host.genesis.genesis.limits);
    Some(match method {
        "arkiv_query" => plan_query(host, params, bounds).map(|page| Planned::Page(Box::new(page))),
        "arkiv_getEntity" => {
            plan_entity(host, params, bounds).map(|page| Planned::Entity(Box::new(page)))
        }
        "arkiv_getEntityCount" => plan_count(host, params, bounds),
        "arkiv_getBlockTiming" => empty_params(params)
            .and_then(|()| {
                let current_block_time = host.head.timestamp_ms / 1000;
                let duration = if host.head.height == 0 {
                    0
                } else {
                    let previous = host.certificate(host.head.height - 1)?.header;
                    current_block_time.saturating_sub(previous.timestamp_ms / 1000)
                };
                Ok(Planned::Ready(json!({
                    "current_block": host.head.height,
                    "current_block_time": current_block_time,
                    "duration": duration,
                })))
            })
            .map_err(ReadError::from),
        _ => return None,
    })
}

impl Planned {
    /// Evaluates the read against its pinned snapshot. Its metered reads stop
    /// early when an enclosing [`arkiv_db::Interrupt`] is raised.
    pub fn run(self) -> Read<Json> {
        match self {
            Self::Ready(value) => Ok(value),
            Self::Count {
                snapshot,
                query,
                bounds,
            } => {
                let mut budget = READ_BUDGET;
                let n = count_matches(&snapshot, &query, bounds, &mut budget)?;
                if n > 9_007_199_254_740_991 {
                    return Err(limits("the count exceeds the JSON safe-integer range").into());
                }
                Ok(json!(n))
            }
            Self::Page(page) => page.run(),
            Self::Entity(page) => {
                let mut result = page.run()?;
                Ok(match result["data"].as_array_mut() {
                    Some(rows) if !rows.is_empty() => rows.swap_remove(0),
                    _ => Json::Null,
                })
            }
        }
    }
}

/// The budget of one native read: `units`, interruptible, planned, admitted
/// against `bounds`.
fn read_budget(units: u64, bounds: QueryLimits) -> Budget {
    Budget::limit(units)
        .interruptible()
        .planned()
        .with_query_limits(bounds)
}

fn exhausted() -> ReadError {
    limits("the read budget is exhausted").into()
}

/// Charges a read's spent units to the request's `budget`.
fn charge(budget: &mut u64, receipt: &arkiv_db::Receipt) -> Read<()> {
    *budget = budget
        .checked_sub(receipt.spent.units())
        .ok_or_else(exhausted)?;
    Ok(())
}

/// How many live entities match `query`, by the planned count (posting
/// sizes where they decide it), charged to `budget`.
fn count_matches(
    snapshot: &Snapshot,
    query: &Query,
    bounds: QueryLimits,
    budget: &mut u64,
) -> Read<u64> {
    match snapshot
        .try_count(
            arkiv::entity_namespace(),
            query,
            read_budget(*budget, bounds),
        )
        .map_err(Error::from)?
    {
        Ok(count) => {
            charge(budget, &count.receipt)?;
            Ok(count.value)
        }
        // Before the entity namespace exists there are no entities.
        Err(r) if r.reason == arkiv_db::Rejection::UnknownNamespace => Ok(0),
        Err(r) => Err(read_error(r)),
    }
}

/// One native page of entities, charged to `budget`, or `None` before the
/// entity namespace exists.
fn native_page(
    snapshot: &Snapshot,
    query: &Query,
    projection: &Projection,
    limit: usize,
    cursor: Option<&Cursor>,
    bounds: QueryLimits,
    budget: &mut u64,
) -> Read<Option<QueryPage>> {
    match snapshot
        .try_query(
            arkiv::entity_namespace(),
            query,
            projection,
            limit,
            cursor,
            read_budget(*budget, bounds),
        )
        .map_err(Error::from)?
    {
        Ok(metered) => {
            charge(budget, &metered.receipt)?;
            Ok(Some(metered.value))
        }
        Err(r) if r.reason == arkiv_db::Rejection::UnknownNamespace => Ok(None),
        Err(r) => Err(read_error(r)),
    }
}

fn plan_count(host: &Host, params: &Json, bounds: QueryLimits) -> Read<Planned> {
    let empty = json!({});
    let options = if params.is_null() {
        &empty
    } else {
        let args = params
            .as_array()
            .filter(|args| args.len() <= 1)
            .ok_or(ErrorCode::InvalidParams)?;
        args.first().filter(|v| !v.is_null()).unwrap_or(&empty)
    };
    let options = options.as_object().ok_or(ErrorCode::InvalidParams)?;
    let height = match options.get("block").filter(|v| !v.is_null()) {
        Some(value) => value.as_u64().ok_or(ErrorCode::InvalidParams)?,
        None => host.head.height,
    };
    if height > host.head.height {
        return Err(unknown_block(height, host.head.height));
    }
    // A count has no order: an `ORDER BY` is read and set aside, so the
    // query string of a page counts as it is.
    let query = match options.get("query").filter(|v| !v.is_null()) {
        Some(value) => parse_query(value.as_str().ok_or(ErrorCode::InvalidParams)?)?.query,
        None => Query::match_all(),
    };
    Ok(Planned::Count {
        snapshot: host.state(height)?,
        query,
        bounds,
    })
}

fn empty_params(params: &Json) -> Result<()> {
    if params.is_null() || params.as_array().is_some_and(Vec::is_empty) {
        Ok(())
    } else {
        Err(ErrorCode::InvalidParams.into())
    }
}

fn quantity(value: &Json) -> Result<u64> {
    if let Some(n) = value.as_u64() {
        return Ok(n);
    }
    value
        .as_str()
        .and_then(crate::text::quantity)
        .ok_or(ErrorCode::InvalidParams.into())
}

fn read_error(error: arkiv_db::Rejected) -> ReadError {
    rejection(&error.reason)
}

/// An engine read rejection in the SDK's table. The callers answer a missing
/// entity namespace (a chain before its first entity) as no entities, so it
/// does not reach here; anything that is no fault of the query is `-32000`.
fn rejection(reason: &arkiv_db::Rejection) -> ReadError {
    use arkiv_db::{QueryFault as F, Rejection as R};
    match reason {
        R::CursorMismatch => cursor_mismatch(),
        R::OutOfBudget => limits("the read budget is exhausted").into(),
        R::InvalidQuery(F::TooManyGroups | F::TooManyLiterals) => {
            limits("the query exceeds the node's query bounds").into()
        }
        R::InvalidQuery(F::ValueTooLarge) => QueryError::new(
            QueryErrorKind::Literal,
            "a str value or prefix exceeds the indexed-string limit",
        )
        .into(),
        R::InvalidQuery(
            fault @ (F::NotIndexable | F::NotOrdered | F::MixedBounds | F::UnboundedRange),
        ) => QueryError::new(QueryErrorKind::Type, fault.to_string()).into(),
        _ => Error::from(ErrorCode::ReadRejected).into(),
    }
}

/// A validated `select`.
#[derive(Clone, Debug, PartialEq)]
struct Selection {
    /// The object as resolved: the request's keys, plus `key: true` unless
    /// it says otherwise or selects nothing but `count`. It binds cursors.
    fields: Json,
    /// `count: true`: the size of the match set rides on every page.
    count: bool,
    /// Whether the request reads a page. `count` with no entity field
    /// selected reads none: `data` is empty and there is no cursor.
    rows: bool,
}

/// The `select` object: every key one of [`FIELDS`] or `count` with a
/// boolean, except `attributes`, which may also map attribute names to
/// booleans. Any other key is refused. `key` defaults to selected unless
/// `count` is the only field selected.
fn selection(value: Option<&Json>) -> Result<Selection> {
    let selected = value.cloned().unwrap_or_else(|| json!({}));
    let object = selected.as_object().ok_or(ErrorCode::InvalidParams)?;
    for (name, v) in object {
        if name == "count" {
            if !v.is_boolean() {
                return Err(ErrorCode::InvalidParams.into());
            }
            continue;
        }
        if !FIELDS.contains(&name.as_str()) {
            return Err(ErrorCode::InvalidParams.into());
        }
        if name == "attributes" && v.is_object() {
            if v.as_object().unwrap().len() > 256
                || !v
                    .as_object()
                    .unwrap()
                    .iter()
                    .all(|(n, v)| arkiv::protocol_name(n) && v.is_boolean())
            {
                return Err(ErrorCode::InvalidParams.into());
            }
        } else if !v.is_boolean() {
            return Err(ErrorCode::InvalidParams.into());
        }
    }
    let count = object.get("count") == Some(&Json::Bool(true));
    let any_field = FIELDS.iter().any(|field| {
        object
            .get(*field)
            .is_some_and(|v| v == true || v.is_object())
    });
    let count_only = count && !any_field;
    let mut fields = selected;
    if !count_only {
        fields
            .as_object_mut()
            .unwrap()
            .entry("key")
            .or_insert(Json::Bool(true));
    }
    Ok(Selection {
        fields,
        count,
        rows: !count_only,
    })
}

/// `arkiv_getEntity`: `[key]` at the head, or `[key, block]` as of a retained
/// block (a hex quantity or a number). The SDK's single-entity read: every
/// field except `attributeSchema`, or `null` when no live entity has the key.
/// Expired entities are purged when their block is built, so a snapshot holds
/// only live ones. Planned as the one-row `$key` query it is equivalent to.
fn plan_entity(host: &Host, params: &Json, bounds: QueryLimits) -> Read<PageRead> {
    let args = params
        .as_array()
        .filter(|p| !p.is_empty() && p.len() <= 2)
        .ok_or(ErrorCode::InvalidParams)?;
    let key = args[0]
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or(ErrorCode::InvalidParams)?;
    let mut options = json!({
        "select": {
            "key": true, "owner": true, "creator": true, "createdAt": true,
            "updatedAt": true, "expiresAt": true, "creationFlags": true,
            "attributes": true,
        },
        "limit": 1,
    });
    if let Some(block) = args.get(1).filter(|b| !b.is_null()) {
        options["atBlock"] = json!(quantity(block)?);
    }
    plan_query(
        host,
        &json!([
            format!("$key = key(0x{})", key.to_ascii_lowercase()),
            options
        ]),
        bounds,
    )
}

/// `arkiv_query`: `[query, options?]`. Options: `atBlock`, `select`, `limit`,
/// `offset` and `cursor`. A cursor carries its own position, so it continues
/// past the offset of the first page and an `offset` beside it is ignored; it
/// needs the explicit `atBlock` of the page that issued it.
fn plan_query(host: &Host, params: &Json, bounds: QueryLimits) -> Read<PageRead> {
    let args = params
        .as_array()
        .filter(|p| !p.is_empty() && p.len() <= 2)
        .ok_or(ErrorCode::InvalidParams)?;
    let source = args[0].as_str().ok_or(ErrorCode::InvalidParams)?;
    let Statement { query, order } = parse_query(source)?;
    let empty = json!({});
    let options = args
        .get(1)
        .unwrap_or(&empty)
        .as_object()
        .ok_or(ErrorCode::InvalidParams)?;
    if options
        .keys()
        .any(|k| !["atBlock", "select", "limit", "offset", "cursor"].contains(&k.as_str()))
    {
        return Err(Error::from(ErrorCode::InvalidParams).into());
    }
    let height = options
        .get("atBlock")
        .map(quantity)
        .transpose()?
        .unwrap_or(host.head.height);
    if height > host.head.height {
        return Err(unknown_block(height, host.head.height));
    }
    // Checked on every page: a cursor is not a lease on its snapshot.
    host.require_state(height)?;
    let selection = selection(options.get("select"))?;
    let limit = options
        .get("limit")
        .map(quantity)
        .transpose()?
        .unwrap_or(100);
    if !(1..=200).contains(&limit) {
        return Err(Error::from(ErrorCode::InvalidParams).into());
    }
    let offset = options
        .get("offset")
        .map(quantity)
        .transpose()?
        .unwrap_or(0);
    let binding: [u8; 32] = keccak256(
        serde_json::to_vec(&json!([
            host.identity.genesis_hash,
            host.identity.run_id,
            height,
            source,
            selection.fields
        ]))
        .map_err(|_| ErrorCode::InvalidParams)?,
    )
    .into();
    let cursor = if let Some(token) = options.get("cursor") {
        // A sorted page is an offset window and never issues a cursor.
        if order.is_some() {
            return Err(QueryError::new(
                QueryErrorKind::Cursor,
                "ORDER BY pages have no cursor: page them with offset, pinned to one block \
                 with atBlock",
            )
            .into());
        }
        if !options.contains_key("atBlock") {
            return Err(cursor_mismatch());
        }
        let token = token
            .as_str()
            .filter(|s| s.len() == 66)
            .ok_or_else(cursor_mismatch)?;
        let cache = CURSORS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| ErrorCode::ReadUnavailable)?;
        let entry = cache
            .entries
            .get(token)
            .filter(|e| e.binding == binding)
            .ok_or_else(cursor_mismatch)?;
        Some(entry.cursor.clone())
    } else {
        None
    };
    Ok(PageRead {
        snapshot: host.state(height)?,
        query,
        order,
        limits: host.genesis.genesis.limits,
        bounds,
        limit: limit as usize,
        offset: if cursor.is_some() { 0 } else { offset },
        cursor,
        selection,
        binding,
        height,
    })
}

impl PageRead {
    fn run(self) -> Read<Json> {
        self.run_within(READ_BUDGET)
    }

    /// The page, every read of the request (count, offset or sort walk,
    /// rows) charged to one `budget`.
    fn run_within(self, mut budget: u64) -> Read<Json> {
        let Self {
            snapshot,
            query,
            order,
            limits,
            bounds,
            limit,
            offset,
            cursor,
            selection,
            binding,
            height,
        } = self;
        if selection.rows
            && let Some(refusal) =
                unaffordable(&snapshot, &query, order.as_ref(), offset, bounds, budget)
        {
            return Err(refusal.into());
        }
        let count = if !selection.count {
            None
        } else if scans(&query, bounds) {
            // Beyond the bounds the count scans, as its page does.
            Some(scan_count(&snapshot, &query, &limits, bounds, &mut budget)?)
        } else {
            Some(count_matches(&snapshot, &query, bounds, &mut budget)?)
        };
        if !selection.rows {
            return page_result(Vec::new(), None, binding, height, count);
        }
        let window = Window {
            snapshot: &snapshot,
            query: &query,
            limits: &limits,
            bounds,
            limit,
            offset,
        };
        // `next`: where the page after this one starts, present only when
        // a further match exists. A sorted page has none.
        let (rows, mut next) = match &order {
            None => {
                let page = read_page(&window, cursor, &mut budget)?;
                (page.rows, page.next)
            }
            Some(order) => (sorted_page(&window, order, &mut budget)?, None),
        };
        let mut data = rows
            .iter()
            .map(|row| project(row, &selection.fields))
            .collect::<Result<Vec<_>>>()?;
        let fit = rows_within(&data, PAGE_BYTES)?;
        if fit < data.len() {
            if fit == 0 {
                return Err(self::limits("one entity alone exceeds the 8 MiB response cap").into());
            }
            // A sorted page has no cursor to carry the cut rows: its
            // window must fit whole.
            if order.is_some() {
                return Err(self::limits(
                    "the sorted page exceeds the 8 MiB response cap; lower the limit",
                )
                .into());
            }
            // A shorter page, whose cut rows are still to come: the next
            // page starts after its last row.
            data.truncate(fit);
            next = Some(snapshot.cursor_after(
                arkiv::entity_namespace(),
                &native_query(&query, bounds),
                &Projection::ALL,
                rows[fit - 1].id,
            ));
        }
        page_result(data, next, binding, height, count)
    }
}

/// The least units a walk charges for each match it passes: `ResultRow` 1
/// with `bytes` of the projection, and the step of the source that found
/// the match (a posting entry, a row visit or a repositioning).
fn least_per_match(pricing: &PriceSchedule, bytes: u64) -> u64 {
    let found = [
        OperationClass::PostingEntry,
        OperationClass::RowVisit,
        OperationClass::RecordProbe,
    ]
    .map(|class| pricing.price(class))
    .into_iter()
    .min()
    .unwrap_or(0);
    pricing
        .price(OperationClass::ResultRow)
        .saturating_add(bytes.saturating_mul(pricing.price(OperationClass::ReadByte)))
        .saturating_add(found)
        .max(1)
}

/// Why a page's walk cannot finish within `budget`, decided before it
/// starts: an `offset` skips `min(offset, matches)` matches reading ids,
/// and an `ORDER BY` reads the key and sort cell of every match, each at
/// least [`least_per_match`]. `None` when the walk may fit, when there is no
/// walk, or when a [`COUNT_PROBE`] count cannot size the match set (the
/// walk then runs under the budget as before). The scan fallback has its own
/// row bound and is never refused here.
fn unaffordable(
    snapshot: &Snapshot,
    query: &Query,
    order: Option<&Order>,
    offset: u64,
    bounds: QueryLimits,
    budget: u64,
) -> Option<QueryError> {
    if scans(query, bounds) {
        return None;
    }
    let pricing = snapshot.pricing();
    let per_match = least_per_match(pricing, if order.is_some() { KEY_BYTES } else { 0 });
    let most = budget / per_match;
    // Without a sort only the skipped matches are walked: an offset the
    // budget covers needs no count.
    if order.is_none() && offset <= most {
        return None;
    }
    let mut probe = COUNT_PROBE;
    let matches = count_matches(snapshot, query, bounds, &mut probe).ok()?;
    match order {
        None => {
            let skipped = offset.min(matches);
            (skipped > most).then(|| {
                limits(&format!(
                    "offset {offset} passes {skipped} matches; the read budget walks at most \
                     {most}: continue from an earlier page's cursor (with its atBlock) or \
                     narrow the query"
                ))
            })
        }
        Some(_) => (matches > most).then(|| {
            limits(&format!(
                "ORDER BY sorts all {matches} matches; the read budget sorts at most {most}: \
                 narrow the query, or drop ORDER BY and page with the cursor"
            ))
        }),
    }
}

/// How many of `rows`, in order, fit in `budget` bytes of a JSON array.
fn rows_within(rows: &[Json], budget: usize) -> Result<usize> {
    let mut used = 2;
    for (i, row) in rows.iter().enumerate() {
        let bytes = serde_json::to_vec(row).map_err(|_| ErrorCode::ResourceLimit)?;
        used += bytes.len() + usize::from(i > 0);
        if used > budget {
            return Ok(i);
        }
    }
    Ok(rows.len())
}

fn page_result(
    data: Vec<Json>,
    next: Option<Cursor>,
    binding: [u8; 32],
    height: u64,
    count: Option<u64>,
) -> Read<Json> {
    let mut result = json!({"data":data,"blockNumber":format!("0x{height:x}")});
    if let Some(count) = count {
        result["count"] = json!(format!("0x{count:x}"));
    }
    if serde_json::to_vec(&result)
        .map_err(|_| ErrorCode::ResourceLimit)?
        .len()
        > RESPONSE_CAP
    {
        return Err(limits("the response exceeds 8 MiB; lower the limit or select less").into());
    }
    if let Some(cursor) = next {
        let mut cache = CURSORS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| ErrorCode::ReadUnavailable)?;
        cache.serial = cache
            .serial
            .checked_add(1)
            .ok_or_else(|| limits("cursor serials are exhausted"))?;
        let token =
            hex(keccak256([binding.as_slice(), &cache.serial.to_be_bytes()].concat()).as_slice());
        while cache.entries.len() >= CURSOR_CAP {
            if let Some(old) = cache.order.pop_front() {
                cache.entries.remove(&old);
            }
        }
        cache.order.push_back(token.clone());
        cache
            .entries
            .insert(token.clone(), CachedCursor { binding, cursor });
        result["cursor"] = json!(token);
    }
    Ok(result)
}

/// The page a request asks for, before its starting position.
struct Window<'a> {
    snapshot: &'a Snapshot,
    query: &'a Query,
    limits: &'a Limits,
    bounds: QueryLimits,
    limit: usize,
    /// Matches to skip first.
    offset: u64,
}

/// Whether `query` exceeds the read's group or literal `bounds`, so that it
/// takes the bounded scan fallback instead of the index.
fn scans(query: &Query, bounds: QueryLimits) -> bool {
    query.groups.len() > bounds.max_groups
        || query.groups.iter().any(|g| g.len() > bounds.max_literals)
}

/// The query the engine evaluates for `query`: itself, or every entity for
/// the scan fallback, which filters the rows itself.
fn native_query(query: &Query, bounds: QueryLimits) -> Query {
    if scans(query, bounds) {
        Query::match_all()
    } else {
        query.clone()
    }
}

/// A record-id page: combine native pages within the read's group, literal
/// and page `bounds` (the chain's committed group and literal limits unless
/// the node raised them). Larger Boolean trees use a bounded scan fallback of
/// at most [`SCAN_ROWS`] rows; other queries take the engine's indexed path.
/// Without a cursor, the first `offset` matches are skipped: by id alone on
/// the indexed path, then the page reads on after the last of them. An
/// offset at or past the end gives an empty page without a continuation.
fn read_page(window: &Window<'_>, mut cursor: Option<Cursor>, budget: &mut u64) -> Read<QueryPage> {
    let &Window {
        snapshot,
        query,
        limits: _,
        bounds,
        limit,
        offset,
    } = window;
    let scan = scans(query, bounds);
    let native = native_query(query, bounds);
    let end = |rows| Ok(QueryPage { rows, next: None });
    let mut skip = offset;
    if !scan {
        while skip > 0 {
            let size = usize::try_from(skip)
                .unwrap_or(usize::MAX)
                .min(bounds.max_page);
            let Some(page) = native_page(
                snapshot,
                &native,
                &Projection::ID_ONLY,
                size,
                cursor.as_ref(),
                bounds,
                budget,
            )?
            else {
                return end(Vec::new());
            };
            skip -= page.rows.len() as u64;
            cursor = match page.next {
                // Read on after the last skipped match with the page's
                // projection: a cursor is a position, not a count.
                Some(next) if skip == 0 => Some(snapshot.cursor_after(
                    arkiv::entity_namespace(),
                    &native,
                    &Projection::ALL,
                    next.after(),
                )),
                Some(next) => Some(next),
                None => return end(Vec::new()),
            };
        }
    }
    let mut rows = Vec::new();
    let mut resume = None;
    for _ in 0..SCAN_ROWS {
        let page_size = if scan {
            1
        } else {
            (limit - rows.len()).min(bounds.max_page)
        };
        if page_size == 0 {
            return Err(exhausted());
        }
        let Some(page) = native_page(
            snapshot,
            &native,
            &Projection::ALL,
            page_size,
            cursor.as_ref(),
            bounds,
            budget,
        )?
        else {
            // Before the entity namespace exists there are no entities.
            return end(rows);
        };
        for row in page.rows {
            if !scan || matches_query(query, &row) {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if rows.len() == limit {
                    return Ok(QueryPage { rows, next: resume });
                }
                rows.push(row);
                resume = page.next.clone();
            }
        }
        cursor = page.next;
        if cursor.is_none() || (!scan && rows.len() == limit) {
            return Ok(QueryPage { rows, next: cursor });
        }
    }
    Err(exhausted())
}

/// What the scan fallback reads of each row to evaluate `query`: the cells
/// its literals name, plus `extra`, and the key when `key`; every cell when
/// they are more than a projection may name.
fn scan_projection(
    query: &Query,
    limits: &Limits,
    key: bool,
    extra: Option<&CellName>,
) -> Projection {
    let names: std::collections::BTreeSet<CellName> = query
        .groups
        .iter()
        .flatten()
        .map(|literal| literal.term.name().clone())
        .chain(extra.cloned())
        .collect();
    if names.len() > limits.max_cells {
        Projection::ALL
    } else {
        Projection::named(key, names)
    }
}

/// `select.count` for a query beyond the read bounds: the matches among at
/// most [`SCAN_ROWS`] rows, reading only the cells the query names.
fn scan_count(
    snapshot: &Snapshot,
    query: &Query,
    limits: &Limits,
    bounds: QueryLimits,
    budget: &mut u64,
) -> Read<u64> {
    let all = Query::match_all();
    let projection = scan_projection(query, limits, false, None);
    let (mut count, mut scanned, mut cursor) = (0u64, 0usize, None);
    loop {
        let Some(page) = native_page(
            snapshot,
            &all,
            &projection,
            limits.max_page,
            cursor.as_ref(),
            bounds,
            budget,
        )?
        else {
            return Ok(0);
        };
        scanned += page.rows.len();
        if scanned > SCAN_ROWS {
            return Err(exhausted());
        }
        count += page
            .rows
            .iter()
            .filter(|row| matches_query(query, row))
            .count() as u64;
        cursor = page.next;
        if cursor.is_none() {
            return Ok(count);
        }
    }
}

/// One match of a sorted walk: its sort value, id and key.
struct Ranked<'o> {
    order: &'o Order,
    value: Option<Value>,
    id: RecordId,
    key: RecordKey,
}

impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ranked<'_> {}
impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order.cmp(
            (self.value.as_ref(), self.id),
            (other.value.as_ref(), other.id),
        )
    }
}

/// An `ORDER BY` page: the `[offset, offset + limit)` window of the matches
/// in `order`, upstream's fetch-and-sort.
///
/// Every match is visited, reading only its key and sort cell (whatever its
/// kind: the sort reads unindexed attributes too), and the first
/// `offset + limit` in order are kept in a bounded heap; then the window's
/// rows are read in full by key. The walk is charged to `budget`, so a match
/// set too large to sort within the read budget is refused as `limits`. The
/// scan fallback reads only the cells it needs and stops at [`SCAN_ROWS`]
/// like a page.
fn sorted_page(window: &Window<'_>, order: &Order, budget: &mut u64) -> Read<Vec<Row>> {
    let &Window {
        snapshot,
        query,
        limits,
        bounds,
        limit,
        offset,
    } = window;
    let scan = scans(query, bounds);
    let native = native_query(query, bounds);
    let projection = if scan {
        scan_projection(query, limits, true, Some(&order.name))
    } else {
        Projection::named(true, [order.name.clone()])
    };
    // The scan fallback keeps its genesis pages: it stops past SCAN_ROWS.
    let page_size = if scan {
        limits.max_page
    } else {
        bounds.max_page
    };
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let keep = start.saturating_add(limit);
    let mut kept: BinaryHeap<Ranked<'_>> = BinaryHeap::new();
    let mut cursor: Option<Cursor> = None;
    let mut scanned = 0usize;
    loop {
        let Some(page) = native_page(
            snapshot,
            &native,
            &projection,
            page_size,
            cursor.as_ref(),
            bounds,
            budget,
        )?
        else {
            return Ok(Vec::new());
        };
        for row in page.rows {
            if scan {
                scanned += 1;
                if scanned > SCAN_ROWS {
                    return Err(exhausted());
                }
                if !matches_query(query, &row) {
                    continue;
                }
            }
            let ranked = Ranked {
                order,
                value: order.value(&row),
                id: row.id,
                key: row.key.clone().ok_or(ErrorCode::StorageCorrupt)?,
            };
            kept.push(ranked);
            if kept.len() > keep {
                kept.pop();
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let sorted = kept.into_sorted_vec();
    let mut rows = Vec::new();
    for ranked in sorted.iter().skip(start).take(limit) {
        let read = snapshot
            .try_get_record(
                arkiv::entity_namespace(),
                &ranked.key,
                &Projection::ALL,
                read_budget(*budget, bounds),
            )
            .map_err(Error::from)?
            .map_err(read_error)?;
        charge(budget, &read.receipt)?;
        rows.push(read.value.ok_or(ErrorCode::StorageCorrupt)?);
    }
    Ok(rows)
}

/// The scan fallback's evaluation of `query` on one row. Like the index, it
/// only sees indexed cells: an unindexed field never matches a predicate.
fn matches_query(query: &Query, row: &Row) -> bool {
    query.groups.iter().any(|group| {
        group.iter().all(|literal| {
            let value = row
                .cells
                .iter()
                .find(|(name, cell)| name == literal.term.name() && cell.is_attribute())
                .map(|(_, c)| &c.value);
            let matches = value.is_some_and(|value| match &literal.term {
                Term::Eq {
                    value: expected, ..
                } => value == expected,
                Term::NotEq {
                    value: expected, ..
                } => value.tag() == expected.tag() && value != expected,
                Term::Exists { tag, .. } => value.tag() == *tag,
                Term::StartsWith { prefix, .. } => {
                    matches!(value, Value::Str(s) if s.starts_with(prefix.as_ref()))
                }
                Term::Range { lower, upper, .. } => {
                    let lower_ok = match lower {
                        Bound::Unbounded => true,
                        Bound::Included(v) => value.cmp_ordered(v).is_some_and(|c| c.is_ge()),
                        Bound::Excluded(v) => value.cmp_ordered(v).is_some_and(|c| c.is_gt()),
                    };
                    let upper_ok = match upper {
                        Bound::Unbounded => true,
                        Bound::Included(v) => value.cmp_ordered(v).is_some_and(|c| c.is_le()),
                        Bound::Excluded(v) => value.cmp_ordered(v).is_some_and(|c| c.is_lt()),
                    };
                    lower_ok && upper_ok
                }
            });
            matches != literal.negated
        })
    })
}

/// One entity in the SDK's `RpcEntity` shape, with only the selected keys.
/// `attributeSchema` entries are `{name, type, indexed}`; `attributes`
/// carries every user attribute, indexed or not, `bytes` values included.
fn project(row: &Row, selected: &Json) -> Result<Json> {
    let e = arkiv::project(row).map_err(|_| ErrorCode::InvalidEntity)?;
    let mut result = serde_json::Map::new();
    for field in FIELDS {
        let choice = &selected[*field];
        if choice != true && !(*field == "attributes" && choice.is_object()) {
            continue;
        }
        let value = match *field {
            "key" => json!(hex(&e.key)),
            "owner" => json!(hex(&e.owner)),
            "creator" => json!(hex(&e.creator)),
            "createdAt" => json!(format!("0x{:x}", e.created_at)),
            "updatedAt" => json!(format!("0x{:x}", e.updated_at)),
            "expiresAt" => json!(format!("0x{:x}", e.expires_at)),
            "creationFlags" => {
                json!({
                    "raw": e.creation_flags,
                    "readonly": e.creation_flags&1!=0,
                    "permissionlessExtension": e.creation_flags&2!=0,
                })
            }
            "attributeSchema" => Json::Array(
                e.attributes
                    .iter()
                    .map(|(n, c)| {
                        scalar(&c.value).map(
                            |(t, _)| json!({"name":n.as_str(),"type":t,"indexed":c.is_attribute()}),
                        )
                    })
                    .collect::<Result<_>>()?,
            ),
            "attributes" => Json::Array(
                e.attributes
                    .iter()
                    .filter(|(n, _)| choice == true || choice[n.as_str()] == true)
                    .map(|(n, c)| {
                        scalar(&c.value).map(|(t, v)| json!({"name":n.as_str(),"type":t,"value":v}))
                    })
                    .collect::<Result<_>>()?,
            ),
            _ => unreachable!(),
        };
        result.insert((*field).into(), value);
    }
    Ok(Json::Object(result))
}
