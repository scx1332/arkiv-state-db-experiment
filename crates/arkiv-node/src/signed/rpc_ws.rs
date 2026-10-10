//! Bounded WebSocket JSON-RPC using the same authenticated dispatcher as HTTP.
//! Subscriptions are connection-local and follow acknowledged canonical blocks.
use super::{rpc, runtime::Shared};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::Semaphore;

const REQUEST_BYTES: usize = 1024 * 1024;
const RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const SUBSCRIPTIONS: usize = 16;
const BATCH: usize = 32;
const MAX_LAG: u64 = 128;
static CONNECTIONS: Semaphore = Semaphore::const_new(16);

pub async fn upgrade(State(rt): State<Shared>, ws: WebSocketUpgrade) -> Response {
    let Ok(permit) = CONNECTIONS.try_acquire() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    ws.max_message_size(REQUEST_BYTES)
        .max_frame_size(REQUEST_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            session(socket, rt).await;
        })
}

struct Subscription {
    next_height: u64,
    // None denotes newHeads, Some denotes logs with address/topics filters.
    filter: Option<Value>,
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

async fn send(socket: &mut WebSocket, value: &Value) -> bool {
    let Ok(text) = serde_json::to_string(value) else {
        return false;
    };
    if text.len() > RESPONSE_BYTES {
        return false;
    }
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(text.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn dispatch(
    rt: &Shared,
    request: Value,
    subscriptions: &mut BTreeMap<String, Subscription>,
    next_id: &mut u64,
) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str);
    if !matches!(method, Some("eth_subscribe" | "eth_unsubscribe")) {
        return rpc::handle_request(rt, request).await;
    }
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !matches!(id, Value::String(_) | Value::Number(_) | Value::Null)
    {
        return failure(Value::Null, -32600, "Invalid request");
    }
    // A notification cannot establish a usable subscription; do not retain one.
    if request.get("id").is_none() {
        return Value::Null;
    }
    let Some(params) = request.get("params").and_then(Value::as_array) else {
        return failure(id, -32602, "Expected array parameters");
    };
    if method == Some("eth_unsubscribe") {
        if params.len() != 1 || !params[0].is_string() {
            return failure(id, -32602, "Expected subscription ID");
        }
        return json!({"jsonrpc":"2.0","id":id,"result":subscriptions.remove(params[0].as_str().unwrap()).is_some()});
    }
    if subscriptions.len() >= SUBSCRIPTIONS {
        return failure(id, -32005, "Subscription limit exceeded");
    }
    let filter = match params.first().and_then(Value::as_str) {
        Some("newHeads") if params.len() == 1 => None,
        Some("logs") if params.len() == 1 => Some(json!({})),
        Some("logs") if params.len() == 2 && params[1].is_object() => {
            let filter = params[1].clone();
            if filter
                .as_object()
                .unwrap()
                .keys()
                .any(|k| !matches!(k.as_str(), "address" | "topics"))
            {
                return failure(
                    id,
                    -32602,
                    "Log subscriptions accept address and topics only",
                );
            }
            Some(filter)
        }
        _ => return failure(id, -32602, "Supported subscriptions: newHeads, logs"),
    };
    if filter
        .as_ref()
        .is_some_and(|filter| rpc::validate_filter(filter).is_err())
    {
        return failure(id, -32602, "Invalid log subscription filter");
    }
    let sampled = rpc::handle_request(
        rt,
        json!({"jsonrpc":"2.0","id":id,"method":"eth_blockNumber","params":[]}),
    )
    .await;
    if sampled.get("error").is_some() {
        return sampled;
    }
    let Some(height) = sampled
        .get("result")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix("0x"))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
    else {
        return failure(id, -32603, "Invalid head response");
    };
    if let Some(ref filter) = filter {
        let mut probe = filter.clone();
        probe["fromBlock"] = json!(format!("0x{height:x}"));
        probe["toBlock"] = probe["fromBlock"].clone();
        let checked = rpc::handle_request(
            rt,
            json!({"jsonrpc":"2.0","id":id,"method":"eth_getLogs","params":[probe]}),
        )
        .await;
        if checked.get("error").is_some() {
            return checked;
        }
    }
    let Some(next_height) = height.checked_add(1) else {
        return failure(id, -32000, "Height exhausted");
    };
    let Some(new_id) = next_id.checked_add(1) else {
        return failure(id, -32000, "Subscription IDs exhausted");
    };
    *next_id = new_id;
    let subscription_id = format!("0x{new_id:x}");
    subscriptions.insert(
        subscription_id.clone(),
        Subscription {
            next_height,
            filter,
        },
    );
    json!({"jsonrpc":"2.0","id":id,"result":subscription_id})
}

