//! Bounded HTTP transport and one writer shared by timer and private controls.
use super::{
    Error, Result,
    host::{Control, Host, RunConfig, StoredBlock, identity},
    light::{Light, VerifiedRequest, resolve},
    wire::*,
};
use arkiv_db::{B256, decode_exact};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

pub enum Node {
    Full(Box<Host>),
    Light(Box<Light>),
}
pub struct Runtime {
    pub node: Mutex<Node>,
    pub peer: Option<String>,
    pub replication_peer: Option<String>,
    pub reads: Arc<Semaphore>,
    pub admin_token: Option<String>,
    pub replication_token: Option<String>,
    pub allow_private_http: bool,
    /// The node's retained history file, when known; public status reports
    /// its size as a storage diagnostic (absent, never zero, when unknown).
    pub storage_path: Option<PathBuf>,
    client: reqwest::Client,
}
pub type Shared = Arc<Runtime>;
impl Runtime {
    pub fn new(
        node: Node,
        peer: Option<String>,
        replication_peer: Option<String>,
    ) -> Result<Shared> {
        let allow_private_http = private_http_from_env()?;
        if let Some(p) = &peer {
            validate_peer_with_policy(p, allow_private_http)?;
        }
        if let Some(p) = &replication_peer {
            validate_peer_with_policy(p, allow_private_http)?;
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error("StorageUnavailable"))?;
        Ok(Arc::new(Self {
            node: Mutex::new(node),
            peer,
            replication_peer,
            reads: Arc::new(Semaphore::new(READ_CONCURRENCY)),
            admin_token: std::env::var("SIMULATOR_ADMIN_TOKEN").ok(),
            replication_token: std::env::var("SIMULATOR_REPLICATION_TOKEN").ok(),
            allow_private_http,
            storage_path: None,
            client,
        }))
    }
}
pub fn validate_peer(peer: &str) -> Result<()> {
    validate_peer_with_policy(peer, false)
}
/// Only an explicit deployment opt-in permits cleartext configured private peers.
/// This never accepts peer URLs from public HTTP request bodies.
pub fn parse_private_http(value: Option<&str>) -> Result<bool> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        _ => Err(Error("InvalidRequest")),
    }
}
pub fn private_http_from_env() -> Result<bool> {
    match std::env::var("SIMULATOR_ALLOW_PRIVATE_HTTP") {
        Ok(value) => parse_private_http(Some(&value)),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(_) => Err(Error("InvalidRequest")),
    }
}
pub fn validate_peer_with_policy(peer: &str, allow_private_http: bool) -> Result<()> {
    let url = reqwest::Url::parse(peer).map_err(|_| Error("InvalidRequest"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(Error("InvalidRequest"));
    }
    let local = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if url.scheme() != "https" && !(url.scheme() == "http" && (local || allow_private_http)) {
        return Err(Error("InvalidRequest"));
    }
    Ok(())
}
fn head_json(h: &crate::Header) -> Json {
    json!({"height":h.height.to_string(),"hash":hex(h.hash().as_slice()),"stateRoot":hex(h.state_root.unwrap_or(B256::ZERO).as_slice())})
}
pub fn status(node: &Node) -> Json {
    let (id, head, paused, health, role, revision, workload, observed, memory) = match node {
        Node::Full(h) => {
            let stats = h.engine.stats();
            (
                &h.identity,
                &h.head,
                h.paused,
                h.health,
                h.role,
                h.revision,
                &h.workload,
                h.observed,
                json!({"engineCacheBytes":stats.cache.payload_capacity.to_string(),"engineCacheEntries":stats.cache.entries,"residentManifests":stats.resident_manifests}),
            )
        }
        Node::Light(h) => (
            &h.identity,
            &h.head,
            h.paused,
            h.health,
            "light",
            0,
            &h.run.workload,
            h.observed,
            json!({"engineCacheBytes":"0","engineCacheEntries":0,"residentManifests":0}),
        ),
    };
    json!({"apiVersion":1,"feedVersion":1,"sourceId":id.source_id,"runId":id.run_id,"genesisHash":id.genesis_hash,"chainId":id.chain_id,"sourceKind":"arkiv-native-simulator","authentication":"unsigned-simulator-v1","durability":"durable","head":head_json(head),"paused":paused,"health":health,"role":role,"configRevision":revision.to_string(),"projectionVersion":arkiv_db::LAYOUT_VERSION,"blockFormatVersion":crate::block::FORMAT_VERSION,"protocolVersion":crate::block::DEMO_PROTOCOL_VERSION,"coverage":{"from":"0","through":head.height.to_string(),"complete":true},"proofProfiles":[super::profile::PROOF_PROFILE],"capabilities":super::profile::capabilities(),"limits":{"feedPageBytes":FEED_CAP.to_string(),"pageItems":HEADER_BATCH},"workload":workload,"memory":memory,"observedPeerHeight":observed.to_string()})
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenesisResponse {
    pub source_id: String,
    pub run_id: String,
    pub genesis_hash: String,
    pub chain_id: String,
    pub genesis: String,
    pub header: String,
    pub durable_identity: String,
}
impl GenesisResponse {
    pub fn validate(
        &self,
        expected: &Identity,
    ) -> Result<arkiv_db::durable::simulator::SimulatorIdentity> {
        let id = Identity {
            source_id: self.source_id.clone(),
            run_id: self.run_id.clone(),
            genesis_hash: self.genesis_hash.clone(),
            chain_id: self.chain_id.clone(),
        };
        if id != *expected {
            return Err(Error("IdentityMismatch"));
        }
        let stored = arkiv_db::durable::simulator::SimulatorIdentity::from_envelope(&unhex(
            &self.durable_identity,
            65536 + 256,
        )?)?;
        let run: RunConfig = decode_exact(&stored.metadata).map_err(|_| Error("InvalidRequest"))?;
        run.validate()?;
        if identity(&stored, &run.genesis) != id
            || pack(&run.genesis) != self.genesis
            || pack(&run.genesis.header()) != self.header
            || stored.node_genesis_hash != run.genesis.header().hash()
        {
            return Err(Error("IdentityMismatch"));
        }
        Ok(stored)
    }
}
fn genesis(node: &Node) -> Result<GenesisResponse> {
    let (id, run, stored) = match node {
        Node::Full(h) => (&h.identity, &h.run, &h.durable_identity),
        Node::Light(h) => (&h.identity, &h.run, &h.durable_identity),
    };
    Ok(GenesisResponse {
        source_id: id.source_id.clone(),
        run_id: id.run_id.clone(),
        genesis_hash: id.genesis_hash.clone(),
        chain_id: id.chain_id.clone(),
        genesis: pack(&run.genesis),
        header: pack(&run.genesis.header()),
        durable_identity: hex(&stored.envelope()?),
    })
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadQuery {
    pub after_height: Option<String>,
    pub after_hash: Option<String>,
    pub from_height: Option<String>,
    pub limit: Option<usize>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Headers {
    pub source_id: String,
    pub run_id: String,
    pub genesis_hash: String,
    pub chain_id: String,
    pub headers: Vec<HeaderDto>,
    pub sampled: HeaderDto,
}

/// Noncanonical storage telemetry for public status: the retained history
/// file's current size. Unknown or unreadable files report nothing rather
/// than a fabricated zero.
fn storage_json(storage: Option<&std::path::Path>) -> Option<Json> {
    let bytes = std::fs::metadata(storage?).ok()?.len();
    Some(json!({"fileBytes":bytes.to_string()}))
}
fn read(node: &Node, path: &str, q: &ReadQuery, storage: Option<&std::path::Path>) -> Result<Json> {
    if path == "status" {
        let mut current = status(node);
        if let Some(storage) = storage_json(storage)
            && let Some(object) = current.as_object_mut()
        {
            object.insert("storage".into(), storage);
        }
        return Ok(current);
    }
    if path == "genesis" {
        return serde_json::to_value(genesis(node)?).map_err(|_| Error("StorageCorrupt"));
    }
    if path == "headers" || path == "feed/blocks" {
        let limit = q.limit.unwrap_or(HEADER_BATCH);
        if !(1..=HEADER_BATCH).contains(&limit) {
            return Err(Error("InvalidRequest"));
        }
        let (head, id) = match node {
            Node::Full(h) => (h.head, &h.identity),
            Node::Light(h) => (h.head, &h.identity),
        };
        let header = |height| match node {
            Node::Full(h) => h.header(height),
            Node::Light(h) => h.header(height),
        };
        let start = if let Some(after) = &q.after_height {
            let after = number(after)?;
            if after > head.height {
                return Err(Error("BlockNotSynced"));
            }
            if q.after_hash.as_deref().map(hash).transpose()? != Some(header(after)?.hash()) {
                return Err(Error("ChainConflict"));
            }
            after.checked_add(1).ok_or(Error("CapacityReached"))?
        } else {
            number(q.from_height.as_deref().unwrap_or("0"))?
        };
        if start > head.height.saturating_add(1) {
            return Err(Error("UnknownBlock"));
        }
        let mut headers = Vec::new();
        for height in (start..=head.height).take(limit) {
            headers.push(HeaderDto::new(&header(height)?)?);
        }
        return serde_json::to_value(Headers {
            source_id: id.source_id.clone(),
            run_id: id.run_id.clone(),
            genesis_hash: id.genesis_hash.clone(),
            chain_id: id.chain_id.clone(),
            headers,
            sampled: HeaderDto::new(&head)?,
        })
        .map_err(|_| Error("StorageCorrupt"));
    }
    let Node::Full(host) = node else {
        return Err(Error("UnsupportedProof"));
    };
    if let Some(h) = path.strip_prefix("feed/blocks/") {
        return serde_json::to_value(host.block(number(h)?)?.feed)
            .map_err(|_| Error("StorageCorrupt"));
    }
    if let Some(h) = path.strip_prefix("replication/blocks/") {
        return Ok(json!({"envelope":hex(&host.block(number(h)?)?.bytes()?)}));
    }
    Err(Error("InvalidRequest"))
}
fn response(result: Result<Json>, cap: usize) -> Response {
    match result.and_then(|v| json_bytes(&v, cap)) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            bytes,
        )
            .into_response(),
        Err(Error(code)) => {
            let status = match code {
                "Unauthorized" => StatusCode::UNAUTHORIZED,
                "UnknownRun" | "UnknownBlock" | "NamespaceNotFound" => StatusCode::NOT_FOUND,
                "IdentityMismatch"
                | "BlockNotSynced"
                | "CoverageUnavailable"
                | "CursorMismatch"
                | "ChainConflict"
                | "RequestConflict"
                | "CapacityReached" => StatusCode::CONFLICT,
                "UnsupportedProof" | "ProofLimitExceeded" | "BudgetExceeded" => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                "InvalidProof" => StatusCode::BAD_GATEWAY,
                "QueueFull" | "ServerBusy" => StatusCode::TOO_MANY_REQUESTS,
                "StorageUnavailable" | "StorageCorrupt" | "CommitUncertain" => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                _ => StatusCode::BAD_REQUEST,
            };
            (status, axum::Json(json!({"error":code}))).into_response()
        }
    }
}
async fn http_read(
    State(rt): State<Shared>,
    Path(path): Path<String>,
    Query(q): Query<ReadQuery>,
) -> Response {
    if path.starts_with("replication/") {
        return response(Err(Error("InvalidRequest")), 1024);
    }
    http_read_allowed(rt, path, q).await
}
async fn http_read_allowed(rt: Shared, path: String, q: ReadQuery) -> Response {
    let permit = match rt.reads.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(Err(Error("ServerBusy")), 1024),
    };
    let cap = if path.starts_with("feed/blocks/") {
        FEED_CAP
    } else {
        RESPONSE_CAP
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let n = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        read(&n, &path, &q, rt.storage_path.as_deref())
    })
    .await
    .unwrap_or(Err(Error("StorageUnavailable")));
    response(result, cap)
}
async fn parse_body<T: serde::de::DeserializeOwned>(body: Body) -> Result<T> {
    let bytes = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, REQUEST_CAP))
        .await
        .map_err(|_| Error("InvalidRequest"))?
        .map_err(|_| Error("InvalidRequest"))?;
    serde_json::from_slice(&bytes).map_err(|_| Error("InvalidRequest"))
}
async fn http_post(State(rt): State<Shared>, Path(path): Path<String>, body: Body) -> Response {
    let permit = match rt.reads.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(Err(Error("ServerBusy")), 1024),
    };
    if path == "query/classify" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ClassifyRequest {
            query: String,
        }
        let result = match parse_body::<ClassifyRequest>(body).await {
            Ok(r) => serde_json::to_value(super::query_language::classify(&r.query))
                .map_err(|_| Error("InvalidRequest")),
            Err(e) => Err(e),
        };
        return response(result, RESPONSE_CAP);
    }
    if path == "query/text" {
        let r = match parse_body::<TextQueryRequest>(body).await {
            Ok(r) => r,
            Err(e) => return response(Err(e), 1024),
        };
        return text_query(rt, r).await;
    }
    if path == "query/verified" || path == "query/inspect" {
        let r = match parse_body::<VerifiedRequest>(body).await {
            Ok(r) => r,
            Err(e) => return response(Err(e), 1024),
        };
        let result = verified(rt, r, path == "query/inspect").await;
        drop(permit);
        return response(result, RESPONSE_CAP);
    }
    let parsed =
        match tokio::time::timeout(Duration::from_secs(5), to_bytes(body, REQUEST_CAP)).await {
            Ok(Ok(b)) => b,
            _ => return response(Err(Error("InvalidRequest")), 1024),
        };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let n = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        if path == "query/eq" {
            let r: ProofRequest =
                serde_json::from_slice(&parsed).map_err(|_| Error("InvalidRequest"))?;
            let request = unpack(&r.request, 4096)?;
            let Node::Full(h) = &*n else {
                return Err(Error("UnsupportedProof"));
            };
            return Ok(json!({"proof":hex(&h.proof(&request)?)}));
        }
        if path == "query/resolve" {
            let r: VerifiedRequest =
                serde_json::from_slice(&parsed).map_err(|_| Error("InvalidRequest"))?;
            let (id, h) = match &*n {
                Node::Full(h) => (&h.identity, h.header(number(&r.height)?)?),
                Node::Light(h) => (&h.identity, h.header(number(&r.height)?)?),
            };
            if r.cursor.is_some() {
                return Err(Error("CursorMismatch"));
            }
            return Ok(json!({"request":pack(&resolve(id,&h,&r)?)}));
        }
        Err(Error("InvalidRequest"))
    })
    .await
    .unwrap_or(Err(Error("StorageUnavailable")));
    response(result, RESPONSE_CAP)
}
fn authorized(rt: &Runtime, headers: &HeaderMap) -> bool {
    let Some(expected) = rt.admin_token.as_deref().filter(|s| s.len() >= 32) else {
        return false;
    };
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| h.strip_prefix("Bearer ") == Some(expected))
}
async fn http_control(State(rt): State<Shared>, headers: HeaderMap, body: Body) -> Response {
    if !authorized(&rt, &headers) {
        return response(Err(Error("Unauthorized")), 1024);
    }
    let permit = match rt.reads.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(Err(Error("ServerBusy")), 1024),
    };
    let c = match parse_body::<Control>(body).await {
        Ok(c) => c,
        Err(e) => return response(Err(e), 1024),
    };
    let result=tokio::task::spawn_blocking(move||{
        let _permit=permit;let mut n=rt.node.try_lock().map_err(|_|Error("ServerBusy"))?;
        let Node::Full(h)=&mut*n else{return Err(Error("InvalidRequest"))};
        let command_id=c.command_id.clone();let result=h.control(c)?;
        Ok(json!({"commandId":command_id,"runId":h.identity.run_id,"configRevision":h.revision.to_string(),"head":head_json(&h.head),"paused":h.paused,"health":h.health,"status":result["status"],"resultHeight":result.get("height").or_else(||result.get("effectiveHeight")).cloned()}))
    }).await.unwrap_or(Err(Error("StorageUnavailable")));
    response(result, REQUEST_CAP)
}
pub fn public_router(rt: Shared) -> Router {
    Router::new()
        .route("/sim/v1/{*path}", get(http_read).post(http_post))
        .with_state(rt)
}
async fn http_body(
    State(rt): State<Shared>,
    Path(height): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&rt, &headers) {
        return response(Err(Error("Unauthorized")), 1024);
    }
    http_read_allowed(
        rt,
        format!("replication/blocks/{height}"),
        ReadQuery::default(),
    )
    .await
}
pub fn control_router(rt: Shared) -> Router {
    Router::new()
        .route("/sim/v1/control", post(http_control))
        .route("/sim/v1/replication/blocks/{height}", get(http_body))
        .with_state(rt)
}

