//! Unverified SDK JSON-RPC reads, with bounded native index execution.
//!
//! Cursors retain native snapshot cursors in a bounded process-local cache. They
//! are not proofs and expire after restart or eviction; callers can restart a
//! query at its explicitly retained block height.
use super::{Error, Result, host::Host};
use crate::{
    arkiv,
    simulator::{
        query_language::{
            ast::{AnnotKey, AttributeValue, BuiltIn, Query as Ast},
            lexer::{self, SpannedToken, Token},
            parse,
        },
        wire::hex,
    },
};
use alloy_primitives::{U256, keccak256};
use arkiv_db::{Budget, CellName, Cursor, Literal, Projection, Query, Row, Term, TypeTag, Value};
use serde_json::{Value as Json, json};
use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::{Mutex, OnceLock},
};

const READ_BUDGET: u64 = 25_000_000;
const RESPONSE_CAP: usize = 8 * 1024 * 1024;
const CURSOR_CAP: usize = 1024;
const GROUP_CAP: usize = 64;
const LITERAL_CAP: usize = 256;
const FIELDS: &[&str] = &[
    "key",
    "owner",
    "creator",
    "createdAt",
    "updatedAt",
    "expiresAt",
    "creationFlags",
    "contentType",
    "payload",
    "attributeSchema",
    "attributes",
];

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

