use super::{syntax::TAGS, *};
use alloy_primitives::U256;

// The reads as served under the lock: planned and run back to back, with
// no offset limit (`ReadLimits::max_offset`), so that the read budget alone
// bounds an offset walk.
fn bounds(host: &Host) -> QueryLimits {
    ReadLimits::default().query_limits(&host.genesis.genesis.limits)
}

fn entity_count(host: &Host, params: &Json) -> Read<Json> {
    plan_count(host, params, bounds(host)).and_then(Planned::run)
}

fn query(host: &Host, params: &Json) -> Read<Json> {
    plan_query(host, params, bounds(host), None).and_then(PageRead::run)
}

/// [`query`] reading engine pages of at most `max_page` rows.
fn query_in_pages(host: &Host, params: &Json, max_page: usize) -> Read<Json> {
    let bounds = QueryLimits {
        max_page,
        ..bounds(host)
    };
    plan_query(host, params, bounds, None).and_then(PageRead::run)
}

/// The SDK's `select`: every key sent as an explicit boolean.
fn sdk_select(attributes: Json) -> Json {
    json!({
        "key": true,
        "owner": true,
        "creator": true,
        "createdAt": true,
        "updatedAt": true,
        "expiresAt": true,
        "creationFlags": true,
        "attributeSchema": true,
        "attributes": attributes,
    })
}

fn kind(result: Read<Json>) -> QueryErrorKind {
    match result {
        Err(ReadError::Query(q)) => q.kind,
        other => panic!("expected a query error, got {other:?}"),
    }
}

#[test]
fn sdk_operators_and_bounds() {
    assert!(parse_query("NOT (a = i32(1) OR b = true)").is_ok());
    let q = parse_query("n != u64(7)").unwrap();
    assert!(matches!(q.query.groups[0][0].term, Term::NotEq { .. }));
    assert_eq!(
        parse_query("NOT EXISTS(name)").unwrap().query.groups[0].len(),
        TAGS.len()
    );
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
}

