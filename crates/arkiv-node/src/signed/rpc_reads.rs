//! Unverified SDK JSON-RPC reads, with bounded native index execution.
//!
//! Cursors retain native snapshot cursors in a bounded process-local cache. They
//! are not proofs and expire after restart or eviction; callers can restart a
//! query at its explicitly retained block height.
//!
//! `arkiv_query` and `arkiv_getEntityCount` are [planned](plan) under the node
//! lock, which checks heights and pins the snapshot, and [run](Planned::run)
//! after it is released (see [`super::reads`]). [`dispatch`] does both.
//!
//! Both evaluate with planned budgets ([`Budget::planned`]): each AND-group
//! is driven by its cheapest index source and counts use stored posting
//! sizes, so the clause order of a query no longer decides its cost, and
//! the rows, cursors and counts are exactly the written evaluation's. Their
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
use super::{Error, Result, host::Host, reads::ReadLimits};
use crate::{
    arkiv,
    simulator::{
        query_language::{
            ast::{AnnotKey, AttributeValue, BuiltIn, Query as Ast},
            error::{ParseError, ParseErrorKind},
            lexer::{self, SpannedToken, Token},
            parse,
        },
        wire::hex,
    },
};
use alloy_primitives::{U256, keccak256};
use arkiv_db::{
    Budget, CellName, Cursor, Limits, Literal, Projection, Query, QueryLimits, Row, Snapshot, Term,
    TypeTag, Value,
};
use serde_json::{Value as Json, json};
use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::{Mutex, OnceLock},
};

const READ_BUDGET: u64 = 25_000_000;
const RESPONSE_CAP: usize = 8 * 1024 * 1024;
/// The bytes a page's entities may take: the response cap less room for the
/// page envelope, its cursor and the JSON-RPC envelope around it.
const PAGE_BYTES: usize = RESPONSE_CAP - 4096;
const CURSOR_CAP: usize = 1024;
const GROUP_CAP: usize = 64;
const LITERAL_CAP: usize = 256;
const QUERY_BYTES: usize = 8192;
const MAX_DEPTH: usize = 32;
const MAX_PREDICATES: usize = 64;
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

