//! Same-origin Scalar reference; independent of database locks and job permits.
use axum::{Json, response::Html};
use serde_json::Value;
use std::sync::LazyLock;

const SPEC: &str = include_str!("state-openapi.json");
static DOCUMENT: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(SPEC).expect("embedded OpenAPI document must be valid"));

pub(super) async fn html() -> Html<&'static str> {
    Html(include_str!("state-docs.html"))
}

pub(super) async fn openapi() -> Json<Value> {
    Json(DOCUMENT.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn reference_covers_signed_routes_and_listener_boundaries() {
        let paths = DOCUMENT["paths"].as_object().unwrap();
        let expected = [
            ("status", "get"),
            ("genesis", "get"),
            ("prefill", "get"),
            ("headers", "get"),
            ("feed/blocks", "get"),
            ("feed/blocks/{height}", "get"),
            ("accounts/{address}", "get"),
            ("accounts/{address}/proof", "get"),
            ("accounts/{address}/nonce", "get"),
            ("requests/{address}/{id}", "get"),
            ("transactions", "post"),
            ("state/explore", "post"),
            ("query/eq", "post"),
            ("query/resolve", "post"),
            ("query/verified", "post"),
            ("query/inspect", "post"),
            ("query/account", "post"),
            ("query/classify", "post"),
            ("query/text", "post"),
            ("query/range", "post"),
            ("query/range/resolve", "post"),
            ("query/range/verified", "post"),
            ("query/range/inspect", "post"),
            ("control", "post"),
            ("control", "get"),
            ("replication/blocks/{height}", "get"),
            ("replication/blocks/{height}", "post"),
        ];
        for (route, method) in expected {
            assert!(
                paths[&format!("/signed/v1/{route}")][method].is_object(),
                "{method} {route}"
            );
        }
        // Detect new literal dispatch branches even when the static inventory above
        // has not been updated. Parameterized branches are covered explicitly.
        let source: String = include_str!("runtime.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for remainder in source.split("path==\"").skip(1) {
            let route = remainder.split('"').next().unwrap();
            assert!(
                paths.contains_key(&format!("/signed/v1/{route}")),
                "undocumented dispatch: {route}"
            );
        }
        for remainder in source.split("matches!(path.as_str(),").skip(1) {
            let branches = remainder.split(')').next().unwrap();
            for route in branches.split('"').skip(1).step_by(2) {
                assert!(
                    paths.contains_key(&format!("/signed/v1/{route}")),
                    "undocumented dispatch: {route}"
                );
            }
        }
        for route in [
            "/signed/v1/control",
            "/signed/v1/replication/blocks/{height}",
        ] {
            for method in ["get", "post"] {
                let operation = &paths[route][method];
                assert_eq!(
                    operation["security"][0]["PrivateBearer"],
                    serde_json::json!([])
                );
                assert!(
                    operation["servers"][0]["url"]
                        .as_str()
                        .unwrap()
                        .starts_with("http://127.0.0.1:")
                );
                assert!(operation["responses"]["401"].is_object());
            }
        }
        assert!(
            paths["/metrics"]["get"]["servers"][0]["url"]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:")
        );
        let mut ids = BTreeSet::new();
        for path in paths.values() {
            for method in ["get", "post"] {
                if let Some(operation) = path.get(method) {
                    let id = operation["operationId"].as_str().unwrap();
                    assert!(ids.insert(id), "duplicate operation ID: {id}");
                }
            }
        }
    }

    #[test]
    fn reference_covers_every_dispatched_rpc_method() {
        let mut implemented = BTreeSet::new();
        for source in [include_str!("rpc.rs"), include_str!("rpc_reads.rs")] {
            let dispatch = source.split("fn dispatch(").nth(1).unwrap();
            // Stop at the following top-level function, excluding tests and other
            // strings outside dispatch while retaining nested match branches.
            let dispatch = dispatch.split("\nfn ").next().unwrap();
            for literal in dispatch.split('"').skip(1).step_by(2) {
                if ["eth_", "net_", "web3_", "arkiv_"]
                    .iter()
                    .any(|prefix| literal.starts_with(prefix))
                {
                    implemented.insert(literal);
                }
            }
        }
        let documented: BTreeSet<_> = DOCUMENT["paths"]["/"]["post"]["x-jsonrpc-methods"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(implemented, documented);
        let websocket = DOCUMENT["paths"]["/"]["get"].to_string();
        for method in ["eth_subscribe", "eth_unsubscribe", "newHeads", "logs"] {
            assert!(
                websocket.contains(method),
                "missing WebSocket method/topic {method}"
            );
        }
    }

    #[test]
    fn every_local_schema_reference_resolves() {
        fn visit(value: &Value) {
            match value {
                Value::Object(object) => {
                    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                        let pointer = reference
                            .strip_prefix('#')
                            .expect("only embedded references");
                        assert!(
                            DOCUMENT.pointer(pointer).is_some(),
                            "unresolved {reference}"
                        );
                    }
                    for value in object.values() {
                        visit(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        visit(value);
                    }
                }
                _ => (),
            }
        }
        visit(&DOCUMENT);
    }
}