#[test]
fn select_accepts_exactly_the_sdk_keys() {
    assert_eq!(selection(None).unwrap().fields, json!({"key":true}));
    // `payload` and `contentType` are not select keys.
    for removed in ["payload", "contentType"] {
        for choice in [true, false] {
            let mut select = sdk_select(json!(true));
            select[removed] = json!(choice);
            assert_eq!(
                selection(Some(&select)),
                Err(ErrorCode::InvalidParams.into())
            );
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
    fixture_with(period_ms, Vec::new())
}

/// The four fixture entities, then one more per entry of `extra` holding
/// those indexed `str` values as `s0`, `s1`, ….
fn fixture_with(period_ms: u64, extra: Vec<Vec<&str>>) -> Host {
    use crate::{
        accounts::AccountConfig,
        genesis_bundle::*,
        signed::{crypto::Signer, history::History, protocol::*},
    };
    use arkiv_db::Cell;
    use arkiv_db_memory::MemoryVersionedStore;
    use std::sync::Arc;
    let signer = Signer::from_bytes([1; 32]).unwrap();
    let mut native = crate::signed::protocol::default_native_genesis(9515, 1_000, period_ms);
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
    let mut rows = (0..4)
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
    for (i, values) in extra.into_iter().enumerate() {
        rows.push(PrefillEntity {
            creator: *signer.address().as_bytes(),
            create_nonce: 4 + i as u64,
            salt: 4 + i as u128,
            expires_at_height: 10,
            flags: 0,
            attributes: values
                .into_iter()
                .enumerate()
                .map(|(j, v)| indexed(&format!("s{j}"), Value::str(v)))
                .collect(),
        });
    }
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
    Host::create(
        Arc::new(MemoryVersionedStore::new()),
        arkiv_db::durable::Options::default(),
        crate::signed::host::Launch {
            history: History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash())
                .unwrap(),
            genesis: genesis.clone(),
            source: [3; 16],
            certificate: SignedHeader::sign(&genesis, genesis.header(), &signer).unwrap(),
            signer: Some(signer),
        },
        crate::signed::host::Prefill::Stream(encode_stream(&rows).unwrap()),
    )
    .unwrap()
}

#[test]
fn str_values_with_control_characters_are_stored_and_queryable() {
    // SDK 1.0's health suite writes "line one\n\tline two"; the network
    // accepts any UTF-8, and a query literal carries the characters raw.
    let multi = "line one\n\tline two";
    let tricky = "x' -- y\nORDER BY s0:str DESC";
    let host = fixture_with(
        1_000,
        vec![vec![multi, "\u{0}nul\u{7f}"], vec![tricky, "\r\n"]],
    );
    let keys = |source: String| {
        let page = query(&host, &json!([source, {"select":sdk_select(json!(true))}])).unwrap();
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["attributes"][0]["value"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let quoted = |v: &str| format!("str('{}')", v.replace('\'', "''"));
    assert_eq!(keys(format!("s0 = {}", quoted(multi))), [multi]);
    assert_eq!(keys(format!("s0 = {}", quoted(tricky))), [tricky]);
    assert_eq!(keys("s1 = str('\u{0}nul\u{7f}')".into()), [multi]);
    assert_eq!(keys("s1 = str('\r\n')".into()), [tricky]);
    assert_eq!(keys("s0 STARTSWITH str('line one\n\t')".into()), [multi]);
    assert_eq!(keys("s0 STARTSWITH str('line one\n ')".into()), [""; 0]);
    // Control characters order by their bytes, below printable text.
    assert_eq!(keys("s1 < str(' ')".into()), [multi, tricky]);
    assert_eq!(
        keys("s0 >= str('') ORDER BY s1:str DESC".into()),
        [tricky, multi]
    );
    // The literal's own `--` and ORDER BY text stay inside the string.
    assert_eq!(
        keys(format!("s0 = {} ORDER BY s0:str ASC", quoted(tricky))),
        [tricky]
    );
    assert_eq!(
        entity_count(
            &host,
            &json!([{"query": format!("s0 = {}", quoted(multi))}])
        )
        .unwrap(),
        json!(1)
    );
}

fn get_entity(host: &Host, params: &Json) -> Read<Json> {
    plan_entity(host, params, bounds(host)).and_then(|page| Planned::Entity(Box::new(page)).run())
}

#[test]
fn get_entity_returns_the_full_live_entity_or_null() {
    let mut host = fixture();
    let page = query(
        &host,
        &json!(["n = u64(7)", {"select":sdk_select(json!(true))}]),
    )
    .unwrap();
    let mut expected = page["data"][0].clone();
    let key = expected["key"].as_str().unwrap().to_owned();
    expected.as_object_mut().unwrap().remove("attributeSchema");
    let entity = get_entity(&host, &json!([key])).unwrap();
    assert_eq!(entity, expected);
    assert!(entity.get("attributeSchema").is_none());
    // Unindexed attributes are returned too.
    assert_eq!(
        entity["attributes"],
        json!([
            {"name": "n", "type": "u64", "value": "0x7"},
            {"name": "note", "type": "str", "value": "seven"},
        ])
    );
    // Keys are case-insensitive hex; an unknown key is `null`.
    assert_eq!(
        get_entity(
            &host,
            &json!([key.to_ascii_uppercase().replacen("0X", "0x", 1)])
        )
        .unwrap(),
        expected
    );
    assert_eq!(
        get_entity(&host, &json!([format!("0x{}", "00".repeat(32))])).unwrap(),
        Json::Null
    );
    // Past its expiry the entity is gone at the head but still readable at
    // a retained block, given as a number or a hex quantity.
    for _ in 0..10 {
        host.produce().unwrap();
    }
    assert_eq!(get_entity(&host, &json!([key])).unwrap(), Json::Null);
    assert_eq!(get_entity(&host, &json!([key, 0])).unwrap(), expected);
    assert_eq!(get_entity(&host, &json!([key, "0x0"])).unwrap(), expected);
    assert_eq!(get_entity(&host, &json!([key, null])).unwrap(), Json::Null);
    assert!(get_entity(&host, &json!([key, 11])).is_err());
    for params in [
        json!([]),
        json!(null),
        json!([key, 0, 0]),
        json!(["0x1234"]),
        json!([key.trim_start_matches("0x")]),
        json!([format!("0x{}", "zz".repeat(32))]),
        json!([format!("{key} OR n = u64(8)")]),
        json!([7]),
    ] {
        assert!(get_entity(&host, &params).is_err(), "{params}");
    }
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
    let page = query(
        &host,
        &json!([
            "*",
            {
                "atBlock": "0x0",
                "limit": 200,
                "select": {
                    "key": true,
                    "owner": true,
                    "creator": true,
                    "createdAt": true,
                    "updatedAt": true,
                    "expiresAt": true,
                    "creationFlags": true,
                    "attributes": true,
                },
            },
        ]),
    )
    .unwrap();
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
    let last = query(
        &host,
        &json!([
            "EXISTS(n)",
            {"limit": "0x1", "select": selected, "cursor": second["cursor"], "atBlock": "0x0"},
        ]),
    )
    .unwrap();
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
}

#[test]
fn read_limits_refuse_bounds_outside_their_range() {
    let defaults = ReadLimits::default();
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
            {"name":"blob", "type":"bytes", "indexed":false},
            {"name":"n", "type":"u64", "indexed":false},
        ])
    );
    assert_eq!(
        unindexed[0]["attributes"],
        json!([
            {"name":"blob", "type":"bytes", "value":"0xabcd"},
            {"name":"n", "type":"u64", "value":"0x7"},
        ])
    );
    let indexed = page("n = u64(7)");
    assert_eq!(
        indexed[0]["attributeSchema"],
        json!([
            {"name":"n", "type":"u64", "indexed":true},
            {"name":"note", "type":"str", "indexed":false},
        ])
    );
    // The SDK's `RpcEntity` keys and nothing else.
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
        ("n STARTSWITH u64(1)", QueryErrorKind::Type),
        ("n BETWEEN u64(1) AND i32(2)", QueryErrorKind::Type),
        ("n BETWEEN u64(1) OR u64(2)", QueryErrorKind::Parse),
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
        Err(ReadError::Node(Error {
            code: ErrorCode::InvalidParams,
            ..
        }))
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
        genesis: crate::signed::protocol::default_native_genesis(9515, 1_000, 1_000),
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
        arkiv_db::durable::Options::default(),
        crate::signed::host::Launch {
            history: History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash())
                .unwrap(),
            genesis: genesis.clone(),
            source: [4; 16],
            certificate: SignedHeader::sign(&genesis, genesis.header(), &signer).unwrap(),
            signer: Some(signer),
        },
        crate::signed::host::Prefill::None,
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
            assert_eq!(e, Error::from(ErrorCode::ReadRejected));
            None
        }
    };
    use QueryErrorKind as K;
    for (reason, kind) in [
        (R::CursorMismatch, Some(K::Cursor)),
        (R::OutOfBudget, Some(K::Limits)),
        (R::InvalidQuery(F::TooManyGroups), Some(K::Limits)),
        (R::InvalidQuery(F::TooManyLiterals), Some(K::Limits)),
        (R::InvalidQuery(F::ValueTooLarge), Some(K::Literal)),
        (R::InvalidQuery(F::NotIndexable), Some(K::Type)),
        (R::InvalidQuery(F::NotOrdered), Some(K::Type)),
        (R::InvalidQuery(F::MixedBounds), Some(K::Type)),
        (R::InvalidQuery(F::UnboundedRange), Some(K::Type)),
        // No fault of the query: a server error, never a query kind.
        (R::Unauthorized, None),
        (R::CostOverflow, None),
        (R::InvalidQuery(F::NoGroups), None),
        (R::InvalidQuery(F::ProjectionTooLarge), None),
    ] {
        assert_eq!(kind_of(reason), kind);
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
    // A sorted page has no cursor to carry cut rows: past the cap it is
    // refused, and a window that fits is served whole.
    let sorted = |limit: u64| {
        query(
            &host,
            &json!([
                "big = true ORDER BY $key",
                {"atBlock": at, "limit": limit, "select": sdk_select(json!(true))},
            ]),
        )
    };
    assert_eq!(kind(sorted(200)), QueryErrorKind::Limits);
    let window = sorted(20).unwrap();
    assert_eq!(window["data"].as_array().unwrap().len(), 20);
    assert!(window.get("cursor").is_none());
}