async fn session(mut socket: WebSocket, rt: Shared) {
    let mut subscriptions = BTreeMap::<String, Subscription>::new();
    let mut next_id = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                let bytes = match message {
                    Message::Text(text) => text.as_bytes().to_vec(),
                    Message::Binary(bytes) => bytes.to_vec(),
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => continue,
                };
                let request: Value = match serde_json::from_slice(&bytes) {
                    Ok(value) => value,
                    Err(_) => { if !send(&mut socket, &failure(Value::Null,-32700,"Parse error")).await { break } continue; }
                };
                let response = if let Some(batch) = request.as_array() {
                    if batch.is_empty() || batch.len() > BATCH {
                        failure(Value::Null,-32600,"Invalid or oversized batch")
                    } else {
                        let mut out = Vec::new();
                        let mut size = 0;
                        for item in batch {
                            let response = dispatch(&rt,item.clone(),&mut subscriptions,&mut next_id).await;
                            if item.get("id").is_some() || !item.is_object() || item.get("method").is_none() {
                                size += response.to_string().len();
                                if size > RESPONSE_BYTES { return }
                                out.push(response);
                            }
                        }
                        if out.is_empty() { Value::Null } else { Value::Array(out) }
                    }
                } else {
                    let notification = request.is_object() && request.get("id").is_none() && request.get("method").is_some();
                    let response = dispatch(&rt,request,&mut subscriptions,&mut next_id).await;
                    if notification { Value::Null } else { response }
                };
                if !response.is_null() && !send(&mut socket,&response).await { break }
            }
            _ = tick.tick(), if !subscriptions.is_empty() => {
                let head = match rt.node.try_lock() { Ok(node) => node.head().height, Err(_) => continue };
                let mut pruned = Vec::new();
                for (id, subscription) in &mut subscriptions {
                    if head.saturating_sub(subscription.next_height) > MAX_LAG { return }
                    // Bound work per tick so socket input and unsubscribe remain responsive.
                    let through = head.min(subscription.next_height.saturating_add(7));
                    while subscription.next_height <= through {
                        let height = subscription.next_height;
                        let request = if let Some(ref filter) = subscription.filter {
                            let mut filter = filter.clone();
                            filter["fromBlock"] = json!(format!("0x{height:x}"));
                            filter["toBlock"] = filter["fromBlock"].clone();
                            json!({"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[filter]})
                        } else {
                            json!({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":[format!("0x{height:x}"),false]})
                        };
                        let answer = rpc::handle_request(&rt,request).await;
                        if answer["error"]["code"] == rpc::HISTORY_PRUNED_CODE {
                            // The replay position fell below the retention floor:
                            // end the subscription explicitly, never skip events.
                            if !send(&mut socket,&json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":id,"error":answer["error"]}})).await { return }
                            pruned.push(id.clone());
                            break;
                        }
                        if answer.get("error").is_some() {
                            // Retain the next height and retry; never silently skip an event.
                            break;
                        }
                        let Some(mut result) = answer.get("result").cloned() else { return };
                        if subscription.filter.is_some() {
                            let Some(logs) = result.as_array() else { return };
                            for log in logs {
                                if !send(&mut socket,&json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":id,"result":log}})).await { return }
                            }
                        } else {
                            let Some(header) = result.as_object_mut() else { return };
                            header.remove("transactions");
                            header.remove("uncles");
                            if !send(&mut socket,&json!({"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":id,"result":result}})).await { return }
                        }
                        let Some(next) = height.checked_add(1) else { return };
                        subscription.next_height = next;
                    }
                }
                for id in pruned {
                    subscriptions.remove(&id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signed::{
        crypto::Signer,
        history::History,
        host::Host,
        protocol::*,
        runtime::{Node, Runtime},
    };
    use arkiv_db::durable::Options;
    use arkiv_db_memory::MemoryVersionedStore;
    use std::sync::Arc;

    fn runtime() -> Shared {
        let signer = Signer::from_bytes([1; 32]).unwrap();
        let genesis = SignedGenesis::new(
            default_native_genesis(9405, 1000),
            [1; 16],
            *signer.address().as_bytes(),
            1000,
            crate::accounts::AccountConfig::new(vec![(
                signer.address(),
                alloy_primitives::U256::from(10u128.pow(21)),
            )])
            .unwrap(),
        )
        .unwrap();
        let certificate = SignedHeader::sign(&genesis, genesis.header(), &signer).unwrap();
        let history =
            History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash()).unwrap();
        let host = Host::create(
            Arc::new(MemoryVersionedStore::new()),
            history,
            genesis,
            [1; 16],
            certificate,
            Some(signer),
            Options::default(),
        )
        .unwrap();
        Runtime::new(Node::Full(Box::new(host)), None, None, None).unwrap()
    }

    #[tokio::test]
    async fn subscription_validation_bounds_and_unsubscribe_are_connection_local() {
        let rt = runtime();
        let mut subscriptions = BTreeMap::new();
        let mut serial = 0;
        for params in [
            json!(["logs", {"address":"0x1"}]),
            json!(["logs", {"fromBlock":"0x0"}]),
            json!(["newPendingTransactions"]),
        ] {
            let result = dispatch(
                &rt,
                json!({"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":params}),
                &mut subscriptions,
                &mut serial,
            )
            .await;
            assert_eq!(result["error"]["code"], -32602);
            assert!(subscriptions.is_empty());
        }
        let result = dispatch(
            &rt,
            json!({"jsonrpc":"2.0","method":"eth_subscribe","params":["newHeads"]}),
            &mut subscriptions,
            &mut serial,
        )
        .await;
        assert!(result.is_null());
        assert!(subscriptions.is_empty());
        for _ in 0..SUBSCRIPTIONS {
            let result = dispatch(
                &rt,
                json!({"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}),
                &mut subscriptions,
                &mut serial,
            )
            .await;
            assert!(result.get("result").is_some(), "{result}");
        }
        let result = dispatch(
            &rt,
            json!({"jsonrpc":"2.0","id":2,"method":"eth_subscribe","params":["newHeads"]}),
            &mut subscriptions,
            &mut serial,
        )
        .await;
        assert_eq!(result["error"]["code"], -32005);
        for expected in [true, false] {
            let result = dispatch(
                &rt,
                json!({"jsonrpc":"2.0","id":3,"method":"eth_unsubscribe","params":["0x1"]}),
                &mut subscriptions,
                &mut serial,
            )
            .await;
            assert_eq!(result["result"], expected);
        }
        assert_eq!(subscriptions.len(), SUBSCRIPTIONS - 1);
    }
}