pub async fn fetch<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    peer: &str,
    path: &str,
    body: Option<Json>,
    cap: usize,
) -> Result<T> {
    fetch_authorized(client, peer, path, body, cap, None, false).await
}
pub async fn fetch_with_policy<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    peer: &str,
    path: &str,
    body: Option<Json>,
    cap: usize,
    allow_private_http: bool,
) -> Result<T> {
    fetch_authorized(client, peer, path, body, cap, None, allow_private_http).await
}
async fn fetch_authorized<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    peer: &str,
    path: &str,
    body: Option<Json>,
    cap: usize,
    token: Option<&str>,
    allow_private_http: bool,
) -> Result<T> {
    validate_peer_with_policy(peer, allow_private_http)?;
    let url = format!("{}{}", peer.trim_end_matches('/'), path);
    let builder = if let Some(body) = body {
        client.post(url).json(&body)
    } else {
        client.get(url)
    };
    let builder = if let Some(token) = token {
        builder.bearer_auth(token)
    } else {
        builder
    };
    let mut response = builder
        .send()
        .await
        .map_err(|_| Error("StorageUnavailable"))?;
    let success = response.status().is_success();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error("StorageUnavailable"))?
    {
        if chunk.len() > cap.saturating_sub(bytes.len()) {
            return Err(Error("InvalidRequest"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !success {
        let v: Json = serde_json::from_slice(&bytes).map_err(|_| Error("StorageUnavailable"))?;
        return Err(Error(match v.get("error").and_then(Json::as_str) {
            Some("ServerBusy") => "ServerBusy",
            Some("ProofLimitExceeded") => "ProofLimitExceeded",
            Some("BudgetExceeded") => "BudgetExceeded",
            Some("UnknownBlock") => "UnknownBlock",
            Some("NamespaceNotFound") => "NamespaceNotFound",
            Some("CursorMismatch") => "CursorMismatch",
            Some("ChainConflict") => "ChainConflict",
            Some("IdentityMismatch") => "IdentityMismatch",
            Some("BlockNotSynced") => "BlockNotSynced",
            Some("Unauthorized") => "Unauthorized",
            Some("UnsupportedProof") => "UnsupportedProof",
            Some("UnsupportedVersion") => "UnsupportedVersion",
            Some("CommitUncertain") => "CommitUncertain",
            Some("StorageCorrupt") => "StorageCorrupt",
            _ => "StorageUnavailable",
        }));
    }
    serde_json::from_slice(&bytes).map_err(|_| Error("InvalidRequest"))
}
/// A text query never bypasses the established Eq verifier. `require_proof`
/// is explicit on the wire; false permits proofs too, but never a silent fallback.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TextQueryRequest {
    query: String,
    require_proof: bool,
    height: String,
    namespace: String,
    limit: u32,
    cursor: Option<String>,
}
async fn text_query(rt: Shared, r: TextQueryRequest) -> Response {
    let classification = super::query_language::classify(&r.query);
    let reject = |code: &str, status: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({
                "error":code,"queryText":r.query,"classification":classification,
                "verificationStatus":{"status":status}
            })),
        )
            .into_response()
    };
    if classification.language.status != "valid" {
        return reject("QueryLanguageError", "not_executed");
    }
    if r.require_proof && classification.proof.status != "supported" {
        return reject("UnsupportedProof", "not_executed");
    }
    if classification.execution.status != "supported" {
        return reject("UnsupportedQueryExecution", "not_executed");
    }
    {
        let n = match rt.node.try_lock() {
            Ok(n) => n,
            Err(_) => return reject("ServerBusy", "not_executed"),
        };
        if !matches!(&*n, Node::Light(_)) {
            return reject("UnsupportedNodeRole", "not_executed");
        }
    }
    let Some(selection) = classification.selection.as_ref() else {
        return reject("UnsupportedQueryExecution", "not_executed");
    };
    let request = VerifiedRequest {
        height: r.height.clone(),
        namespace: r.namespace.clone(),
        attribute: selection.attribute.clone(),
        value_type: selection.value_type.clone(),
        value: selection.value.clone(),
        limit: r.limit,
        cursor: r.cursor.clone(),
    };
    match verified(rt, request, true).await {
        Ok(mut page) => {
            if let Some(object) = page.as_object_mut() {
                object.insert("classification".into(), json!(classification));
                object.insert("queryText".into(), json!(r.query));
                object.insert("verificationStatus".into(), json!({"status":"verified"}));
            }
            response(Ok(page), RESPONSE_CAP)
        }
        Err(e) => reject(
            e.0,
            match e.0 {
                "ProofLimitExceeded" | "BudgetExceeded" => "resource_limit",
                _ => "failed",
            },
        ),
    }
}