/// What a node rejected an `arkiv_query` (or `arkiv_getEntityCount`) for:
/// the SDK's frozen code table (`src/query/errors.ts`, `QueryErrorKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryErrorKind {
    /// The query string does not parse (`-32001`).
    Parse,
    /// A well-formed predicate is ill-typed: a range operator on an
    /// equality-only type, an unknown tag (`-32002`).
    Type,
    /// A literal failed validation: an `i32` out of range, a bad EIP-55
    /// checksum, more than 18 decimal places (`-32003`).
    Literal,
    /// A node budget was exceeded: query length, predicate count, nesting
    /// depth, Boolean expansion, read budget or response size (`-32004`).
    Limits,
    /// The cursor expired, is malformed, or belongs to another query, block
    /// or selection (`-32005`).
    Cursor,
    /// The requested block is outside the range the node retains: above the
    /// head or below the state floor (`-32006`).
    Block,
}
impl QueryErrorKind {
    /// The JSON-RPC error code.
    pub const fn code(self) -> i64 {
        match self {
            Self::Parse => -32001,
            Self::Type => -32002,
            Self::Literal => -32003,
            Self::Limits => -32004,
            Self::Cursor => -32005,
            Self::Block => -32006,
        }
    }
    /// The SDK's name for the kind.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Type => "type",
            Self::Literal => "literal",
            Self::Limits => "limits",
            Self::Cursor => "cursor",
            Self::Block => "block",
        }
    }
}
/// A refused SDK query: its kind, a human-readable message and, for errors
/// at a known place in the query string, `data`: `{position, got}` plus
/// `expected` for a parse error. `position` is a UTF-8 byte offset.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryError {
    pub kind: QueryErrorKind,
    pub message: String,
    pub data: Option<Json>,
}
impl QueryError {
    pub fn new(kind: QueryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            data: None,
        }
    }
    /// An error at byte `position` of `source`.
    fn at(kind: QueryErrorKind, source: &str, position: usize, message: impl Into<String>) -> Self {
        let message = message.into();
        let position = position.min(source.len());
        let rest = source.get(position..).unwrap_or("");
        let got: String = rest
            .split_whitespace()
            .next()
            .map_or_else(|| "end of query".into(), |t| t.chars().take(32).collect());
        let mut data = json!({"position": position, "got": got});
        if kind == QueryErrorKind::Parse {
            data["expected"] = json!(message);
        }
        Self {
            kind,
            message,
            data: Some(data),
        }
    }
    /// The JSON-RPC `error` object.
    pub fn to_json(&self) -> Json {
        let mut error = json!({"code": self.kind.code(), "message": format!("query rejected ({}): {}", self.kind.name(), self.message)});
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}
/// Why an SDK read was refused: a query error in the SDK's table, or a node
/// error (invalid parameters, a pruned height, storage).
#[derive(Clone, Debug, PartialEq)]
pub enum ReadError {
    Query(QueryError),
    Node(Error),
}
impl From<Error> for ReadError {
    fn from(error: Error) -> Self {
        Self::Node(error)
    }
}
impl From<QueryError> for ReadError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}
type Read<T> = std::result::Result<T, ReadError>;
fn limits(message: &str) -> QueryError {
    QueryError::new(QueryErrorKind::Limits, message)
}
fn cursor_mismatch() -> ReadError {
    QueryError::new(
        QueryErrorKind::Cursor,
        "the cursor expired, is malformed, or belongs to another query, block or selection",
    )
    .into()
}
fn unknown_block(height: u64, head: u64) -> ReadError {
    QueryError::new(
        QueryErrorKind::Block,
        format!("block {height} is above the head {head}"),
    )
    .into()
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
    matches!(method, "arkiv_query" | "arkiv_getEntityCount")
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
}
pub struct PageRead {
    snapshot: Snapshot,
    query: Query,
    limits: Limits,
    bounds: QueryLimits,
    limit: usize,
    cursor: Option<Cursor>,
    selected: Json,
    binding: [u8; 32],
    height: u64,
}
/// Validates an SDK read and pins its snapshot, or `None` for another method.
/// `reads` supplies the node's query bounds.
pub fn plan(host: &Host, method: &str, params: &Json, reads: ReadLimits) -> Option<Read<Planned>> {
    let bounds = reads.query_limits(&host.genesis.genesis.limits);
    Some(match method {
        "arkiv_query" => {
            plan_query(host, params, bounds).map(|page| Planned::Page(Box::new(page)))
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
                Ok(Planned::Ready(json!({"current_block": host.head.height, "current_block_time": current_block_time, "duration": duration})))
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
                let n = match snapshot
                    .try_count(
                        arkiv::entity_namespace(),
                        &query,
                        read_budget(READ_BUDGET, bounds),
                    )
                    .map_err(Error::from)?
                {
                    Ok(count) => count.value,
                    // Before the entity namespace exists there are no entities.
                    Err(r) if r.reason == arkiv_db::Rejection::UnknownNamespace => 0,
                    Err(r) => return Err(read_error(r)),
                };
                if n > 9_007_199_254_740_991 {
                    return Err(limits("the count exceeds the JSON safe-integer range").into());
                }
                Ok(json!(n))
            }
            Self::Page(page) => page.run(),
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
fn plan_count(host: &Host, params: &Json, bounds: QueryLimits) -> Read<Planned> {
    let empty = json!({});
    let options = if params.is_null() {
        &empty
    } else {
        let args = params
            .as_array()
            .filter(|args| args.len() <= 1)
            .ok_or(Error("InvalidParams"))?;
        args.first().filter(|v| !v.is_null()).unwrap_or(&empty)
    };
    let options = options.as_object().ok_or(Error("InvalidParams"))?;
    let height = match options.get("block").filter(|v| !v.is_null()) {
        Some(value) => value.as_u64().ok_or(Error("InvalidParams"))?,
        None => host.head.height,
    };
    if height > host.head.height {
        return Err(unknown_block(height, host.head.height));
    }
    let query = match options.get("query").filter(|v| !v.is_null()) {
        Some(value) => parse_query(value.as_str().ok_or(Error("InvalidParams"))?)?,
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
        Err(Error("InvalidParams"))
    }
}
fn quantity(value: &Json) -> Result<u64> {
    if let Some(n) = value.as_u64() {
        return Ok(n);
    }
    let s = value
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .ok_or(Error("InvalidParams"))?;
    if s.is_empty()
        || (s.len() > 1 && s.starts_with('0'))
        || !s.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error("InvalidParams"));
    }
    u64::from_str_radix(s, 16).map_err(|_| Error("InvalidParams"))
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
        _ => Error("ReadRejected").into(),
    }
}
/// The `select` object: every key one of [`FIELDS`] with a boolean, except
/// `attributes`, which may also map attribute names to booleans. Anything
/// else, including the removed `payload` and `contentType`, is refused.
fn selection(value: Option<&Json>) -> Result<Json> {
    let selected = value.cloned().unwrap_or_else(|| json!({}));
    let object = selected.as_object().ok_or(Error("InvalidParams"))?;
    for (name, v) in object {
        if !FIELDS.contains(&name.as_str()) {
            return Err(Error("InvalidParams"));
        }
        if name == "attributes" && v.is_object() {
            if v.as_object().unwrap().len() > 256
                || !v
                    .as_object()
                    .unwrap()
                    .iter()
                    .all(|(n, v)| arkiv::protocol_name(n) && v.is_boolean())
            {
                return Err(Error("InvalidParams"));
            }
        } else if !v.is_boolean() {
            return Err(Error("InvalidParams"));
        }
    }
    let mut selected = selected;
    selected
        .as_object_mut()
        .unwrap()
        .entry("key")
        .or_insert(Json::Bool(true));
    Ok(selected)
}
fn plan_query(host: &Host, params: &Json, bounds: QueryLimits) -> Read<PageRead> {
    let args = params
        .as_array()
        .filter(|p| !p.is_empty() && p.len() <= 2)
        .ok_or(Error("InvalidParams"))?;
    let source = args[0].as_str().ok_or(Error("InvalidParams"))?;
    let query = parse_query(source)?;
    let empty = json!({});
    let options = args
        .get(1)
        .unwrap_or(&empty)
        .as_object()
        .ok_or(Error("InvalidParams"))?;
    if options
        .keys()
        .any(|k| !["atBlock", "select", "limit", "cursor"].contains(&k.as_str()))
    {
        return Err(Error("InvalidParams").into());
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
    let selected = selection(options.get("select"))?;
    let limit = options
        .get("limit")
        .map(quantity)
        .transpose()?
        .unwrap_or(100);
    if !(1..=200).contains(&limit) {
        return Err(Error("InvalidParams").into());
    }
    let binding: [u8; 32] = keccak256(
        serde_json::to_vec(&json!([
            host.identity.genesis_hash,
            host.identity.run_id,
            height,
            source,
            selected
        ]))
        .map_err(|_| Error("InvalidParams"))?,
    )
    .into();
    let cursor = if let Some(token) = options.get("cursor") {
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
            .map_err(|_| Error("ReadUnavailable"))?;
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
        limits: host.genesis.genesis.limits,
        bounds,
        limit: limit as usize,
        cursor,
        selected,
        binding,
        height,
    })
}
impl PageRead {
    fn run(self) -> Read<Json> {
        let Self {
            snapshot,
            query,
            limits,
            bounds,
            limit,
            cursor,
            selected,
            binding,
            height,
        } = self;
        let mut page = read_page(&snapshot, &query, &limits, bounds, limit, cursor.clone())?;
        let mut data = page
            .rows
            .iter()
            .map(|row| project(row, &selected))
            .collect::<Result<Vec<_>>>()?;
        let fit = rows_within(&data, PAGE_BYTES)?;
        if fit < data.len() {
            if fit == 0 {
                return Err(self::limits("one entity alone exceeds the 8 MiB response cap").into());
            }
            // A shorter page: re-read the same pinned snapshot with the
            // shorter limit, so its cursor follows the last row returned.
            page = read_page(&snapshot, &query, &limits, bounds, fit, cursor)?;
            if page.rows.len() != fit {
                return Err(Error("StorageCorrupt").into());
            }
            data.truncate(fit);
        }
        page_result(data, page.next, binding, height)
    }
}
/// How many of `rows`, in order, fit in `budget` bytes of a JSON array.
fn rows_within(rows: &[Json], budget: usize) -> Result<usize> {
    let mut used = 2;
    for (i, row) in rows.iter().enumerate() {
        let bytes = serde_json::to_vec(row).map_err(|_| Error("ResourceLimit"))?;
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
) -> Read<Json> {
    let mut result = json!({"data":data,"blockNumber":format!("0x{height:x}")});
    if serde_json::to_vec(&result)
        .map_err(|_| Error("ResourceLimit"))?
        .len()
        > RESPONSE_CAP
    {
        return Err(limits("the response exceeds 8 MiB; lower the limit or select less").into());
    }
    if let Some(cursor) = next {
        let mut cache = CURSORS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| Error("ReadUnavailable"))?;
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
/// Combine native pages within the read's group and literal `bounds` (the
/// chain's committed limits unless the node raised them). Larger Boolean
/// trees use a bounded scan fallback; other queries take the engine's
/// indexed path. The budget is shared by all reads in this RPC.
fn read_page(
    snapshot: &arkiv_db::Snapshot,
    query: &Query,
    limits: &arkiv_db::Limits,
    bounds: QueryLimits,
    limit: usize,
    mut cursor: Option<Cursor>,
) -> Read<arkiv_db::QueryPage> {
    let exhausted = || ReadError::from(self::limits("the read budget is exhausted"));
    let scan = query.groups.len() > bounds.max_groups
        || query.groups.iter().any(|g| g.len() > bounds.max_literals);
    let all = Query::match_all();
    let native = if scan { &all } else { query };
    let mut rows = Vec::new();
    let mut remaining = READ_BUDGET;
    let mut resume = None;
    for _ in 0..4096 {
        let page_size = if scan {
            1
        } else {
            (limit - rows.len()).min(limits.max_page)
        };
        if page_size == 0 {
            return Err(exhausted());
        }
        let metered = match snapshot
            .try_query(
                arkiv::entity_namespace(),
                native,
                &Projection::ALL,
                page_size,
                cursor.as_ref(),
                read_budget(remaining, bounds),
            )
            .map_err(Error::from)?
        {
            Ok(metered) => metered,
            // Before the entity namespace exists there are no entities.
            Err(r) if r.reason == arkiv_db::Rejection::UnknownNamespace => {
                return Ok(arkiv_db::QueryPage { rows, next: None });
            }
            Err(r) => return Err(read_error(r)),
        };
        remaining = remaining
            .checked_sub(metered.receipt.spent.units())
            .ok_or_else(exhausted)?;
        let page = metered.value;
        for row in page.rows {
            if !scan || matches_query(query, &row) {
                if rows.len() == limit {
                    return Ok(arkiv_db::QueryPage { rows, next: resume });
                }
                rows.push(row);
                resume = page.next.clone();
            }
        }
        cursor = page.next;
        if cursor.is_none() || (!scan && rows.len() == limit) {
            return Ok(arkiv_db::QueryPage { rows, next: cursor });
        }
    }
    Err(exhausted())
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
                    matches!(value,Value::Str(s) if s.starts_with(prefix.as_ref()))
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
fn scalar(value: &Value) -> Result<(&'static str, Json)> {
    Ok(match value {
        Value::Bool(v) => ("bool", json!(v)),
        Value::I32(v) => ("i32", json!(v)),
        Value::U64(v) => ("u64", json!(format!("0x{v:x}"))),
        Value::U256(v) => ("u256", json!(format!("0x{:x}", U256::from_be_bytes(*v)))),
        Value::Dec(v) => ("dec", json!(arkiv::format_decimal(v))),
        Value::Bytes32(v) => ("bytes32", json!(hex(v))),
        Value::Bytes(v) => ("bytes", json!(hex(v))),
        Value::Str(v) => ("str", json!(v.as_ref())),
        Value::Addr(v) => ("addr", json!(hex(v))),
        Value::Key(v) => ("key", json!(hex(v))),
        // i64 and the golem-grid scalars are not SDK types.
        _ => return Err(Error("InvalidEntity")),
    })
}
/// Every user attribute of a verified entity, indexed or not, as
/// `{name, type, indexed, value}` with the `arkiv_query` value encoding.
pub(crate) fn verified_attributes(e: &arkiv::ArkivEntityProjection) -> Result<Json> {
    e.attributes
        .iter()
        .map(|(n, c)| {
            scalar(&c.value).map(
                |(t, v)| json!({"name":n.as_str(),"type":t,"indexed":c.is_attribute(),"value":v}),
            )
        })
        .collect::<Result<Vec<_>>>()
        .map(Json::Array)
}
/// One entity in the SDK's `RpcEntity` shape, with only the selected keys.
/// `attributeSchema` entries are `{name, type, indexed}`; `attributes`
/// carries every user attribute, indexed or not, `bytes` values included.
fn project(row: &Row, selected: &Json) -> Result<Json> {
    let e = arkiv::project(row).map_err(|_| Error("InvalidEntity"))?;
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
                json!({"raw":e.creation_flags,"readonly":e.creation_flags&1!=0,"permissionlessExtension":e.creation_flags&2!=0})
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

// Boolean conversion is bounded before expansion, not after allocating a DNF.
fn bounded(groups: Vec<Vec<Literal>>) -> std::result::Result<Query, QueryError> {
    if groups.len() > GROUP_CAP || groups.iter().map(Vec::len).sum::<usize>() > LITERAL_CAP {
        return Err(limits(
            "the query expands to more than 64 groups or 256 literals",
        ));
    }
    Ok(Query { groups })
}
fn union(mut a: Query, b: Query) -> std::result::Result<Query, QueryError> {
    if a.groups.len() + b.groups.len() > GROUP_CAP {
        return Err(limits("the query expands to more than 64 groups"));
    }
    a.groups.extend(b.groups);
    bounded(a.groups)
}
fn intersect(a: Query, b: Query) -> std::result::Result<Query, QueryError> {
    if a.groups.len().saturating_mul(b.groups.len()) > GROUP_CAP {
        return Err(limits("the query expands to more than 64 groups"));
    }
    let total = a
        .groups
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        .saturating_mul(b.groups.len())
        + b.groups
            .iter()
            .map(Vec::len)
            .sum::<usize>()
            .saturating_mul(a.groups.len());
    if total > LITERAL_CAP {
        return Err(limits("the query expands to more than 256 literals"));
    }
    bounded(
        a.groups
            .iter()
            .flat_map(|a| {
                b.groups
                    .iter()
                    .map(move |b| a.iter().chain(b).cloned().collect())
            })
            .collect(),
    )
}
fn complement(query: Query) -> std::result::Result<Query, QueryError> {
    let mut out = Query::match_all();
    for group in query.groups {
        out = intersect(
            out,
            Query {
                groups: group
                    .into_iter()
                    .map(|mut l| {
                        l.negated = !l.negated;
                        vec![l]
                    })
                    .collect(),
            },
        )?;
    }
    Ok(out)
}
fn name(key: &AnnotKey) -> Option<CellName> {
    let text = match key {
        AnnotKey::User(n) => n.as_str(),
        AnnotKey::BuiltIn(b) => match b {
            BuiltIn::Owner => "$owner",
            BuiltIn::Creator => "$creator",
            BuiltIn::Key => "$key",
            BuiltIn::ExpiresAt => "$expiresAt",
            BuiltIn::CreatedAt => "$createdAt",
        },
    };
    CellName::new(text).ok()
}
fn native_value(key: &AnnotKey, value: &AttributeValue) -> Option<Value> {
    Some(match value {
        AttributeValue::Bool(v) => Value::Bool(*v),
        AttributeValue::Int(v) => Value::I32(*v),
        AttributeValue::U64(v) => Value::U64(*v),
        AttributeValue::U256(v)
            if matches!(
                key,
                AnnotKey::BuiltIn(BuiltIn::CreatedAt | BuiltIn::ExpiresAt)
            ) =>
        {
            Value::U64(U256::from_be_bytes(*v).try_into().ok()?)
        }
        AttributeValue::U256(v) => Value::U256(*v),
        AttributeValue::Decimal(v) => Value::Dec(*v),
        AttributeValue::Bytes32(v) => Value::Bytes32(*v),
        AttributeValue::Str(v) => Value::Str(v.as_str().into()),
        AttributeValue::EthereumAddress(v) => Value::Addr(*v),
        AttributeValue::EntityKey(v) => Value::Key(*v),
    })
}
/// One predicate's term; `source` and `start` locate it for errors.
fn leaf(ast: Ast, neq: bool, source: &str, start: usize) -> std::result::Result<Query, QueryError> {
    let (key, value, op) = match ast {
        Ast::Eq { key, value } => (key, value, 0),
        Ast::Gt { key, value } => (key, value, 1),
        Ast::Gte { key, value } => (key, value, 2),
        Ast::Lt { key, value } => (key, value, 3),
        Ast::Lte { key, value } => (key, value, 4),
        Ast::StartsWith { key, value } => (key, value, 5),
        _ => {
            return Err(QueryError::at(
                QueryErrorKind::Parse,
                source,
                start,
                "a predicate",
            ));
        }
    };
    let n = name(&key)
        .ok_or_else(|| QueryError::at(QueryErrorKind::Parse, source, start, "an attribute name"))?;
    let v = native_value(&key, &value).ok_or_else(|| {
        QueryError::at(
            QueryErrorKind::Literal,
            source,
            start,
            "a block height that fits in u64",
        )
    })?;
    let term = match op {
        0 if neq => Term::not_eq(n, v),
        0 => Term::eq(n, v),
        1 => Term::range(n, Bound::Excluded(v), Bound::Unbounded),
        2 => Term::range(n, Bound::Included(v), Bound::Unbounded),
        3 => Term::range(n, Bound::Unbounded, Bound::Excluded(v)),
        4 => Term::range(n, Bound::Unbounded, Bound::Included(v)),
        _ => match v {
            Value::Str(s) => Term::starts_with(n, &s),
            _ => {
                return Err(QueryError::at(
                    QueryErrorKind::Type,
                    source,
                    start,
                    "STARTSWITH takes a str prefix",
                ));
            }
        },
    };
    Ok(Query::term(term))
}
const TAGS: &[(&str, TypeTag)] = &[
    ("bool", TypeTag::Bool),
    ("i32", TypeTag::I32),
    ("u64", TypeTag::U64),
    ("u256", TypeTag::U256),
    ("dec", TypeTag::Dec),
    ("bytes32", TypeTag::Bytes32),
    ("str", TypeTag::Str),
    ("addr", TypeTag::Addr),
    ("key", TypeTag::Key),
];
struct Parser<'a> {
    source: &'a str,
    tokens: Vec<SpannedToken>,
    at: usize,
    depth: usize,
    predicates: usize,
}
/// A source-language error, located in the whole query: `offset` is where
/// the sub-string the parser saw starts.
fn source_error(error: ParseError, source: &str, offset: usize) -> QueryError {
    let kind = match error.kind {
        ParseErrorKind::MalformedInputError => QueryErrorKind::Parse,
        ParseErrorKind::TypeError => QueryErrorKind::Type,
        ParseErrorKind::LiteralError => QueryErrorKind::Literal,
        ParseErrorKind::QueryLimitError => QueryErrorKind::Limits,
    };
    match error.failure_position {
        Some(position) => QueryError::at(kind, source, offset + position, error.message),
        None => QueryError::new(kind, error.message),
    }
}
fn parse_query(source: &str) -> std::result::Result<Query, QueryError> {
    if source.len() > QUERY_BYTES {
        return Err(limits("the query is longer than 8192 bytes"));
    }
    let tokens = lexer::tokenize(source).map_err(|e| source_error(e, source, 0))?;
    if tokens.len() == 1 && tokens[0].token == Token::Star {
        return Ok(Query::match_all());
    }
    let mut p = Parser {
        source,
        tokens,
        at: 0,
        depth: 0,
        predicates: 0,
    };
    let q = p.or()?;
    if p.at != p.tokens.len() {
        return Err(p.parse_error("AND, OR or the end of the query"));
    }
    Ok(q)
}
impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|t| &t.token)
    }
    /// Where the next token starts, or the end of the query.
    fn position(&self) -> usize {
        self.tokens
            .get(self.at)
            .map_or(self.source.len(), |t| t.start)
    }
    fn parse_error(&self, expected: &str) -> QueryError {
        QueryError::at(
            QueryErrorKind::Parse,
            self.source,
            self.position(),
            expected,
        )
    }
    fn take(&mut self, t: Token) -> bool {
        if self.peek() == Some(&t) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Token, expected: &str) -> std::result::Result<(), QueryError> {
        if self.take(t) {
            Ok(())
        } else {
            Err(self.parse_error(expected))
        }
    }
    fn or(&mut self) -> std::result::Result<Query, QueryError> {
        let mut q = self.and()?;
        while self.take(Token::Or) {
            q = union(q, self.and()?)?;
        }
        Ok(q)
    }
    fn and(&mut self) -> std::result::Result<Query, QueryError> {
        let mut q = self.unary()?;
        while self.take(Token::And) {
            q = intersect(q, self.unary()?)?;
        }
        Ok(q)
    }
    fn unary(&mut self) -> std::result::Result<Query, QueryError> {
        if self.depth >= MAX_DEPTH {
            return Err(limits("the query nests more than 32 levels deep"));
        }
        self.depth += 1;
        let result = if self.take(Token::Not) {
            self.unary().and_then(complement)
        } else if self.take(Token::LParen) {
            self.or()
                .and_then(|q| self.expect(Token::RParen, "')'").map(|()| q))
        } else {
            self.predicate()
        };
        self.depth -= 1;
        result
    }
    fn predicate(&mut self) -> std::result::Result<Query, QueryError> {
        self.predicates += 1;
        if self.predicates > MAX_PREDICATES {
            return Err(limits("the query has more than 64 predicates"));
        }
        if matches!(self.peek(), Some(Token::Exists | Token::TypeOf)) {
            let typed = self.take(Token::TypeOf);
            if !typed {
                self.expect(Token::Exists, "EXISTS")?;
            }
            self.expect(Token::LParen, "'('")?;
            let n = match self.peek().cloned() {
                Some(Token::Name(n)) if arkiv::sdk_safe_name(&n) => n,
                Some(Token::SysName(_)) => {
                    return Err(QueryError::at(
                        QueryErrorKind::Type,
                        self.source,
                        self.position(),
                        "a user attribute: system attributes are always present",
                    ));
                }
                _ => return Err(self.parse_error("an attribute name")),
            };
            self.at += 1;
            self.expect(Token::RParen, "')'")?;
            let n = CellName::new(&n).map_err(|_| self.parse_error("an attribute name"))?;
            if typed {
                self.expect(Token::Eq, "'='")?;
                let Some(Token::Name(tag)) = self.peek().cloned() else {
                    return Err(self.parse_error("a type tag"));
                };
                let tag = TAGS
                    .iter()
                    .find(|(s, _)| s.eq_ignore_ascii_case(&tag))
                    .map(|(_, t)| *t)
                    .ok_or_else(|| {
                        QueryError::at(
                            QueryErrorKind::Type,
                            self.source,
                            self.position(),
                            "an indexable type tag: bool, i32, u64, u256, dec, bytes32, str, \
                             addr or key (bytes is never indexed)",
                        )
                    })?;
                self.at += 1;
                Ok(Query::term(Term::exists(n, tag)))
            } else {
                Ok(Query {
                    groups: TAGS
                        .iter()
                        .map(|(_, t)| vec![Literal::of(Term::exists(n.clone(), *t))])
                        .collect(),
                })
            }
        } else {
            if !matches!(self.peek(), Some(Token::Name(_) | Token::SysName(_))) {
                return Err(self.parse_error("a predicate"));
            }
            let start = self.position();
            let op = self.tokens.get(self.at + 1).ok_or_else(|| {
                QueryError::at(
                    QueryErrorKind::Parse,
                    self.source,
                    self.source.len(),
                    "a comparison operator",
                )
            })?;
            let neq = op.token == Token::Neq;
            let op_start = op.start;
            self.at += 3;
            if self.at > self.tokens.len() {
                return Err(QueryError::at(
                    QueryErrorKind::Parse,
                    self.source,
                    self.source.len(),
                    "a value",
                ));
            }
            let end = self.position();
            let mut predicate = self.source[start..end].to_string();
            if neq {
                // Same width, so source-parser offsets stay aligned.
                predicate.replace_range(op_start - start..op_start - start + 2, "= ");
            }
            let ast = parse::parse(&predicate).map_err(|e| source_error(e, self.source, start))?;
            leaf(ast, neq, self.source, start)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The reads as served under the lock: planned and run back to back.
    fn bounds(host: &Host) -> QueryLimits {
        ReadLimits::default().query_limits(&host.genesis.genesis.limits)
    }
    fn entity_count(host: &Host, params: &Json) -> Read<Json> {
        plan_count(host, params, bounds(host)).and_then(Planned::run)
    }
    fn query(host: &Host, params: &Json) -> Read<Json> {
        plan_query(host, params, bounds(host)).and_then(PageRead::run)
    }
    /// The SDK's `select`: every key sent as an explicit boolean.
    fn sdk_select(attributes: Json) -> Json {
        json!({"key":true,"owner":true,"creator":true,"createdAt":true,"updatedAt":true,"expiresAt":true,"creationFlags":true,"attributeSchema":true,"attributes":attributes})
    }
    fn kind(result: Read<Json>) -> QueryErrorKind {
        match result {
            Err(ReadError::Query(q)) => q.kind,
            other => panic!("expected a query error, got {other:?}"),
        }
    }
    #[test]
    fn sdk_operators_and_bounds() {
        for s in [
            "*",
            "name STARTSWITH str('pre')",
            "n >= u64(3) AND n < u64(9)",
            "n != u64(7)",
            "EXISTS(name)",
            "TYPEOF(n) = u64",
            "NOT EXISTS(name)",
            "NOT (a = i32(1) OR b = true)",
            "$createdAt >= u64(0)",
        ] {
            assert!(parse_query(s).is_ok(), "{s}");
        }
        let q = parse_query("n != u64(7)").unwrap();
        assert!(matches!(q.groups[0][0].term, Term::NotEq { .. }));
        assert_eq!(
            parse_query("NOT EXISTS(name)").unwrap().groups[0].len(),
            TAGS.len()
        );
        assert!(parse_query("s > str('x')").is_err());
        assert!(parse_query("TYPEOF(n) = bytes").is_err());
        assert!(parse_query(&format!("{}a = true", "NOT ".repeat(40))).is_err());
        let explosive = std::iter::repeat_n("(a = true OR b = true)", 8)
            .collect::<Vec<_>>()
            .join(" AND ");
        assert!(parse_query(&explosive).is_err());
    }
    #[test]
    fn scalar_encodings_do_not_depend_on_magnitude() {
        assert_eq!(scalar(&Value::U64(1)).unwrap(), ("u64", json!("0x1")));
        assert_eq!(
            scalar(&Value::U256(U256::MAX.to_be_bytes())).unwrap().1,
            json!(format!("0x{:x}", U256::MAX))
        );
        assert_eq!(scalar(&Value::I32(-2)).unwrap().1, json!(-2));
        assert_eq!(
            scalar(&Value::Dec(arkiv::parse_decimal("-1.25").unwrap()))
                .unwrap()
                .1,
            json!("-1.25")
        );
        assert!(selection(Some(&json!({"payload":1}))).is_err());
        assert_eq!(selection(None).unwrap(), json!({"key":true}));
        // The removed payload model is not selectable at all.
        for removed in ["payload", "contentType"] {
            for choice in [true, false] {
                let mut select = sdk_select(json!(true));
                select[removed] = json!(choice);
                assert_eq!(selection(Some(&select)), Err(Error("InvalidParams")));
            }
        }
        // Every SDK `RpcSelect` key is accepted, `arkiv.schema` included.
        assert!(selection(Some(&sdk_select(json!(true)))).is_ok());
        assert!(selection(Some(&sdk_select(json!({"arkiv.schema":true,"title":true})))).is_ok());
        assert!(selection(Some(&sdk_select(json!(false)))).is_ok());
    }
    fn fixture() -> Host {
        fixture_with_period(1_000)
    }
    fn fixture_with_period(period_ms: u64) -> Host {
        use crate::{
            accounts::AccountConfig,
            genesis_bundle::*,
            signed::{crypto::Signer, history::History, protocol::*},
        };
        use arkiv_db::Cell;
        use arkiv_db_memory::MemoryVersionedStore;
        use std::sync::Arc;
        let signer = Signer::from_bytes([1; 32]).unwrap();
        let mut native = crate::signed::protocol::default_native_genesis(9515, 1_000);
        native.limits.max_page = 2;
        let genesis = SignedGenesis {
            genesis: native,
            run_id: [7; 16],
            proposer: *signer.address().as_bytes(),
            block_period_ms: period_ms,
            limits: Default::default(),
            accounts: AccountConfig::new(vec![(
                signer.address(),
                U256::from(1_000_000_000_000_000_000u64),
            )])
            .unwrap(),
            prefill: None,
        };
        let indexed = |name: &str, value| (CellName::new(name).unwrap(), Cell::attribute(value));
        let field = |name: &str, value| (CellName::new(name).unwrap(), Cell::field(value));
        let rows = (0..4)
            .map(|n| PrefillEntity {
                creator: *signer.address().as_bytes(),
                create_nonce: n,
                salt: n as u128,
                expires_at_height: 10,
                flags: 0,
                attributes: match n {
                    0 => vec![
                        indexed("n", Value::U64(7)),
                        field("note", Value::str("seven")),
                    ],
                    1 => vec![indexed("n", Value::U64(8))],
                    2 => vec![indexed("n", Value::str("eight"))],
                    // The same name and value as row 0, but unindexed.
                    _ => vec![
                        field("blob", Value::bytes(&[0xab, 0xcd])),
                        field("n", Value::U64(7)),
                    ],
                },
            })
            .collect::<Vec<_>>();
        let genesis = build_genesis(
            genesis,
            PrefillLimits {
                max_entities: 256,
                max_canonical_bytes: MAX_CANONICAL_BYTES,
                max_storage_units: MAX_STORAGE_UNITS,
            },
            *signer.address().as_bytes(),
            &rows,
        )
        .unwrap();
        Host::create_with_prefill(
            Arc::new(MemoryVersionedStore::new()),
            History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash()).unwrap(),
            genesis.clone(),
            [3; 16],
            SignedHeader::sign(&genesis, genesis.header(), &signer).unwrap(),
            Some(signer),
            arkiv_db::durable::Options::default(),
            Some(encode_stream(&rows).unwrap()),
        )
        .unwrap()
    }
    #[test]
    fn indexer_genesis_walk_uses_historical_count_and_numeric_page_limit() {
        let mut host = fixture();
        for _ in 0..10 {
            host.produce().unwrap();
        }
        assert_eq!(entity_count(&host, &json!([])).unwrap(), json!(0));
        assert_eq!(
            entity_count(&host, &json!([{"block":0}])).unwrap(),
            json!(4)
        );
        assert_eq!(
            entity_count(&host, &json!([{"block":0,"query":"n = u64(7)"}])).unwrap(),
            json!(1)
        );
        let page = query(&host, &json!(["*", {"atBlock":"0x0","limit":200,"select":{"key":true,"owner":true,"creator":true,"createdAt":true,"updatedAt":true,"expiresAt":true,"creationFlags":true,"attributes":true}}])).unwrap();
        assert_eq!(page["data"].as_array().unwrap().len(), 4);
        assert_eq!(page["blockNumber"], "0x0");
        for params in [
            json!([{"block":11}]),
            json!([{"block":-1}]),
            json!([{"block":0.5}]),
            json!([{}, {}]),
        ] {
            assert!(entity_count(&host, &params).is_err());
        }
        assert!(query(&host, &json!(["*",{"limit":201}])).is_err());
        assert!(query(&host, &json!(["*",{"limit":0.5}])).is_err());
    }
    #[test]
    fn actual_snapshot_reads_extended_semantics_projection_and_pinned_cursors() {
        let mut host = fixture();
        let count = |q: &str| {
            query(&host, &json!([q])).unwrap()["data"]
                .as_array()
                .unwrap()
                .len()
        };
        assert_eq!(count("*"), 4);
        assert_eq!(count("n != u64(7)"), 1);
        assert_eq!(count("NOT n = u64(7)"), 3);
        assert_eq!(count("EXISTS(n)"), 3);
        assert_eq!(count("NOT EXISTS(n)"), 1);
        assert_eq!(count("TYPEOF(n) = u64"), 2);
        assert_eq!(count("n >= u64(7) AND n < u64(8)"), 1);
        assert_eq!(count("n STARTSWITH str('ei')"), 1);
        assert_eq!(count("$createdAt = u64(0)"), 4);
        assert_eq!(
            dispatch(&host, "arkiv_getEntityCount", &json!([]))
                .unwrap()
                .unwrap(),
            json!(4)
        );
        assert_eq!(
            dispatch(&host, "arkiv_getBlockTiming", &json!([]))
                .unwrap()
                .unwrap()["duration"],
            0
        );
        let selected = json!({"key":true,"attributes":{"n":true},"createdAt":true});
        let first = query(
            &host,
            &json!(["EXISTS(n)",{"limit":"0x1","select":selected}]),
        )
        .unwrap();
        assert_eq!(first["data"][0]["attributes"][0]["value"], "0x7");
        assert_eq!(first["data"][0]["createdAt"], "0x0");
        assert!(first["data"][0].get("owner").is_none());
        let token = first["cursor"].clone();
        let options = json!({"limit":"0x1","select":selected,"cursor":token,"atBlock":"0x0"});
        let second = query(&host, &json!(["EXISTS(n)", options])).unwrap();
        assert_eq!(second["data"][0]["attributes"][0]["value"], "0x8");
        host.produce().unwrap();
        assert_eq!(
            query(&host, &json!(["EXISTS(n)", options])).unwrap()["data"],
            second["data"]
        );
        assert!(query(&host, &json!(["*", options])).is_err());
        let mut changed = options.clone();
        changed["select"] = json!({"owner":true});
        assert!(query(&host, &json!(["EXISTS(n)", changed])).is_err());
        let mut unpinned = options;
        unpinned.as_object_mut().unwrap().remove("atBlock");
        assert!(query(&host, &json!(["EXISTS(n)", unpinned])).is_err());
        let last=query(&host,&json!(["EXISTS(n)",{"limit":"0x1","select":selected,"cursor":second["cursor"],"atBlock":"0x0"}])).unwrap();
        assert_eq!(last["data"][0]["attributes"][0]["type"], "str");
        assert!(last.get("cursor").is_none());
    }
    #[test]
    fn clause_order_and_read_bounds_do_not_change_answers() {
        let host = fixture();
        let run = |method: &str, params: Json, reads: ReadLimits| {
            plan(&host, method, &params, reads)
                .unwrap()
                .and_then(Planned::run)
        };
        let defaults = ReadLimits::default();
        // Broad and selective clauses in either order: one answer.
        for (forwards, backwards) in [
            (
                "$createdAt = u64(0) AND n = u64(7)",
                "n = u64(7) AND $createdAt = u64(0)",
            ),
            (
                "$createdAt >= u64(0) AND TYPEOF(n) = u64 AND n != u64(8)",
                "n != u64(8) AND TYPEOF(n) = u64 AND $createdAt >= u64(0)",
            ),
        ] {
            let page = |source: &str| run("arkiv_query", json!([source]), defaults).unwrap();
            assert_eq!(page(forwards), page(backwards));
            let count = |source: &str| {
                run("arkiv_getEntityCount", json!([{"query":source}]), defaults).unwrap()
            };
            assert_eq!(count(forwards), count(backwards));
        }
        // Nine AND clauses exceed the genesis bound of eight literals: a
        // count is refused and a page scans, unless the node raises it.
        let nine = std::iter::repeat_n("$createdAt = u64(0)", 8)
            .chain(["n = u64(8)"])
            .collect::<Vec<_>>()
            .join(" AND ");
        let wide = ReadLimits {
            max_literals: Some(16),
            ..defaults
        };
        assert_eq!(
            kind(run(
                "arkiv_getEntityCount",
                json!([{"query":nine}]),
                defaults
            )),
            QueryErrorKind::Limits
        );
        assert_eq!(
            run("arkiv_getEntityCount", json!([{"query":nine}]), wide),
            Ok(json!(1))
        );
        let scanned = run("arkiv_query", json!([nine]), defaults).unwrap();
        assert_eq!(scanned["data"].as_array().unwrap().len(), 1);
        assert_eq!(run("arkiv_query", json!([nine]), wide).unwrap(), scanned);
        // Nine OR terms exceed the bound of eight groups the same way.
        let ors = (0..9)
            .map(|n| format!("n = u64({n})"))
            .collect::<Vec<_>>()
            .join(" OR ");
        assert_eq!(
            kind(run(
                "arkiv_getEntityCount",
                json!([{"query":ors}]),
                defaults
            )),
            QueryErrorKind::Limits
        );
        let many = ReadLimits {
            max_groups: Some(9),
            ..defaults
        };
        assert_eq!(
            run("arkiv_getEntityCount", json!([{"query":ors}]), many),
            Ok(json!(2))
        );
        assert!(
            ReadLimits {
                max_groups: Some(0),
                ..defaults
            }
            .validate()
            .is_err()
        );
        assert!(
            ReadLimits {
                max_literals: Some(ReadLimits::MAX_LITERALS + 1),
                ..defaults
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn unindexed_attributes_are_returned_but_never_matched() {
        let host = fixture();
        let page = |source: &str| {
            query(
                &host,
                &json!([source, {"select": sdk_select(json!(true)), "limit": "0xc8"}]),
            )
            .unwrap()["data"]
                .as_array()
                .unwrap()
                .clone()
        };
        // Row 3 holds `n = u64(7)` unindexed: only row 0's indexed one matches,
        // and predicates on `blob`/`note` never match.
        assert_eq!(page("n = u64(7)").len(), 1);
        assert_eq!(page("EXISTS(n)").len(), 3);
        assert_eq!(page("note = str('seven')").len(), 0);
        assert_eq!(page("EXISTS(note) OR EXISTS(blob)").len(), 0);
        assert_eq!(
            dispatch(
                &host,
                "arkiv_getEntityCount",
                &json!([{"query":"n = u64(7)"}])
            )
            .unwrap()
            .unwrap(),
            json!(1)
        );
        let unindexed = page("NOT EXISTS(n)");
        assert_eq!(unindexed.len(), 1);
        assert_eq!(
            unindexed[0]["attributeSchema"],
            json!([
                {"name":"blob","type":"bytes","indexed":false},
                {"name":"n","type":"u64","indexed":false},
            ])
        );
        assert_eq!(
            unindexed[0]["attributes"],
            json!([
                {"name":"blob","type":"bytes","value":"0xabcd"},
                {"name":"n","type":"u64","value":"0x7"},
            ])
        );
        let indexed = page("n = u64(7)");
        assert_eq!(
            indexed[0]["attributeSchema"],
            json!([
                {"name":"n","type":"u64","indexed":true},
                {"name":"note","type":"str","indexed":false},
            ])
        );
        // The SDK's `RpcEntity` keys and nothing else: no payload model.
        let keys: Vec<_> = indexed[0].as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "attributeSchema",
                "attributes",
                "createdAt",
                "creationFlags",
                "creator",
                "expiresAt",
                "key",
                "owner",
                "updatedAt"
            ]
        );
        // A named subset selects unindexed attributes by name too.
        let subset = query(
            &host,
            &json!(["n = u64(7)", {"select": sdk_select(json!({"note": true}))}]),
        )
        .unwrap();
        assert_eq!(
            subset["data"][0]["attributes"],
            json!([{"name":"note","type":"str","value":"seven"}])
        );
    }
    #[test]
    fn query_errors_use_the_sdk_code_table() {
        let mut host = fixture();
        host.produce().unwrap();
        let error = |source: &str, options: Json| match query(&host, &json!([source, options])) {
            Err(ReadError::Query(q)) => q,
            other => panic!("{source}: {other:?}"),
        };
        for (source, kind) in [
            ("n =", QueryErrorKind::Parse),
            ("(n = u64(1)", QueryErrorKind::Parse),
            ("n = u64(1) n = u64(2)", QueryErrorKind::Parse),
            ("EXISTS(and)", QueryErrorKind::Parse),
            ("n > str('x')", QueryErrorKind::Type),
            ("TYPEOF(n) = bytes", QueryErrorKind::Type),
            ("TYPEOF(n) = nope", QueryErrorKind::Type),
            ("EXISTS($owner)", QueryErrorKind::Type),
            ("$contentType = str('text/plain')", QueryErrorKind::Type),
            ("$payload = str('x')", QueryErrorKind::Type),
            ("n = i32(2147483648)", QueryErrorKind::Literal),
            ("n = dec(1.0000000000000000001)", QueryErrorKind::Literal),
        ] {
            assert_eq!(error(source, json!({})).kind, kind, "{source}");
        }
        let explosive = std::iter::repeat_n("(a = true OR b = true)", 8)
            .collect::<Vec<_>>()
            .join(" AND ");
        for source in [
            "a".repeat(8193),
            format!("{}a = true", "NOT ".repeat(40)),
            explosive,
            vec!["a = true"; 65].join(" AND "),
        ] {
            assert_eq!(error(&source, json!({})).kind, QueryErrorKind::Limits);
        }
        let cursor = error(
            "*",
            json!({"atBlock":"0x0","cursor":format!("0x{}", "00".repeat(32))}),
        );
        assert_eq!(cursor.kind, QueryErrorKind::Cursor);
        assert_eq!(
            error("*", json!({"cursor":"0x00"})).kind,
            QueryErrorKind::Cursor
        );
        let block = error("*", json!({"atBlock":"0x99"}));
        assert_eq!(block.kind, QueryErrorKind::Block);
        assert_eq!(
            kind(entity_count(&host, &json!([{"block":99}]))),
            QueryErrorKind::Block
        );
        assert_eq!(
            kind(entity_count(&host, &json!([{"query":"n ="}]))),
            QueryErrorKind::Parse
        );
        // Codes are the SDK's, and a parse error says where.
        let codes: Vec<i64> = [
            QueryErrorKind::Parse,
            QueryErrorKind::Type,
            QueryErrorKind::Literal,
            QueryErrorKind::Limits,
            QueryErrorKind::Cursor,
            QueryErrorKind::Block,
        ]
        .iter()
        .map(|k| k.code())
        .collect();
        assert_eq!(codes, [-32001, -32002, -32003, -32004, -32005, -32006]);
        let parse = error("n = u64(1) AND (m = u64(2)", json!({}));
        assert_eq!(
            parse.to_json()["data"],
            json!({"position":26,"expected":"')'","got":"end of query"})
        );
        assert_eq!(parse.to_json()["code"], -32001);
        let literal = error("n = i32(2147483648)", json!({}));
        assert_eq!(literal.to_json()["data"]["position"], 8);
        // Malformed parameters are not query errors.
        assert!(matches!(
            query(&host, &json!(["*", {"limit": 0}])),
            Err(ReadError::Node(Error("InvalidParams")))
        ));
    }
    /// A chain without a prefill: its entity namespace only appears with
    /// the proposer's bootstrap transaction in block 1.
    fn bare_fixture() -> Host {
        use crate::{
            accounts::AccountConfig,
            signed::{crypto::Signer, history::History, protocol::*},
        };
        use arkiv_db_memory::MemoryVersionedStore;
        use std::sync::Arc;
        let signer = Signer::from_bytes([1; 32]).unwrap();
        let genesis = SignedGenesis {
            genesis: crate::signed::protocol::default_native_genesis(9515, 1_000),
            run_id: [8; 16],
            proposer: *signer.address().as_bytes(),
            block_period_ms: 1_000,
            limits: Default::default(),
            accounts: AccountConfig::new(vec![(
                signer.address(),
                U256::from(1_000_000_000_000_000_000u64),
            )])
            .unwrap(),
            prefill: None,
        };
        Host::create(
            Arc::new(MemoryVersionedStore::new()),
            History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash()).unwrap(),
            genesis.clone(),
            [4; 16],
            SignedHeader::sign(&genesis, genesis.header(), &signer).unwrap(),
            Some(signer),
            arkiv_db::durable::Options::default(),
        )
        .unwrap()
    }
    #[test]
    fn a_chain_without_its_entity_namespace_has_no_entities() {
        let host = bare_fixture();
        assert_eq!(host.head.height, 0);
        for params in [
            json!(["*", {"atBlock":"0x0"}]),
            json!(["*"]),
            json!(["n = u64(1)", {"atBlock":"0x0","select":sdk_select(json!(true))}]),
        ] {
            assert_eq!(
                query(&host, &params).unwrap(),
                json!({"data":[],"blockNumber":"0x0"}),
                "{params}"
            );
        }
        for params in [
            json!([{"block":0}]),
            json!([]),
            json!([{"query":"n = u64(1)"}]),
        ] {
            assert_eq!(entity_count(&host, &params).unwrap(), json!(0), "{params}");
        }
    }
    #[test]
    fn engine_rejections_map_onto_the_sdk_table() {
        use arkiv_db::{QueryFault as F, Rejection as R};
        let kind_of = |reason: R| match rejection(&reason) {
            ReadError::Query(q) => Some(q.kind),
            ReadError::Node(e) => {
                assert_eq!(e, Error("ReadRejected"));
                None
            }
        };
        assert_eq!(kind_of(R::CursorMismatch), Some(QueryErrorKind::Cursor));
        assert_eq!(kind_of(R::OutOfBudget), Some(QueryErrorKind::Limits));
        assert_eq!(
            kind_of(R::InvalidQuery(F::TooManyGroups)),
            Some(QueryErrorKind::Limits)
        );
        assert_eq!(
            kind_of(R::InvalidQuery(F::TooManyLiterals)),
            Some(QueryErrorKind::Limits)
        );
        assert_eq!(
            kind_of(R::InvalidQuery(F::ValueTooLarge)),
            Some(QueryErrorKind::Literal)
        );
        for fault in [
            F::NotIndexable,
            F::NotOrdered,
            F::MixedBounds,
            F::UnboundedRange,
        ] {
            assert_eq!(kind_of(R::InvalidQuery(fault)), Some(QueryErrorKind::Type));
        }
        // No fault of the query: a server error, never a query kind.
        for reason in [
            R::Unauthorized,
            R::CostOverflow,
            R::InvalidQuery(F::NoGroups),
            R::InvalidQuery(F::ProjectionTooLarge),
        ] {
            assert_eq!(kind_of(reason), None);
        }
    }
    #[test]
    fn a_page_past_the_response_cap_is_shortened_and_its_cursor_continues() {
        use crate::{
            TxEnvelope, TxOp,
            signed::{crypto::Signer, protocol::SignedTx},
        };
        use arkiv_db::Cell;
        let mut host = fixture();
        let signer = Signer::from_bytes([1; 32]).unwrap();
        const BIG: u64 = 40;
        for nonce in 0..BIG {
            let mut request_id = [0x55; 32];
            request_id[24..].copy_from_slice(&nonce.to_be_bytes());
            let tx = TxEnvelope {
                chain_id: host.genesis.genesis.chain_id,
                actor: signer.address(),
                request_id,
                tx_budget: 2_000_000,
                ops: vec![TxOp::Arkiv {
                    namespace: arkiv::entity_namespace(),
                    operation: arkiv::ArkivOperation::Create {
                        salt: 100 + nonce as u128,
                        flags: 0,
                        expiry: crate::Expiry {
                            expires_at: 0,
                            min_lifetime: 100,
                        },
                        attributes: vec![
                            (
                                CellName::new("blob").unwrap(),
                                Cell::field(Value::bytes(&vec![nonce as u8; 128 * 1024])),
                            ),
                            (
                                CellName::new("big").unwrap(),
                                Cell::attribute(Value::Bool(true)),
                            ),
                        ],
                    },
                }],
            };
            host.submit(SignedTx::sign(&host.genesis, nonce, tx, &signer).unwrap())
                .unwrap();
            if nonce % 4 == 3 {
                host.produce().unwrap();
            }
        }
        host.produce().unwrap();
        assert_eq!(
            entity_count(&host, &json!([{"query":"big = true"}])).unwrap(),
            json!(BIG)
        );
        let at = format!("0x{:x}", host.head.height);
        let options = json!({"atBlock":at,"limit":"0xc8","select":sdk_select(json!(true))});
        let first = query(&host, &json!(["big = true", options])).unwrap();
        let first_rows = first["data"].as_array().unwrap().clone();
        let size = serde_json::to_vec(&first).unwrap().len();
        // Each entity is about 256 KiB of JSON: the page stops below 8 MiB.
        assert!(size <= RESPONSE_CAP, "{size}");
        assert!(size > RESPONSE_CAP - 300 * 1024, "{size}");
        assert!(first_rows.len() < BIG as usize && !first_rows.is_empty());
        // The cursor follows the last row returned: the walk sees every
        // entity exactly once.
        let mut keys: Vec<_> = first_rows.iter().map(|e| e["key"].clone()).collect();
        let mut cursor = first.get("cursor").cloned();
        assert!(cursor.is_some());
        while let Some(token) = cursor {
            let mut more = options.clone();
            more["cursor"] = token;
            let page = query(&host, &json!(["big = true", more])).unwrap();
            keys.extend(
                page["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["key"].clone()),
            );
            cursor = page.get("cursor").cloned();
        }
        let distinct: std::collections::BTreeSet<_> = keys.iter().map(|k| k.to_string()).collect();
        assert_eq!((keys.len(), distinct.len()), (BIG as usize, BIG as usize));
        // A row that cannot fit even alone keeps the error.
        assert_eq!(rows_within(&[json!("x".repeat(64))], 16).unwrap(), 0);
        assert_eq!(rows_within(&[json!(1), json!(2)], 5).unwrap(), 2);
        assert_eq!(rows_within(&[json!(1), json!(2)], 4).unwrap(), 1);
    }
    #[test]
    fn timing_uses_numeric_height_and_observed_whole_seconds() {
        let mut host = fixture_with_period(1_500);
        let timing = |host: &Host| {
            dispatch(host, "arkiv_getBlockTiming", &json!([]))
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            timing(&host),
            json!({"current_block":0,"current_block_time":1,"duration":0})
        );
        host.produce().unwrap();
        assert_eq!(
            timing(&host),
            json!({"current_block":1,"current_block_time":2,"duration":1})
        );
        host.produce().unwrap();
        assert_eq!(
            timing(&host),
            json!({"current_block":2,"current_block_time":4,"duration":2})
        );
    }
}