#[test]
fn rows_within_counts_the_rows_that_fit_the_budget() {
    assert_eq!(rows_within(&[json!("x".repeat(64))], 16).unwrap(), 0);
    assert_eq!(rows_within(&[json!(1), json!(2)], 5).unwrap(), 2);
    assert_eq!(rows_within(&[json!(1), json!(2)], 4).unwrap(), 1);
}

/// Submits one signed transaction of `ops` from the fixture's signer.
fn submit(host: &mut Host, nonce: &mut u64, ops: Vec<arkiv::ArkivOperation>) {
    use crate::{
        TxEnvelope, TxOp,
        signed::{crypto::Signer, protocol::SignedTx},
    };
    let signer = Signer::from_bytes([1; 32]).unwrap();
    let mut request_id = [0x66; 32];
    request_id[24..].copy_from_slice(&nonce.to_be_bytes());
    let tx = TxEnvelope {
        chain_id: host.genesis.genesis.chain_id,
        actor: signer.address(),
        request_id,
        tx_budget: 2_000_000,
        ops: ops
            .into_iter()
            .map(|operation| TxOp::Arkiv {
                namespace: arkiv::entity_namespace(),
                operation,
            })
            .collect(),
    };
    host.submit(SignedTx::sign(&host.genesis, *nonce, tx, &signer).unwrap())
        .unwrap();
    *nonce += 1;
    // The pool holds a few pending transactions per account.
    if nonce.is_multiple_of(4) {
        host.produce().unwrap();
    }
}

fn create(salt: u128, attributes: Vec<(&str, arkiv_db::Cell)>) -> arkiv::ArkivOperation {
    arkiv::ArkivOperation::Create {
        salt,
        flags: 0,
        expiry: crate::Expiry {
            expires_at: 0,
            min_lifetime: 100,
        },
        attributes: attributes
            .into_iter()
            .map(|(n, c)| (CellName::new(n).unwrap(), c))
            .collect(),
    }
}

fn delete(key: &Json) -> arkiv::ArkivOperation {
    let bytes = crate::simulator::wire::unhex(key.as_str().unwrap(), 32).unwrap();
    arkiv::ArkivOperation::Delete {
        key: RecordKey::new(&bytes).unwrap(),
    }
}

/// The keys of a page.
fn keys(page: &Json) -> Vec<Json> {
    page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].clone())
        .collect()
}

/// Every page of `source` with `options`, following cursors at the
/// first page's block: the keys and the pages.
fn walk(host: &Host, source: &str, options: Json) -> (Vec<Json>, Vec<Json>) {
    let first = query(host, &json!([source, options])).unwrap();
    let mut all = keys(&first);
    let mut pages = vec![first.clone()];
    let mut cursor = first.get("cursor").cloned();
    while let Some(token) = cursor {
        let mut more = options.clone();
        more["cursor"] = token;
        more["atBlock"] = first["blockNumber"].clone();
        let page = query(host, &json!([source, more])).unwrap();
        all.extend(keys(&page));
        cursor = page.get("cursor").cloned();
        pages.push(page);
    }
    (all, pages)
}