async fn verified(rt: Shared, r: VerifiedRequest, inspect: bool) -> Result<Json> {
    let (request, previous) = {
        let n = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        let Node::Light(h) = &*n else {
            return Err(Error("UnsupportedProof"));
        };
        h.request(&r)?
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Proof {
        proof: String,
    }
    let fetch_started = Instant::now();
    let result: Proof = fetch_with_policy(
        &rt.client,
        rt.peer.as_deref().ok_or(Error("InvalidRequest"))?,
        "/sim/v1/query/eq",
        Some(json!({"request":pack(&request)})),
        RESPONSE_CAP,
        rt.allow_private_http,
    )
    .await?;
    let fetch_ms = fetch_started.elapsed().as_millis();
    let cap = if inspect {
        super::inspection::INSPECT_PROOF_CAP
    } else {
        proof_limits().max_proof_bytes
    };
    if result.proof.len() > cap * 2 + 2 {
        return Err(Error("ProofLimitExceeded"));
    }
    let bytes = unhex(&result.proof, cap)?;
    tokio::task::spawn_blocking(move || {
        let verify_started = Instant::now();
        let mut n = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        let Node::Light(h) = &mut *n else {
            return Err(Error("UnsupportedProof"));
        };
        let mut page = if inspect {
            h.inspect(&r, &request, previous.as_ref(), &bytes)?
        } else {
            h.verify(&r, &request, previous.as_ref(), &bytes)?
        };
        // Diagnostics describe this light process's own work on the complete
        // proof it fetched; they are telemetry, not part of the verified page.
        if let Some(object) = page.as_object_mut() {
            object.insert(
                "diagnostics".into(),
                json!({"verifier":"light-follower","proofBytes":bytes.len().to_string(),"fetchMs":fetch_ms.to_string(),"verifyMs":verify_started.elapsed().as_millis().to_string()}),
            );
        }
        Ok(page)
    })
    .await
    .map_err(|_| Error("StorageUnavailable"))?
}
pub async fn sync_once(rt: Shared) -> Result<()> {
    let (head, id, light, paused) = {
        let n = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        match &*n {
            Node::Full(h) => (h.head, h.identity.clone(), false, h.paused),
            Node::Light(h) => (h.head, h.identity.clone(), true, h.paused),
        }
    };
    if paused {
        return Ok(());
    }
    let peer = rt.peer.as_deref().ok_or(Error("InvalidRequest"))?;
    let path = format!(
        "/sim/v1/headers?afterHeight={}&afterHash={}&limit={}",
        head.height,
        hex(head.hash().as_slice()),
        if light { HEADER_BATCH } else { 1 }
    );
    let batch: Headers = fetch_with_policy(
        &rt.client,
        peer,
        &path,
        None,
        REQUEST_CAP,
        rt.allow_private_http,
    )
    .await?;
    if (
        &batch.source_id,
        &batch.run_id,
        &batch.genesis_hash,
        &batch.chain_id,
    ) != (&id.source_id, &id.run_id, &id.genesis_hash, &id.chain_id)
    {
        return Err(Error("IdentityMismatch"));
    }
    if batch.headers.len() > if light { HEADER_BATCH } else { 1 } {
        return Err(Error("InvalidRequest"));
    }
    let chain = number(&id.chain_id)?;
    let sampled = batch.sampled.validate(chain)?;
    let headers = batch
        .headers
        .iter()
        .map(|h| h.validate(chain))
        .collect::<Result<Vec<_>>>()?;
    if light {
        let mut node = rt.node.lock().map_err(|_| Error("StorageUnavailable"))?;
        let Node::Light(h) = &mut *node else {
            return Err(Error("InvalidRequest"));
        };
        return h.append(&headers, &sampled);
    }
    {
        let mut node = rt.node.lock().map_err(|_| Error("StorageUnavailable"))?;
        let Node::Full(h) = &mut *node else {
            return Err(Error("InvalidRequest"));
        };
        h.observed = h.observed.max(sampled.height);
        if sampled.height <= h.head.height && h.header(sampled.height)?.hash() != sampled.hash() {
            return Err(Error("ChainConflict"));
        }
        h.health = if sampled.height == h.head.height {
            "following"
        } else {
            "stalled"
        };
        for header in &headers {
            if header.height <= h.head.height && h.header(header.height)? != *header {
                return Err(Error("ChainConflict"));
            }
            if header.height > sampled.height
                || (header.height == sampled.height && header.hash() != sampled.hash())
            {
                return Err(Error("BlockNotSynced"));
            }
        }
    }
    // Fetch one bounded full envelope only for a full follower. No light path reaches here.
    for header in headers {
        if header.height > head.height
            && (header.height != head.height.checked_add(1).ok_or(Error("CapacityReached"))?
                || header.parent_hash != head.hash())
        {
            return Err(Error("BlockNotSynced"));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct BodyDto {
            envelope: String,
        }
        let body: BodyDto = fetch_authorized(
            &rt.client,
            rt.replication_peer
                .as_deref()
                .ok_or(Error("InvalidRequest"))?,
            &format!("/sim/v1/replication/blocks/{}", header.height),
            None,
            RESPONSE_CAP,
            Some(
                rt.replication_token
                    .as_deref()
                    .ok_or(Error("Unauthorized"))?,
            ),
            rt.allow_private_http,
        )
        .await?;
        let bytes = unhex(&body.envelope, ENVELOPE_CAP)?;
        let rt = rt.clone();
        tokio::task::spawn_blocking(move || {
            let mut n = rt.node.lock().map_err(|_| Error("StorageUnavailable"))?;
            let Node::Full(h) = &mut *n else {
                return Err(Error("InvalidRequest"));
            };
            let block = StoredBlock::from_bytes(&bytes, &h.identity, &h.run.genesis)?;
            if block.block.header != header {
                return Err(Error("ChainConflict"));
            }
            h.import(block)?;
            h.observed = h.observed.max(sampled.height);
            h.health = if h.head.height == sampled.height {
                "following"
            } else {
                "stalled"
            };
            Ok(())
        })
        .await
        .map_err(|_| Error("StorageUnavailable"))??;
    }
    Ok(())
}
pub async fn run_loop(rt: Shared, interval: Duration) {
    let mut timer = tokio::time::interval(interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        let producer = rt.peer.is_none();
        let result = if producer {
            let rt = rt.clone();
            tokio::task::spawn_blocking(move || {
                let mut n = rt.node.lock().map_err(|_| Error("StorageUnavailable"))?;
                let Node::Full(h) = &mut *n else {
                    return Err(Error("InvalidRequest"));
                };
                if h.paused || h.health == "storage-fenced" {
                    return Ok(());
                }
                h.produce(None).map(|_| ())
            })
            .await
            .unwrap_or(Err(Error("StorageUnavailable")))
        } else {
            sync_once(rt.clone()).await
        };
        if let Err(e) = result
            && let Ok(mut n) = rt.node.lock()
        {
            let (health, paused) = match &mut *n {
                Node::Full(h) => (&mut h.health, &mut h.paused),
                Node::Light(h) => (&mut h.health, &mut h.paused),
            };
            match e.0 {
                "ChainConflict" | "IdentityMismatch" | "InvalidProof" => {
                    *health = "chain-conflict";
                    *paused = true;
                }
                "CommitUncertain" | "StorageCorrupt" => {
                    *health = "storage-fenced";
                    *paused = true;
                }
                "CapacityReached" => {
                    *health = "stalled";
                    *paused = true;
                }
                "ServerBusy" => {}
                _ => {
                    if *health != "storage-fenced" {
                        *health = "stalled";
                    }
                }
            }
        }
    }
}