pub fn dispatch(host: &Host, method: &str, params: &Json) -> Option<Result<Json>> {
    Some(match method {
        "arkiv_query" => query(host, params),
        "arkiv_getEntityCount" => entity_count(host, params),
        "arkiv_getBlockTiming" => empty_params(params).and_then(|()| {
            let current_block_time = host.head.timestamp_ms / 1000;
            let duration = if host.head.height == 0 {
                0
            } else {
                let previous = host.certificate(host.head.height - 1)?.header;
                current_block_time.saturating_sub(previous.timestamp_ms / 1000)
            };
            Ok(json!({"current_block": host.head.height, "current_block_time": current_block_time, "duration": duration}))
        }),
        _ => return None,
    })
}
fn entity_count(host: &Host, params: &Json) -> Result<Json> {
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
        return Err(Error("UnknownBlock"));
    }
    let query = match options.get("query").filter(|v| !v.is_null()) {
        Some(value) => parse_query(value.as_str().ok_or(Error("InvalidParams"))?)?,
        None => Query::match_all(),
    };
    let snapshot = host.state(height)?;
    let n = snapshot
        .try_count(
            arkiv::entity_namespace(),
            &query,
            Budget::limit(READ_BUDGET),
        )?
        .map_err(read_error)?
        .value;
    if n > 9_007_199_254_740_991 {
        return Err(Error("ResourceLimit"));
    }
    Ok(json!(n))
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
fn read_error(error: arkiv_db::Rejected) -> Error {
    match error.reason {
        arkiv_db::Rejection::CursorMismatch => Error("CursorMismatch"),
        arkiv_db::Rejection::OutOfBudget => Error("ResourceLimit"),
        _ => Error("QueryRejected"),
    }
}
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
fn query(host: &Host, params: &Json) -> Result<Json> {
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
        return Err(Error("InvalidParams"));
    }
    let height = options
        .get("atBlock")
        .map(quantity)
        .transpose()?
        .unwrap_or(host.head.height);
    if height > host.head.height {
        return Err(Error("UnknownBlock"));
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
        return Err(Error("InvalidParams"));
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
            return Err(Error("CursorMismatch"));
        }
        let token = token
            .as_str()
            .filter(|s| s.len() == 66)
            .ok_or(Error("CursorMismatch"))?;
        let cache = CURSORS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| Error("ReadUnavailable"))?;
        let entry = cache
            .entries
            .get(token)
            .filter(|e| e.binding == binding)
            .ok_or(Error("CursorMismatch"))?;
        Some(entry.cursor.clone())
    } else {
        None
    };
    let snapshot = host.state(height)?;
    let page = read_page(
        &snapshot,
        &query,
        &host.genesis.genesis.limits,
        limit as usize,
        cursor,
    )?;
    let data = page
        .rows
        .iter()
        .map(|row| project(row, &selected))
        .collect::<Result<Vec<_>>>()?;
    let mut result = json!({"data":data,"blockNumber":format!("0x{height:x}")});
    if serde_json::to_vec(&result)
        .map_err(|_| Error("ResourceLimit"))?
        .len()
        > RESPONSE_CAP
    {
        return Err(Error("ResourceLimit"));
    }
    if let Some(cursor) = page.next {
        let mut cache = CURSORS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| Error("ReadUnavailable"))?;
        cache.serial = cache.serial.checked_add(1).ok_or(Error("ResourceLimit"))?;
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
/// Combine native pages without weakening the chain's committed read limits.
/// Larger Boolean trees use a bounded scan fallback; simple queries retain the
/// engine's indexed path. The budget is shared by all reads in this RPC.
fn read_page(
    snapshot: &arkiv_db::Snapshot,
    query: &Query,
    limits: &arkiv_db::Limits,
    limit: usize,
    mut cursor: Option<Cursor>,
) -> Result<arkiv_db::QueryPage> {
    let scan = query.groups.len() > limits.max_groups
        || query.groups.iter().any(|g| g.len() > limits.max_literals);
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
            return Err(Error("ResourceLimit"));
        }
        let metered = snapshot
            .try_query(
                arkiv::entity_namespace(),
                native,
                &Projection::ALL,
                page_size,
                cursor.as_ref(),
                Budget::limit(remaining),
            )?
            .map_err(read_error)?;
        remaining = remaining
            .checked_sub(metered.receipt.spent.units())
            .ok_or(Error("ResourceLimit"))?;
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
    Err(Error("ResourceLimit"))
}
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
            "contentType" => json!(e.content_type),
            "payload" => json!(hex(&e.payload)),
            "attributeSchema" => Json::Array(
                e.attributes
                    .iter()
                    .map(|(n, v)| scalar(v).map(|(t, _)| json!({"name":n.as_str(),"type":t})))
                    .collect::<Result<_>>()?,
            ),
            "attributes" => Json::Array(
                e.attributes
                    .iter()
                    .filter(|(n, _)| choice == true || choice[n.as_str()] == true)
                    .map(|(n, v)| {
                        scalar(v).map(|(t, v)| json!({"name":n.as_str(),"type":t,"value":v}))
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
fn bounded(groups: Vec<Vec<Literal>>) -> Result<Query> {
    if groups.len() > GROUP_CAP || groups.iter().map(Vec::len).sum::<usize>() > LITERAL_CAP {
        return Err(Error("QueryLimit"));
    }
    Ok(Query { groups })
}
fn union(mut a: Query, b: Query) -> Result<Query> {
    if a.groups.len() + b.groups.len() > GROUP_CAP {
        return Err(Error("QueryLimit"));
    }
    a.groups.extend(b.groups);
    bounded(a.groups)
}
fn intersect(a: Query, b: Query) -> Result<Query> {
    if a.groups.len().saturating_mul(b.groups.len()) > GROUP_CAP {
        return Err(Error("QueryLimit"));
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
        return Err(Error("QueryLimit"));
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
fn complement(query: Query) -> Result<Query> {
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
fn name(key: &AnnotKey) -> Result<CellName> {
    let text = match key {
        AnnotKey::User(n) => n.as_str(),
        AnnotKey::BuiltIn(b) => match b {
            BuiltIn::Owner => "$owner",
            BuiltIn::Creator => "$creator",
            BuiltIn::Key => "$key",
            BuiltIn::ExpiresAt => "$expiresAt",
            BuiltIn::CreatedAt => "$createdAt",
            BuiltIn::ContentType => "$contentType",
        },
    };
    CellName::new(text).map_err(|_| Error("InvalidQuery"))
}
fn native_value(key: &AnnotKey, value: &AttributeValue) -> Result<Value> {
    Ok(match value {
        AttributeValue::Bool(v) => Value::Bool(*v),
        AttributeValue::Int(v) => Value::I32(*v),
        AttributeValue::U64(v) => Value::U64(*v),
        AttributeValue::U256(v)
            if matches!(
                key,
                AnnotKey::BuiltIn(BuiltIn::CreatedAt | BuiltIn::ExpiresAt)
            ) =>
        {
            Value::U64(
                U256::from_be_bytes(*v)
                    .try_into()
                    .map_err(|_| Error("InvalidQuery"))?,
            )
        }
        AttributeValue::U256(v) => Value::U256(*v),
        AttributeValue::Decimal(v) => Value::Dec(*v),
        AttributeValue::Bytes32(v) => Value::Bytes32(*v),
        AttributeValue::Str(v) => Value::Str(v.as_str().into()),
        AttributeValue::EthereumAddress(v) => Value::Addr(*v),
        AttributeValue::EntityKey(v) => Value::Key(*v),
    })
}
fn leaf(ast: Ast, neq: bool) -> Result<Query> {
    let (key, value, op) = match ast {
        Ast::Eq { key, value } => (key, value, 0),
        Ast::Gt { key, value } => (key, value, 1),
        Ast::Gte { key, value } => (key, value, 2),
        Ast::Lt { key, value } => (key, value, 3),
        Ast::Lte { key, value } => (key, value, 4),
        Ast::StartsWith { key, value } => (key, value, 5),
        _ => return Err(Error("InvalidQuery")),
    };
    let n = name(&key)?;
    let v = native_value(&key, &value)?;
    let term = match op {
        0 if neq => Term::not_eq(n, v),
        0 => Term::eq(n, v),
        1 => Term::range(n, Bound::Excluded(v), Bound::Unbounded),
        2 => Term::range(n, Bound::Included(v), Bound::Unbounded),
        3 => Term::range(n, Bound::Unbounded, Bound::Excluded(v)),
        4 => Term::range(n, Bound::Unbounded, Bound::Included(v)),
        _ => match v {
            Value::Str(s) => Term::starts_with(n, &s),
            _ => return Err(Error("InvalidQuery")),
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
fn parse_query(source: &str) -> Result<Query> {
    if source.len() > 8192 {
        return Err(Error("QueryLimit"));
    }
    let tokens = lexer::tokenize(source).map_err(|_| Error("InvalidQuery"))?;
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
        return Err(Error("InvalidQuery"));
    }
    Ok(q)
}
impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|t| &t.token)
    }
    fn take(&mut self, t: Token) -> bool {
        if self.peek() == Some(&t) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Token) -> Result<()> {
        if self.take(t) {
            Ok(())
        } else {
            Err(Error("InvalidQuery"))
        }
    }
    fn or(&mut self) -> Result<Query> {
        let mut q = self.and()?;
        while self.take(Token::Or) {
            q = union(q, self.and()?)?;
        }
        Ok(q)
    }
    fn and(&mut self) -> Result<Query> {
        let mut q = self.unary()?;
        while self.take(Token::And) {
            q = intersect(q, self.unary()?)?;
        }
        Ok(q)
    }
    fn unary(&mut self) -> Result<Query> {
        if self.depth >= 32 {
            return Err(Error("QueryLimit"));
        }
        self.depth += 1;
        let result = if self.take(Token::Not) {
            complement(self.unary()?)
        } else if self.take(Token::LParen) {
            let q = self.or()?;
            self.expect(Token::RParen)?;
            Ok(q)
        } else {
            self.predicate()
        };
        self.depth -= 1;
        result
    }
    fn predicate(&mut self) -> Result<Query> {
        self.predicates += 1;
        if self.predicates > 64 {
            return Err(Error("QueryLimit"));
        }
        if matches!(self.peek(), Some(Token::Exists | Token::TypeOf)) {
            let typed = self.take(Token::TypeOf);
            if !typed {
                self.expect(Token::Exists)?;
            }
            self.expect(Token::LParen)?;
            let Some(Token::Name(n)) = self.peek().cloned() else {
                return Err(Error("InvalidQuery"));
            };
            if !arkiv::sdk_safe_name(&n) {
                return Err(Error("InvalidQuery"));
            }
            self.at += 1;
            self.expect(Token::RParen)?;
            let n = CellName::new(&n).map_err(|_| Error("InvalidQuery"))?;
            if typed {
                self.expect(Token::Eq)?;
                let Some(Token::Name(tag)) = self.peek().cloned() else {
                    return Err(Error("InvalidQuery"));
                };
                self.at += 1;
                let tag = TAGS
                    .iter()
                    .find(|(s, _)| s.eq_ignore_ascii_case(&tag))
                    .map(|(_, t)| *t)
                    .ok_or(Error("InvalidQuery"))?;
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
            let start = self.tokens.get(self.at).ok_or(Error("InvalidQuery"))?.start;
            if !matches!(self.peek(), Some(Token::Name(_) | Token::SysName(_))) {
                return Err(Error("InvalidQuery"));
            }
            let op = self.tokens.get(self.at + 1).ok_or(Error("InvalidQuery"))?;
            let neq = op.token == Token::Neq;
            let op_start = op.start;
            self.at += 3;
            if self.at > self.tokens.len() {
                return Err(Error("InvalidQuery"));
            }
            let end = self
                .tokens
                .get(self.at)
                .map(|t| t.start)
                .unwrap_or(self.source.len());
            let mut predicate = self.source[start..end].to_string();
            if neq {
                predicate.replace_range(op_start - start..op_start - start + 2, "=");
            }
            leaf(
                parse::parse(&predicate).map_err(|_| Error("InvalidQuery"))?,
                neq,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let rows = (0..4)
            .map(|n| PrefillEntity {
                creator: *signer.address().as_bytes(),
                create_nonce: n,
                salt: n as u128,
                expires_at_height: 10,
                flags: 0,
                content_type: "text/plain".into(),
                payload: format!("row{n}").into_bytes(),
                attributes: match n {
                    0 => vec![(CellName::new("n").unwrap(), Value::U64(7))],
                    1 => vec![(CellName::new("n").unwrap(), Value::U64(8))],
                    2 => vec![(CellName::new("n").unwrap(), Value::str("eight"))],
                    _ => vec![],
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
        let page = query(&host, &json!(["*", {"atBlock":"0x0","limit":200,"select":{"key":true,"owner":true,"creator":true,"createdAt":true,"updatedAt":true,"expiresAt":true,"creationFlags":true,"contentType":true,"attributes":true}}])).unwrap();
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
        let selected = json!({"key":true,"attributes":{"n":true},"payload":true,"createdAt":true});
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