#[test]
fn offset_skips_matches_in_creation_order() {
    let host = fixture();
    let every = keys(&query(&host, &json!(["*", {"limit": 10}])).unwrap());
    assert_eq!(every.len(), 4);
    // In pages of two (the fixture's genesis page size), offsets of three
    // or more skip over more than one engine page.
    for offset in 0..6u64 {
        for limit in 1..4usize {
            for spelling in [json!(offset), json!(format!("0x{offset:x}"))] {
                let params = json!(["*", {"limit": limit, "offset": spelling}]);
                let page = query(&host, &params).unwrap();
                let paged = query_in_pages(&host, &params, 2).unwrap();
                assert_eq!(page["data"], paged["data"]);
                assert_eq!(page.get("cursor").is_some(), paged.get("cursor").is_some());
                let from = (offset as usize).min(4);
                let to = (from + limit).min(4);
                assert_eq!(keys(&page), every[from..to], "{offset} {limit}");
                // A cursor exactly when matches remain after the page.
                assert_eq!(page.get("cursor").is_some(), to < 4, "{offset} {limit}");
            }
        }
    }
    // An offset at or past the end: an empty page without a cursor.
    assert_eq!(
        query(&host, &json!(["*", {"offset": "0x4"}])).unwrap(),
        json!({"data":[],"blockNumber":"0x0"})
    );
    assert_eq!(
        query(&host, &json!(["*", {"offset": u64::MAX}])).unwrap()["data"],
        json!([])
    );
    // A filtered query skips its own matches: rows 0, 1 and 2.
    let page = query(&host, &json!(["EXISTS(n)", {"offset": 2}])).unwrap();
    assert_eq!(keys(&page), every[2..3]);
    // The cursor of an offset page continues past it; an offset beside
    // a cursor is ignored, since the cursor carries the position.
    let first = query(&host, &json!(["*", {"limit": 1, "offset": 1}])).unwrap();
    let next = |extra: Json| {
        let mut options = json!({"limit": 2, "atBlock": "0x0", "cursor": first["cursor"]});
        if let Some(offset) = extra.get("offset") {
            options["offset"] = offset.clone();
        }
        query(&host, &json!(["*", options])).unwrap()
    };
    assert_eq!(keys(&next(json!({}))), every[2..4]);
    assert_eq!(next(json!({"offset": 3})), next(json!({})));
    assert!(next(json!({})).get("cursor").is_none());
    // The scan fallback (nine literals) skips its own matches too.
    let nine = |last: &str| {
        std::iter::repeat_n("$createdAt = u64(0)", 8)
            .chain([last])
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let scanned = nine("EXISTS(n)");
    assert_eq!(
        keys(&query(&host, &json!([scanned, {"offset": 1, "limit": 1}])).unwrap()),
        every[1..2]
    );
    let (walked, _) = walk(&host, &scanned, json!({"offset": 1, "limit": 1}));
    assert_eq!(walked, every[1..3]);
    // Malformed offsets are invalid params.
    for offset in [
        json!(-1),
        json!(0.5),
        json!("0x"),
        json!("1"),
        json!("0x01"),
        json!(null),
    ] {
        assert!(
            matches!(
                query(&host, &json!(["*", {"offset": offset}])),
                Err(ReadError::Node(Error {
                    code: ErrorCode::InvalidParams,
                    ..
                }))
            ),
            "{offset}"
        );
    }
}

#[test]
fn select_count_is_the_match_set_on_every_page() {
    let host = fixture();
    let sdk = |count: bool| {
        let mut select = sdk_select(json!(true));
        select["count"] = json!(count);
        select
    };
    // Beside entity fields: the page, and the count of every match.
    let page = query(
        &host,
        &json!(["EXISTS(n)", {"select": sdk(true), "limit": 1}]),
    )
    .unwrap();
    assert_eq!(page["count"], "0x3");
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    // Repeated, unchanged, on every page; independent of limit and offset.
    let (_, pages) = walk(&host, "EXISTS(n)", json!({"select": sdk(true), "limit": 1}));
    assert_eq!(pages.len(), 3);
    assert!(pages.iter().all(|p| p["count"] == "0x3"));
    let offset = query(
        &host,
        &json!(["EXISTS(n)", {"select": sdk(true), "offset": 9}]),
    )
    .unwrap();
    assert_eq!(offset, json!({"data":[],"blockNumber":"0x0","count":"0x3"}));
    // Unselected or false: no count at all.
    for select in [sdk(false), json!({"key": true})] {
        let page = query(&host, &json!(["*", {"select": select}])).unwrap();
        assert!(page.get("count").is_none());
    }
    // Alone (the SDK's client.count() sends exactly this), or beside
    // fields all false: the count is the answer, with no rows or cursor.
    for select in [
        json!({"count": true}),
        json!({"key": false, "count": true}),
        json!({
            "key": false,
            "owner": false,
            "creator": false,
            "createdAt": false,
            "updatedAt": false,
            "expiresAt": false,
            "creationFlags": false,
            "attributeSchema": false,
            "attributes": false,
            "count": true,
        }),
    ] {
        assert_eq!(
            query(&host, &json!(["*", {"select": select, "limit": 1}])).unwrap(),
            json!({"data":[],"blockNumber":"0x0","count":"0x4"}),
        );
    }
    assert_eq!(
        query(&host, &json!(["n = u64(7)", {"select": {"count": true}}])).unwrap()["count"],
        "0x1"
    );
    // A count reads ORDER BY and ignores it, here and in getEntityCount.
    assert_eq!(
        query(
            &host,
            &json!(["EXISTS(n) ORDER BY n:u64 DESC", {"select": {"count": true}}])
        )
        .unwrap()["count"],
        "0x3"
    );
    assert_eq!(
        entity_count(&host, &json!([{"query": "TYPEOF(n) = u64 ORDER BY n:u64"}])).unwrap(),
        json!(2)
    );
    // `count` must be a boolean; a chain without entities counts zero.
    assert!(selection(Some(&json!({"count": 1}))).is_err());
    assert_eq!(
        query(&bare_fixture(), &json!(["*", {"select": {"count": true}}])).unwrap(),
        json!({"data":[],"blockNumber":"0x0","count":"0x0"})
    );
    // Beyond the read bounds (EXISTS without a type is nine groups) the
    // count scans, as its page does, where getEntityCount refuses.
    let nine = std::iter::repeat_n("$createdAt = u64(0)", 9)
        .collect::<Vec<_>>()
        .join(" AND ");
    assert_eq!(
        query(&host, &json!([nine, {"select": {"count": true}}])).unwrap()["count"],
        "0x4"
    );
    assert_eq!(
        kind(entity_count(&host, &json!([{"query": nine}]))),
        QueryErrorKind::Limits
    );
    assert_eq!(
        query(
            &host,
            &json!(["NOT EXISTS(n)", {"select": {"count": true}}])
        )
        .unwrap()["count"],
        "0x1"
    );
}

#[test]
fn order_by_sorts_by_value_with_missing_values_below_and_ties_in_creation_order() {
    let host = fixture();
    let every = keys(&query(&host, &json!(["*", {"limit": 10}])).unwrap());
    let sorted = |source: &str| keys(&query(&host, &json!([source, {"limit": 10}])).unwrap());
    let rows = |ids: &[usize]| ids.iter().map(|&i| every[i].clone()).collect::<Vec<_>>();
    // n: row 0 holds 7, row 1 holds 8, row 2 a str, row 3 an unindexed 7.
    // The sort reads row 3's 7 though no predicate can; row 2 has no
    // u64 and sorts below every value: first ASC, last DESC. The two 7s
    // tie and come in creation order either way.
    assert_eq!(sorted("* ORDER BY n:u64"), rows(&[2, 0, 3, 1]));
    assert_eq!(sorted("* ORDER BY n:u64 ASC"), rows(&[2, 0, 3, 1]));
    assert_eq!(sorted("* order by n : U64 asc"), rows(&[2, 0, 3, 1]));
    assert_eq!(sorted("* ORDER BY n:u64 DESC"), rows(&[1, 0, 3, 2]));
    assert_eq!(sorted("* ORDER BY n:str DESC"), rows(&[2, 0, 1, 3]));
    assert_eq!(sorted("EXISTS(n) ORDER BY n:u64 DESC"), rows(&[1, 0, 2]));
    assert_eq!(sorted("* ORDER BY missing:i32 DESC"), every);
    // System attributes have their type; $createdAt ties everywhere here.
    let mut by_key = every.clone();
    by_key.sort_by_key(|k| k.as_str().unwrap().to_owned());
    assert_eq!(sorted("* ORDER BY $key"), by_key);
    assert_eq!(sorted("* ORDER BY $key:key ASC"), by_key);
    by_key.reverse();
    assert_eq!(sorted("* ORDER BY $key DESC"), by_key);
    assert_eq!(sorted("* ORDER BY $createdAt DESC"), every);
    assert_eq!(sorted("* ORDER BY $owner:addr"), every);
    assert_eq!(sorted("* ORDER BY $expiresAt:u64"), every);
    // Its words are read where the clause stands, nowhere else.
    assert_eq!(
        sorted("n = u64(8) OR desc = u64(1) ORDER BY desc:u64 DESC"),
        rows(&[1])
    );
    // Every window of the order, in both directions: a sorted page is an
    // offset window and carries no cursor, whatever follows it.
    for (source, order) in [
        ("* ORDER BY n:u64", [2, 0, 3, 1]),
        ("* ORDER BY n:u64 DESC", [1, 0, 3, 2]),
    ] {
        for offset in 0..5usize {
            for limit in 1..4usize {
                let page =
                    query(&host, &json!([source, {"limit": limit, "offset": offset}])).unwrap();
                let from = offset.min(4);
                let to = (from + limit).min(4);
                assert_eq!(keys(&page), rows(&order[from..to]), "{source} {offset}");
                assert!(page.get("cursor").is_none());
            }
        }
        // Offset windows at a pinned block, with the count, partition
        // the order.
        let mut walked = Vec::new();
        for offset in (0..6).step_by(2) {
            let page = query(
                &host,
                &json!([
                    source,
                    {
                        "limit": 2,
                        "offset": offset,
                        "atBlock": "0x0",
                        "select": {"key": true, "count": true},
                    },
                ]),
            )
            .unwrap();
            assert_eq!(page["count"], "0x4");
            walked.extend(keys(&page));
        }
        assert_eq!(walked, rows(&order), "{source}");
        // The sorted page is the full entity, projected as selected.
        let page = query(
            &host,
            &json!([source, {"limit": 1, "select": sdk_select(json!(true)), "offset": 3}]),
        )
        .unwrap();
        let expected = query(
            &host,
            &json!([
                format!("$key = key({})", every[order[3]].as_str().unwrap()),
                {"select": sdk_select(json!(true))},
            ]),
        )
        .unwrap();
        assert_eq!(page["data"], expected["data"]);
    }
    // The scan fallback sorts its matches the same way.
    let nine = std::iter::repeat_n("$createdAt = u64(0)", 8)
        .chain(["EXISTS(n)"])
        .collect::<Vec<_>>()
        .join(" AND ");
    let scanned = format!("{nine} ORDER BY n:u64 DESC");
    assert_eq!(sorted(&scanned), rows(&[1, 0, 2]));
    assert_eq!(
        keys(&query(&host, &json!([scanned, {"limit": 1, "offset": 2}])).unwrap()),
        rows(&[2])
    );
    // ORDER BY takes no cursor: not one of an unsorted page, nor a made-up
    // one, nor anything else.
    let first = query(&host, &json!(["*", {"limit": 1}])).unwrap();
    for cursor in [first["cursor"].clone(), json!("0x00"), json!(7)] {
        let error = match query(
            &host,
            &json!(["* ORDER BY n:u64", {"limit": 1, "atBlock": "0x0", "cursor": cursor}]),
        ) {
            Err(ReadError::Query(q)) => q,
            other => panic!("{other:?}"),
        };
        assert_eq!(error.kind, QueryErrorKind::Cursor);
        assert!(error.message.contains("offset"), "{}", error.message);
    }
}

#[test]
fn order_by_errors_use_the_sdk_code_table() {
    let host = fixture();
    for (source, expected) in [
        ("* ORDER BY n", QueryErrorKind::Type),
        ("* ORDER BY n:bytes", QueryErrorKind::Type),
        ("* ORDER BY n:nope", QueryErrorKind::Type),
        ("* ORDER BY n:str('x')", QueryErrorKind::Type),
        ("* ORDER BY $owner:u64", QueryErrorKind::Type),
        ("* ORDER BY $updatedAt", QueryErrorKind::Type),
        ("* ORDER BY $payload", QueryErrorKind::Type),
        ("* ORDER", QueryErrorKind::Parse),
        ("* ORDER n:u64", QueryErrorKind::Parse),
        ("* ORDER BY", QueryErrorKind::Parse),
        ("* ORDER BY 5:i32", QueryErrorKind::Parse),
        ("* ORDER BY n:", QueryErrorKind::Parse),
        ("* ORDER BY n::u64", QueryErrorKind::Parse),
        ("* ORDER BY n:u64 sideways", QueryErrorKind::Parse),
        ("* ORDER BY n:u64 DESC n", QueryErrorKind::Parse),
        ("* ORDER BY n:u64 AND a = 1", QueryErrorKind::Parse),
        ("* ORDER BY n:u64 ORDER BY n:u64", QueryErrorKind::Parse),
        ("(n = u64(1) ORDER BY n:u64)", QueryErrorKind::Parse),
        ("* AND n = u64(1)", QueryErrorKind::Parse),
    ] {
        assert_eq!(kind(query(&host, &json!([source]))), expected, "{source}");
    }
    let error = match query(&host, &json!(["* ORDER BY rank"])) {
        Err(ReadError::Query(q)) => q,
        other => panic!("{other:?}"),
    };
    assert!(
        error.message.contains("ORDER BY rank:str"),
        "{}",
        error.message
    );
    assert_eq!(error.to_json()["data"]["position"], "* ORDER BY ".len());
    assert_eq!(error.to_json()["code"], -32002);
}

#[test]
fn between_and_ranges_take_every_indexable_type() {
    let host = fixture();
    let every = keys(&query(&host, &json!(["*", {"limit": 10}])).unwrap());
    let count = |source: &str| {
        query(&host, &json!([source])).unwrap()["data"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(count("n BETWEEN u64(7) AND u64(8)"), 2);
    assert_eq!(count("n BETWEEN u64(8) AND u64(8)"), 1);
    assert_eq!(count("n between u64(8) and u64(7)"), 0);
    assert_eq!(count("n BETWEEN u64(7) AND u64(8) AND n = u64(8)"), 1);
    assert_eq!(count("NOT n BETWEEN u64(7) AND u64(8)"), 2);
    assert_eq!(count("n BETWEEN str('a') AND str('f')"), 1);
    assert_eq!(count("n > str('a')"), 1);
    assert_eq!(count("n < str('a')"), 0);
    let mut sorted = every;
    sorted.sort_by_key(|k| k.as_str().unwrap().to_owned());
    let (low, high) = (sorted[1].as_str().unwrap(), sorted[2].as_str().unwrap());
    assert_eq!(count(&format!("$key > key({low})")), 2);
    assert_eq!(
        count(&format!("$key BETWEEN key({low}) AND key({high})")),
        2
    );
    assert_eq!(
        entity_count(&host, &json!([{"query": "n BETWEEN u64(7) AND u64(8)"}])).unwrap(),
        json!(2)
    );
    assert_eq!(count("$expiresAt BETWEEN u64(10) AND u64(10)"), 4);
}

#[test]
fn offset_sort_and_count_walks_are_charged_to_the_read_budget() {
    let mut host = fixture();
    let mut nonce = 0;
    for i in 0..40u64 {
        submit(
            &mut host,
            &mut nonce,
            vec![create(
                1000 + u128::from(i),
                vec![("w", arkiv_db::Cell::attribute(Value::U64(i % 7)))],
            )],
        );
    }
    host.produce().unwrap();
    let run = |source: &str, options: Json, budget: u64| {
        plan_query(&host, &json!([source, options]), bounds(&host), None)
            .and_then(|page| page.run_within(budget))
    };
    // The least budget each read is served under: a walk past 39
    // matches, or through all 40 to sort them, costs more than a page of
    // one, and is refused as `limits` under that page's budget.
    let least = |source: &str, options: Json| {
        let (mut low, mut high) = (1, READ_BUDGET);
        assert!(run(source, options.clone(), high).is_ok());
        while low < high {
            let mid = low + (high - low) / 2;
            match run(source, options.clone(), mid) {
                Ok(_) => high = mid,
                Err(error) => {
                    assert_eq!(kind(Err(error)), QueryErrorKind::Limits);
                    low = mid + 1;
                }
            }
        }
        low
    };
    let typed = "TYPEOF(w) = u64";
    let page = least(typed, json!({"limit": 1}));
    let skip = least(typed, json!({"limit": 1, "offset": 39}));
    let sort = least("TYPEOF(w) = u64 ORDER BY w:u64", json!({"limit": 1}));
    let count = least(
        typed,
        json!({"limit": 1, "select": {"key": true, "count": true}}),
    );
    assert!(skip > page + 39, "{skip} {page}");
    assert!(sort > page + 40 * 3, "{sort} {page}");
    assert!(count > page, "{count} {page}");
    for (source, options) in [
        (typed, json!({"limit": 1, "offset": 39})),
        ("TYPEOF(w) = u64 ORDER BY w:u64", json!({"limit": 1})),
    ] {
        assert_eq!(kind(run(source, options, page)), QueryErrorKind::Limits);
    }
    // Under the real budget both answer, and agree with a full walk.
    let every = keys(&run("TYPEOF(w) = u64", json!({"limit": 40}), READ_BUDGET).unwrap());
    assert_eq!(
        keys(
            &run(
                "TYPEOF(w) = u64",
                json!({"limit": 1, "offset": 39}),
                READ_BUDGET
            )
            .unwrap()
        ),
        every[39..]
    );
    let sorted = keys(
        &run(
            "TYPEOF(w) = u64 ORDER BY w:u64 DESC",
            json!({"limit": 40}),
            READ_BUDGET,
        )
        .unwrap(),
    );
    let mut expected: Vec<(u64, usize)> = (0..40).map(|i| (i % 7, i as usize)).collect();
    expected.sort_by_key(|&(w, i)| (std::cmp::Reverse(w), i));
    assert_eq!(
        sorted,
        expected
            .iter()
            .map(|&(_, i)| every[i].clone())
            .collect::<Vec<_>>()
    );
}

/// The fixture with 40 more entities: `w` is `i % 7` and `v` is `i`, so a
/// range on `v` is a run of one term per match.
fn walk_fixture() -> Host {
    let mut host = fixture();
    let mut nonce = 0;
    for i in 0..40u64 {
        submit(
            &mut host,
            &mut nonce,
            vec![create(
                1000 + u128::from(i),
                vec![
                    ("w", arkiv_db::Cell::attribute(Value::U64(i % 7))),
                    ("v", arkiv_db::Cell::attribute(Value::U64(i))),
                ],
            )],
        );
    }
    host.produce().unwrap();
    host
}

/// The least budget `params` is served under with engine pages of
/// `max_page` rows; below it the read is refused as `limits`.
fn least_budget(host: &Host, params: &Json, max_page: usize) -> u64 {
    let bounds = QueryLimits {
        max_page,
        ..bounds(host)
    };
    let run =
        |budget| plan_query(host, params, bounds, None).and_then(|page| page.run_within(budget));
    let (mut low, mut high) = (1, READ_BUDGET);
    assert!(run(high).is_ok());
    while low < high {
        let mid = low + (high - low) / 2;
        match run(mid) {
            Ok(_) => high = mid,
            Err(error) => {
                assert_eq!(kind(Err(error)), QueryErrorKind::Limits);
                low = mid + 1;
            }
        }
    }
    low
}

#[test]
fn walk_pages_change_charges_not_answers() {
    let host = walk_fixture();
    let sources = [
        "*",
        "TYPEOF(w) = u64",
        "v >= u64(3)",
        "v >= u64(3) AND w = u64(2)",
        "v >= u64(3) ORDER BY w:u64 DESC",
        "TYPEOF(w) = u64 ORDER BY v:u64",
        "v >= u64(3) AND w != u64(2) ORDER BY v:u64 DESC",
    ];
    for source in sources {
        for (offset, limit) in [(0, 200), (0, 3), (5, 3), (13, 7), (36, 5), (60, 2)] {
            let params = json!([source, {"limit": limit, "offset": offset, "select": {
                "key": true, "attributes": true, "count": true,
            }}]);
            let wide = query(&host, &params).unwrap();
            let narrow = query_in_pages(&host, &params, 2).unwrap();
            assert_eq!(wide["data"], narrow["data"], "{source} {offset} {limit}");
            assert_eq!(wide["count"], narrow["count"]);
            assert_eq!(wide.get("cursor").is_some(), narrow.get("cursor").is_some());
        }
    }
    // A range drives its group as a run, planned again for every engine
    // page: sorting its 37 matches in pages of two plans it 19 times.
    for source in [
        "v >= u64(3) ORDER BY w:u64 DESC",
        "v >= u64(3) AND w != u64(2) ORDER BY v:u64",
    ] {
        let params = json!([source, {"limit": 1}]);
        let (wide, narrow) = (
            least_budget(&host, &params, ReadLimits::WALK_PAGE),
            least_budget(&host, &params, 2),
        );
        assert!(2 * wide < narrow, "{source}: {wide} {narrow}");
    }
    let params = json!(["v >= u64(3)", {"limit": 1, "offset": 30}]);
    let (wide, narrow) = (
        least_budget(&host, &params, ReadLimits::WALK_PAGE),
        least_budget(&host, &params, 2),
    );
    assert!(2 * wide < narrow, "offset: {wide} {narrow}");
}

/// The page's walk alone, without the up-front check: what the read did
/// under `budget` before the check existed.
fn walk_alone(host: &Host, params: &Json, budget: u64) -> Read<()> {
    let page = plan_query(host, params, bounds(host), None)?;
    let window = Window {
        snapshot: &page.snapshot,
        query: &page.query,
        limits: &page.limits,
        bounds: page.bounds,
        limit: page.limit,
        offset: page.offset,
    };
    let mut budget = budget;
    match &page.order {
        None => read_page(&window, None, &mut budget).map(|_| ()),
        Some(order) => sorted_page(&window, order, &mut budget).map(|_| ()),
    }
}

/// Whether the up-front check refuses `params` under `budget`.
fn refused_up_front(host: &Host, params: &Json, budget: u64) -> Option<QueryError> {
    let page = plan_query(host, params, bounds(host), None).unwrap();
    unaffordable(
        &page.snapshot,
        &page.query,
        page.order.as_ref(),
        page.offset,
        page.bounds,
        budget,
    )
}

#[test]
fn walks_the_budget_cannot_finish_are_refused_before_they_start() {
    let host = walk_fixture();
    let pricing = host.genesis.genesis.pricing.clone();
    let skip = least_per_match(&pricing, 0);
    let sort = least_per_match(&pricing, KEY_BYTES);
    // The GLM schedule: ResultRow 10, a posting entry 10, ReadByte 1.
    assert_eq!((skip, sort), (20, 52));
    let run = |params: &Json, budget: u64| {
        plan_query(&host, params, bounds(&host), None).and_then(|page| page.run_within(budget))
    };
    let message = |result: Read<Json>| match result {
        Err(ReadError::Query(q)) => {
            assert_eq!(q.kind, QueryErrorKind::Limits);
            q.message
        }
        other => panic!("expected a limits error, got {other:?}"),
    };
    // An offset past 39 of the 40 matches, which the budget cannot pay
    // for at 20 units each, is refused naming the walk, not by exhaustion.
    let offset = json!(["TYPEOF(w) = u64", {"limit": 1, "offset": 39}]);
    assert_eq!(
        message(run(&offset, 39 * skip - 1)),
        "offset 39 passes 39 matches; the read budget walks at most 38: continue from an \
         earlier page's cursor (with its atBlock) or narrow the query"
    );
    // An offset past the end walks only the matches there are.
    let past = json!(["TYPEOF(w) = u64", {"limit": 1, "offset": u64::MAX}]);
    assert_eq!(
        message(run(&past, 40 * skip - 1)),
        format!(
            "offset {} passes 40 matches; the read budget walks at most 39: continue from \
             an earlier page's cursor (with its atBlock) or narrow the query",
            u64::MAX
        )
    );
    assert_eq!(
        run(&past, READ_BUDGET).unwrap(),
        json!({"data": [], "blockNumber": format!("0x{:x}", host.head.height)})
    );
    let sorted = json!(["TYPEOF(w) = u64 ORDER BY v:u64 DESC", {"limit": 1}]);
    assert_eq!(
        message(run(&sorted, 40 * sort - 1)),
        "ORDER BY sorts all 40 matches; the read budget sorts at most 39: narrow the query, \
         or drop ORDER BY and page with the cursor"
    );
    // Neither a page without an offset nor a count-only read is checked.
    for params in [
        json!(["TYPEOF(w) = u64", {"limit": 1}]),
        json!(["TYPEOF(w) = u64 ORDER BY v:u64", {"offset": 39, "select": {"count": true}}]),
    ] {
        assert!(run(&params, 5_000).is_ok(), "{params}");
    }
    // Only reads the budget would have ended anyway are refused: wherever
    // the check refuses, the walk alone runs out under the same budget.
    let mut refusals = 0;
    for params in [
        offset,
        past,
        sorted,
        json!(["v >= u64(3) AND w != u64(2) ORDER BY v:u64", {"limit": 2, "offset": 4}]),
        json!(["*", {"limit": 2, "offset": 41}]),
        json!(["v >= u64(3)", {"limit": 2, "offset": 30}]),
    ] {
        for budget in (1..4_000).step_by(37) {
            if refused_up_front(&host, &params, budget).is_some() {
                refusals += 1;
                assert_eq!(
                    kind(walk_alone(&host, &params, budget).map(|()| json!(null))),
                    QueryErrorKind::Limits,
                    "{params} {budget}"
                );
            }
        }
    }
    assert!(refusals > 100, "{refusals}");
}

#[test]
fn offsets_above_the_node_limit_are_refused_before_planning() {
    let host = walk_fixture();
    let run = |params: Json, max_offset: Option<u64>| {
        let reads = ReadLimits {
            max_offset,
            ..ReadLimits::default()
        };
        plan(&host, "arkiv_query", &params, reads)
            .unwrap()
            .and_then(Planned::run)
    };
    assert_eq!(ReadLimits::default().max_offset, Some(100_000));
    let typed = "TYPEOF(w) = u64";
    let every = keys(&run(json!([typed, {"limit": 40}]), Some(39)).unwrap());
    // At the limit the page is served.
    assert_eq!(
        keys(&run(json!([typed, {"limit": 1, "offset": 39}]), Some(39)).unwrap()),
        every[39..]
    );
    // Above it, sorted or not, the request is refused naming the limit.
    for source in [typed, "TYPEOF(w) = u64 ORDER BY v:u64"] {
        match run(json!([source, {"limit": 1, "offset": "0x28"}]), Some(39)) {
            Err(ReadError::Query(q)) => {
                assert_eq!(q.kind, QueryErrorKind::Limits);
                assert_eq!(
                    q.message,
                    "offset 40 exceeds this node's limit of 39 (--read-max-offset): page with \
                     the cursor of the previous page and its blockNumber as atBlock, or narrow \
                     the query"
                );
            }
            other => panic!("expected a limits error, got {other:?}"),
        }
    }
    // The default limit: an offset of 100,000 runs (past the end here),
    // one more is refused.
    let default = ReadLimits::default().max_offset;
    assert_eq!(
        run(json!(["*", {"offset": 100_000}]), default).unwrap()["data"],
        json!([])
    );
    assert_eq!(
        kind(run(json!(["*", {"offset": 100_001}]), default)),
        QueryErrorKind::Limits
    );
    // Without a limit the read budget alone bounds the walk, as before.
    assert_eq!(
        run(json!(["*", {"offset": u64::MAX}]), None).unwrap()["data"],
        json!([])
    );
    // An offset beside a cursor is ignored, whatever its size.
    let first = run(json!([typed, {"limit": 1}]), Some(39)).unwrap();
    let next = run(
        json!([typed, {
            "limit": 1,
            "offset": 1_000,
            "atBlock": first["blockNumber"],
            "cursor": first["cursor"],
        }]),
        Some(39),
    )
    .unwrap();
    assert_eq!(keys(&next), every[1..2]);
}

#[test]
fn offset_pages_at_a_pinned_block_stay_stable_while_the_head_changes() {
    let mut host = fixture();
    let mut nonce = 0;
    for i in 0..12u64 {
        submit(
            &mut host,
            &mut nonce,
            vec![create(
                2000 + u128::from(i),
                vec![("p", arkiv_db::Cell::attribute(Value::U64(i % 3)))],
            )],
        );
    }
    host.produce().unwrap();
    let pinned = format!("0x{:x}", host.head.height);
    let select = |count: bool| json!({"key": true, "attributes": true, "count": count});
    // Each page, its fresh cursor token reduced to whether it has one.
    let pages = |host: &Host, source: &str, at: Option<&str>| {
        (0..5u64)
            .map(|page| {
                let mut options = json!({"limit": 3, "offset": page * 3, "select": select(true)});
                if let Some(at) = at {
                    options["atBlock"] = json!(at);
                }
                let mut page = query(host, &json!([source, options])).unwrap();
                if let Some(cursor) = page.get_mut("cursor") {
                    *cursor = json!(true);
                }
                page
            })
            .collect::<Vec<_>>()
    };
    // The indexed path, the scan fallback (EXISTS without a type is
    // nine groups) and a sorted walk.
    let sources = [
        "TYPEOF(p) = u64",
        "EXISTS(p)",
        "TYPEOF(p) = u64 ORDER BY p:u64 DESC",
    ];
    let before: Vec<_> = sources
        .iter()
        .map(|source| pages(&host, source, Some(&pinned)))
        .collect();
    for (source, pages) in sources.iter().zip(&before) {
        let seen: Vec<_> = pages.iter().flat_map(keys).collect();
        let distinct: std::collections::BTreeSet<_> = seen.iter().map(Json::to_string).collect();
        assert_eq!((seen.len(), distinct.len()), (12, 12), "{source}");
        assert!(pages.iter().all(|p| p["count"] == "0xc"));
        assert!(pages.iter().all(|p| p["blockNumber"] == pinned.as_str()));
    }
    // Between the pages the head moves on: an entity (sorting first) is
    // created and one from the first page is deleted.
    let doomed = keys(&before[0][0])[0].clone();
    submit(
        &mut host,
        &mut nonce,
        vec![
            create(3000, vec![("p", arkiv_db::Cell::attribute(Value::U64(9)))]),
            delete(&doomed),
        ],
    );
    host.produce().unwrap();
    host.produce().unwrap();
    // Pinned to page one's block, every offset page is unchanged.
    for (source, before) in sources.iter().zip(&before) {
        assert_eq!(&pages(&host, source, Some(&pinned)), before, "{source}");
    }
    // At the head the same offsets have shifted: rows repeat or are
    // missed across pages read before and after a write.
    for (source, before) in sources.iter().zip(&before) {
        let head = pages(&host, source, None);
        assert_eq!(head[0]["count"], "0xc");
        assert_ne!(
            head.iter().flat_map(keys).collect::<Vec<_>>(),
            before.iter().flat_map(keys).collect::<Vec<_>>(),
            "{source}"
        );
    }
    // An unsorted offset page at the pinned block also continues by
    // cursor, from the same snapshot, past later writes.
    for source in &sources[..2] {
        let first = query(
            &host,
            &json!([
                source,
                {"limit": 4, "offset": 4, "atBlock": pinned, "select": select(false)},
            ]),
        )
        .unwrap();
        let (rest, _) = walk(
            &host,
            source,
            json!({"limit": 4, "offset": 4, "atBlock": pinned, "select": select(false)}),
        );
        assert_eq!(keys(&first), rest[..4]);
        let reference: Vec<_> = before[sources.iter().position(|s| s == source).unwrap()]
            .iter()
            .flat_map(keys)
            .collect();
        assert_eq!(rest, reference[4..], "{source}");
    }
}

#[test]
fn timing_uses_numeric_height_and_observed_whole_seconds() {
    let mut host = fixture_with_period(1_500);
    // A clock before the schedule: blocks carry the floor, start + height × period.
    host.set_clock(|| 0);
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
