//! Bounded signed node transport. Equality and complete index-range proofs.
#[path = "range_api.rs"]
mod range_api;
#[path = "state_api.rs"]
mod state_api;
#[path = "state_docs.rs"]
mod state_docs;
use super::{
    Error, Result,
    host::Host,
    light::Light,
    protocol::{SignedBlock, SignedGenesis, SignedHeader, SignedTx},
    reads::{ReadLimits, Reads},
};
use crate::simulator::{
    light::{VerifiedRequest, resolve},
    query_language,
    wire::*,
};
use arkiv_db::{
    OwnerId, encode,
    proof::{EqRequest, VerifiedContinuation, VerifiedEq},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

pub enum Node {
    Full(Box<Host>),
    Light {
        light: Box<Light>,
        identity: Identity,
    },
}
impl Node {
    pub fn genesis(&self) -> &SignedGenesis {
        match self {
            Self::Full(h) => &h.genesis,
            Self::Light { light, .. } => &light.genesis,
        }
    }
    pub fn identity(&self) -> &Identity {
        match self {
            Self::Full(h) => &h.identity,
            Self::Light { identity, .. } => identity,
        }
    }
    pub fn head(&self) -> crate::Header {
        match self {
            Self::Full(h) => h.head,
            Self::Light { light, .. } => light.head,
        }
    }
    pub fn certificate(&self, height: u64) -> Result<SignedHeader> {
        match self {
            Self::Full(h) => h.certificate(height),
            Self::Light { light, .. } => light.header(height),
        }
    }
}
pub struct Runtime {
    pub node: Mutex<Node>,
    pub peer: Option<String>,
    pub replication_peer: Option<String>,
    token: Option<String>,
    pub client: reqwest::Client,
    pub(super) slots: Arc<Semaphore>,
    /// Admission of detached reads (see [`super::reads`]).
    pub(super) reads: Reads,
    pub(super) rpc: Mutex<super::rpc::RpcState>,
    continuations: Mutex<VecDeque<(String, EqRequest, VerifiedContinuation)>>,
}
pub type Shared = Arc<Runtime>;
impl Runtime {
    pub fn new(
        node: Node,
        peer: Option<String>,
        replication_peer: Option<String>,
        token: Option<String>,
    ) -> Result<Shared> {
        Self::with_read_limits(node, peer, replication_peer, token, ReadLimits::default())
    }
    pub fn with_read_limits(
        node: Node,
        peer: Option<String>,
        replication_peer: Option<String>,
        token: Option<String>,
        read_limits: ReadLimits,
    ) -> Result<Shared> {
        let reads = Reads::new(read_limits)?;
        for p in [&peer, &replication_peer].into_iter().flatten() {
            crate::simulator::runtime::validate_peer(p)?;
        }
        if token.as_ref().is_some_and(|s| s.len() < 32) {
            return Err(Error("InvalidRequest"));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error("StorageUnavailable"))?;
        let rpc = super::rpc::initial_state(&node)?;
        Ok(Arc::new(Self {
            node: Mutex::new(node),
            peer,
            replication_peer,
            token,
            client,
            slots: Arc::new(Semaphore::new(4)),
            reads,
            rpc: Mutex::new(rpc),
            continuations: Mutex::new(VecDeque::new()),
        }))
    }
    pub fn read_limits(&self) -> ReadLimits {
        self.reads.limits()
    }
    /// Detached reads currently holding a read permit.
    pub fn reads_in_flight(&self) -> usize {
        self.reads.in_flight()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenesisResponse {
    pub identity: Identity,
    pub genesis_bytes: String,
    pub certificate_bytes: String,
}
impl GenesisResponse {
    pub fn validate(
        &self,
        expected_hash: arkiv_db::B256,
        proposer: [u8; 20],
        chain_id: u64,
    ) -> Result<(SignedGenesis, SignedHeader)> {
        let genesis: SignedGenesis = unpack(&self.genesis_bytes, 65536)?;
        genesis.validate()?;
        let certificate: SignedHeader = unpack(&self.certificate_bytes, 512)?;
        certificate.authenticate(&genesis)?;
        if genesis.hash() != expected_hash
            || genesis.proposer != proposer
            || genesis.genesis.chain_id != chain_id
            || certificate.header != genesis.header()
            || self.identity.genesis_hash != hex(expected_hash.as_slice())
            || self.identity.chain_id != chain_id.to_string()
            || self.identity.run_id != hex(&genesis.run_id)[2..]
        {
            return Err(Error("IdentityMismatch"));
        }
        Ok((genesis, certificate))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Headers {
    pub identity: Identity,
    pub certificates: Vec<String>,
    pub sampled: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadQuery {
    height: Option<String>,
    after_height: Option<String>,
    after_hash: Option<String>,
    limit: Option<usize>,
}
fn snapshot(header: &crate::Header) -> Json {
    json!({"height":header.height.to_string(),"hash":hex(header.hash().as_slice()),"stateRoot":hex(header.state_root.as_slice())})
}
pub fn status(node: &Node) -> Json {
    let genesis = node.genesis();
    // Permanent entities exist only through a permanent-prefill genesis
    // (launch versions 3 and 4); live transactions still cannot create them.
    let permanent = genesis
        .prefill
        .as_ref()
        .is_some_and(|p| p.permanent_prefill());
    let mut capabilities = crate::simulator::profile::capabilities(&genesis.genesis.limits);
    capabilities["gas"] = json!({
        "profile":crate::signed::protocol::GAS_PROFILE, "scheduleId":crate::gas::SCHEDULE_ID,
        "blockGasLimit":genesis.genesis.params.block_budget.to_string(),
        "userGasLimit":genesis.genesis.params.user_budget.to_string(),
        "transactionIntrinsicGas":crate::gas::TX_INTRINSIC_GAS.to_string(),
        "referenceGasPriceWei":crate::gas::REFERENCE_GAS_PRICE_WEI.to_string(),
        "token":"GLM", "tokenDecimals":18, "feesDebited":true,
        "gasPriceWei":crate::accounts::GAS_PRICE_WEI.to_string(),
        "feePolicy":"burn", "accountProofProfile":"glm-account-v1",
        "rentPeriodBlocks":crate::gas::RENT_PERIOD_BLOCKS.to_string(),
        "rentGasPerStorageUnitPeriod":crate::gas::RENT_GAS_PER_UNIT_PERIOD.to_string(),
        "permanentEntitiesSupported":permanent, "refunds":false, "storageRefunds":false,
        "unusedGasRefunds":true,
        "scope":"live-state logical occupancy; archive history excluded"
    });
    capabilities["sdkRpcProfile"] = json!({
        "id":"arkiv-sdk-native-v1", "endpoint":"/", "transports":["http-jsonrpc","websocket-jsonrpc"],
        "ethereumCompatibleConsensus":false, "completeEthereumApi":false,
        "signedWrites":true,
        "transactionFormats":["eip155-legacy","eip1559-empty-access-list"],
        "queryLanguage":"bounded-typed-boolean", "queryProofs":false,
        "queryOperators":["equality","inequality","ranges","prefix","and","or","not","exists","typeof"],
        "subscriptions":["newHeads","logs"], "readMode":if matches!(node,Node::Light{..}){"forwarded-unverified"}else{"local-unverified"},
        "unsupported":["permanent-entities","arbitrary-evm","contract-creation","nonempty-access-lists","ethereum-mpt-roots"],
        "feePolicy":"fixed-one-gwei; signed-prices-are-ceilings; no-priority-payment"
    });
    if permanent {
        // Present only on permanent-prefill chains; finite status is unchanged.
        capabilities["gas"]["permanentEntities"] = json!({
            "source":"genesis-prefill", "expiresAt":u64::MAX.to_string(),
            "genesisRent":"none", "expirySweep":false,
            "liveCreate":false, "liveExtendToPermanent":false,
            "liveUpdate":"rejected:PermanentStorageUnsupported", "liveDelete":true
        });
        if let Some(unsupported) = capabilities["sdkRpcProfile"]["unsupported"].as_array_mut() {
            for item in unsupported {
                if item == "permanent-entities" {
                    *item = json!("permanent-entity-writes");
                }
            }
        }
    }
    capabilities["genesisPrefill"] = json!(genesis.prefill.is_some());
    capabilities["signatures"] = json!(true);
    capabilities["proofTrust"] = json!("pinned-single-proposer-secp256k1");
    let (role, health, paused) = match node {
        Node::Full(h) => (
            if h.is_producer() { "producer" } else { "full" },
            h.health,
            h.paused,
        ),
        Node::Light { light, .. } => ("light", light.health, false),
    };
    let mut value = json!({"apiVersion":1,"feedVersion":1,"identity":node.identity(),"sourceId":node.identity().source_id,"runId":node.identity().run_id,"genesisHash":node.identity().genesis_hash,"chainId":node.identity().chain_id,"sourceKind":"arkiv-native-signed","authentication":"signed-proposer-v1","proposer":hex(&node.genesis().proposer),"consensus":false,"finality":false,"role":role,"health":health,"paused":paused,"head":snapshot(&node.head()),"durability":"durable","bodyRetention":if matches!(node,Node::Light{..}){"headers-only"}else{"all"},"proofProfiles":[crate::simulator::profile::PROOF_PROFILE,range_api::PROFILE],"capabilities":capabilities});
    // State retention of this node (a light node keeps no state: null).
    let retention = match node {
        Node::Full(h) => crate::retention::status(&h.engine),
        Node::Light { .. } => {
            json!({"stateRetention":null,"stateRetentionWindow":null,"stateFloor":null,"receiptFloor":null})
        }
    };
    if let (Some(value), Json::Object(fields)) = (value.as_object_mut(), retention) {
        value.extend(fields);
    }
    value
}
/// Details of this request's `HistoryPruned` refusal, recorded on the host
/// while the request held the node lock.
fn refusal(rt: &Shared, result: &Result<Json>) -> Option<(u64, u64)> {
    if !matches!(result, Err(Error("HistoryPruned"))) {
        return None;
    }
    match &*rt.node.try_lock().ok()? {
        Node::Full(host) => host.refusal.take(),
        Node::Light { .. } => None,
    }
}
fn respond(rt: &Shared, result: Result<Json>) -> Response {
    let refusal = refusal(rt, &result);
    response_with(result, refusal)
}
fn response(result: Result<Json>) -> Response {
    response_with(result, None)
}
fn response_with(result: Result<Json>, refusal: Option<(u64, u64)>) -> Response {
    match result {
        Ok(value) => match json_bytes(&value, RESPONSE_CAP) {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/json")],
                bytes,
            )
                .into_response(),
            Err(e) => response(Err(e)),
        },
        Err(e) => {
            let status = match e.0 {
                "Unauthorized" => StatusCode::UNAUTHORIZED,
                "HistoryPruned" => StatusCode::GONE,
                "ServerBusy" | "PoolFull" => StatusCode::TOO_MANY_REQUESTS,
                "StorageUnavailable"
                | "HistoryUnavailable"
                | "CommitUncertain"
                | super::reads::DEADLINE_EXCEEDED => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            (
                status,
                axum::Json(crate::retention::error_body(e.0, refusal)),
            )
                .into_response()
        }
    }
}
fn actor(s: &str) -> Result<OwnerId> {
    Ok(OwnerId::new(
        unhex(s, 20)?
            .try_into()
            .map_err(|_| Error("InvalidRequest"))?,
    ))
}
fn read(node: &Node, path: &str, q: &ReadQuery) -> Result<Json> {
    if path == "status" {
        return Ok(status(node));
    }
    if path == "genesis" {
        return Ok(json!(GenesisResponse {
            identity: node.identity().clone(),
            genesis_bytes: pack(node.genesis()),
            certificate_bytes: pack(&node.certificate(0)?)
        }));
    }
    if path == "prefill" {
        return match node {
            Node::Full(host) => Ok(
                json!({"prefillBytes":hex(host.prefill_stream.as_ref().ok_or(Error("PrefillBundleRequired"))?)}),
            ),
            _ => Err(Error("FullNodeRequired")),
        };
    }
    if path == "headers" || path == "feed/blocks" {
        let limit = q.limit.unwrap_or(64);
        if !(1..=64).contains(&limit) {
            return Err(Error("InvalidRequest"));
        }
        let start = if let Some(after) = &q.after_height {
            let n = number(after)?;
            if n > node.head().height {
                return Err(Error("BlockNotSynced"));
            }
            if q.after_hash.as_deref().map(hash).transpose()?
                != Some(node.certificate(n)?.header.hash())
            {
                return Err(Error("ChainConflict"));
            }
            n.checked_add(1).ok_or(Error("CapacityReached"))?
        } else {
            0
        };
        let end = node
            .head()
            .height
            .min(start.saturating_add(limit as u64 - 1));
        if path == "headers" {
            let certificates = (start..=end)
                .map(|h| node.certificate(h).map(|c| pack(&c)))
                .collect::<Result<Vec<_>>>()?;
            return Ok(json!(Headers {
                identity: node.identity().clone(),
                certificates,
                sampled: pack(&node.certificate(node.head().height)?)
            }));
        }
        let Node::Full(h) = node else {
            return Err(Error("UnsupportedNodeRole"));
        };
        let mut blocks = Vec::new();
        for height in start..=end {
            let feed = h.feed(height)?;
            blocks.push(feed);
            if json_bytes(&blocks, FEED_CAP).is_err() {
                blocks.pop();
                break;
            }
        }
        return Ok(
            json!({"identity":node.identity(),"blocks":blocks,"sampled":pack(&node.certificate(node.head().height)?)}),
        );
    }
    if let Some(height) = path.strip_prefix("feed/blocks/") {
        let Node::Full(h) = node else {
            return Err(Error("UnsupportedNodeRole"));
        };
        return Ok(json!(h.feed(number(height)?)?));
    }
    let parts: Vec<_> = path.split('/').collect();
    if let ["accounts", address] | ["accounts", address, "proof"] = parts.as_slice() {
        let Node::Full(h) = node else {
            return Err(Error("UnsupportedNodeRole"));
        };
        let height = q
            .height
            .as_deref()
            .map(number)
            .transpose()?
            .unwrap_or(h.head.height);
        return h.account_response(height, actor(address)?, parts.len() == 3);
    }
    if let ["accounts", address, "nonce"] = parts.as_slice() {
        let Node::Full(h) = node else {
            return Err(Error("UnsupportedNodeRole"));
        };
        let address = actor(address)?;
        let committed = h.committed_nonce(address)?;
        return Ok(
            json!({"actor":hex(address.as_bytes()),"committedNextNonce":committed.to_string(),"pendingNextNonce":h.pool.contiguous_next(address,committed).to_string(),"sampled":snapshot(&h.head)}),
        );
    }
    if let ["requests", address, id] = parts.as_slice() {
        let Node::Full(h) = node else {
            return Err(Error("UnsupportedNodeRole"));
        };
        return h.request_status(
            actor(address)?,
            unhex(id, 32)?
                .try_into()
                .map_err(|_| Error("InvalidRequest"))?,
        );
    }
    Err(Error("InvalidRequest"))
}
/// Bounded binary transport for a file-backed canonical genesis stream.
/// The follower verifies the complete stream against its pinned genesis.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrefillChunkQuery {
    offset: u64,
    length: u32,
}
async fn prefill_chunk(State(rt): State<Shared>, Query(q): Query<PrefillChunkQuery>) -> Response {
    const CHUNK_CAP: u32 = 1024 * 1024;
    if q.length == 0 || q.length > CHUNK_CAP {
        return response(Err(Error("InvalidRequest")));
    }
    let permit = match rt.slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(Err(Error("ServerBusy"))),
    };
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let _permit = permit;
        let (path, bytes) = {
            let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
            let Node::Full(host) = &*node else {
                return Err(Error("FullNodeRequired"));
            };
            let launch = host
                .genesis
                .prefill
                .as_ref()
                .ok_or(Error("PrefillBundleRequired"))?;
            (
                host.prefill_file
                    .clone()
                    .ok_or(Error("PrefillBundleRequired"))?,
                launch.canonical_bytes,
            )
        };
        let end = q
            .offset
            .checked_add(u64::from(q.length))
            .ok_or(Error("InvalidRequest"))?;
        if end > bytes {
            return Err(Error("InvalidRequest"));
        }
        let mut file = std::fs::File::open(path).map_err(|_| Error("StorageUnavailable"))?;
        if file
            .metadata()
            .map_err(|_| Error("StorageUnavailable"))?
            .len()
            != bytes
        {
            return Err(Error("InvalidPrefillBundle"));
        }
        file.seek(SeekFrom::Start(q.offset))
            .map_err(|_| Error("StorageUnavailable"))?;
        let mut buffer = vec![0; q.length as usize];
        file.read_exact(&mut buffer)
            .map_err(|_| Error("StorageUnavailable"))?;
        Ok(buffer)
    })
    .await
    .unwrap_or(Err(Error("StorageUnavailable")));
    match result {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(error) => response(Err(error)),
    }
}
async fn http_read(
    State(rt): State<Shared>,
    Path(path): Path<String>,
    Query(q): Query<ReadQuery>,
) -> Response {
    let permit = match rt.slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(Err(Error("ServerBusy"))),
    };
    let worker = rt.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let node = worker.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        read(&node, &path, &q)
    })
    .await
    .unwrap_or(Err(Error("StorageUnavailable")));
    respond(&rt, result)
}
async fn parse<T: serde::de::DeserializeOwned>(body: Body, cap: usize) -> Result<T> {
    let bytes = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, cap))
        .await
        .map_err(|_| Error("InvalidRequest"))?
        .map_err(|_| Error("InvalidRequest"))?;
    serde_json::from_slice(&bytes).map_err(|_| Error("InvalidRequest"))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountQuery {
    actor: String,
    height: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Submission {
    transaction_bytes: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProofRequest {
    request: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Classify {
    query: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    action: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TextQuery {
    query: String,
    require_proof: bool,
    height: String,
    namespace: String,
    limit: u32,
    cursor: Option<String>,
}
async fn http_post(State(rt): State<Shared>, Path(path): Path<String>, body: Body) -> Response {
    let admission = match rt.slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(Err(Error("ServerBusy"))),
    };
    // Detached reads hold their admission permit only while the body is
    // read; they are then evaluated under a read permit, outside the lock.
    if matches!(path.as_str(), "state/explore" | "query/eq" | "query/range") {
        let result = match path.as_str() {
            "state/explore" => state_api::handle(rt.clone(), body, admission).await,
            "query/range" => range_api::proof(rt.clone(), body, admission).await,
            _ => eq_proof(rt.clone(), body, admission).await,
        };
        return respond(&rt, result);
    }
    let permit = Arc::new(admission);
    let shared = rt.clone();
    let result:Result<Json>=async {
        if matches!(path.as_str(), "query/range/resolve" | "query/range/verified" | "query/range/inspect") { return range_api::handle(rt.clone(), &path, body, permit.clone()).await; }
        if path=="query/account" { let request: AccountQuery = parse(body, REQUEST_CAP).await?; return verified_account(rt.clone(), request).await; }
        if path=="query/classify" {let r:Classify=parse(body,REQUEST_CAP).await?;return Ok(json!(query_language::classify(&r.query)));}
        if path=="query/text" {
            let r:TextQuery=parse(body,REQUEST_CAP).await?;let classification=query_language::classify(&r.query);
            let fail=if classification.language.status!="valid"{Some("QueryLanguageError")}else if r.require_proof&&classification.proof.status!="supported"{Some("UnsupportedProof")}else if classification.execution.status!="supported"{Some("UnsupportedQueryExecution")}else{None};
            if let Some(code)=fail{return Ok(json!({"error":code,"queryText":r.query,"classification":classification,"verificationStatus":{"status":"not_executed"}}));}
            let s=classification.selection.as_ref().ok_or(Error("UnsupportedQueryExecution"))?;
            let request=VerifiedRequest {height:r.height,namespace:r.namespace,attribute:s.attribute.clone(),value_type:s.value_type.clone(),value:s.value.clone(),limit:r.limit,cursor:r.cursor};
            return match verified(rt.clone(),request,true,permit.clone()).await {Ok(mut result)=>{result["classification"]=json!(classification);result["queryText"]=json!(r.query);result["verificationStatus"]=json!({"status":"verified"});Ok(result)},Err(e)=>Ok(json!({"error":e.0,"queryText":r.query,"classification":classification,"verificationStatus":{"status":if matches!(e.0,"ProofLimitExceeded"|"BudgetExceeded"){"resource_limit"}else{"failed"}}}))};
        }
        if path=="query/verified"||path=="query/inspect" {let request=parse(body,REQUEST_CAP).await?;return verified(rt.clone(),request,path=="query/inspect",permit.clone()).await;}
        let cap=if path=="transactions"{2*super::protocol::TX_CAP as usize+4096}else{REQUEST_CAP};
        let bytes=tokio::time::timeout(Duration::from_secs(5),to_bytes(body,cap)).await.map_err(|_|Error("InvalidRequest"))?.map_err(|_|Error("InvalidRequest"))?;
        let worker_permit=permit.clone();
        tokio::task::spawn_blocking(move||{let _permit=worker_permit;
            if path=="transactions" {
                let r:Submission=serde_json::from_slice(&bytes).map_err(|_|Error("InvalidRequest"))?;
                let tx:SignedTx=unpack(&r.transaction_bytes,super::protocol::TX_CAP as usize)?;
                let genesis=rt.node.try_lock().map_err(|_|Error("ServerBusy"))?.genesis().clone();tx.validate(&genesis)?;
                let mut node=rt.node.try_lock().map_err(|_|Error("ServerBusy"))?;
                let Node::Full(h)=&mut*node else{return Err(Error("ProducerOnly"));};return h.submit(tx);
            }
            let node=rt.node.try_lock().map_err(|_|Error("ServerBusy"))?;
            if path=="query/resolve" {let r:VerifiedRequest=serde_json::from_slice(&bytes).map_err(|_|Error("InvalidRequest"))?;if r.cursor.is_some(){return Err(Error("CursorMismatch"));}let header=node.certificate(number(&r.height)?)?.header;return Ok(json!({"request":pack(&resolve(node.identity(),&header,&r)?)}));}
            Err(Error("InvalidRequest"))
        }).await.map_err(|_|Error("StorageUnavailable"))?
    }.await;
    drop(permit);
    respond(&shared, result)
}
/// `query/eq` on a full node, a detached read (see [`super::reads`]).
async fn eq_proof(
    rt: Shared,
    body: Body,
    admission: tokio::sync::OwnedSemaphorePermit,
) -> Result<Json> {
    let r: ProofRequest = parse(body, REQUEST_CAP).await?;
    let request: EqRequest = unpack(&r.request, 4096)?;
    let admitted = rt.reads.admit()?;
    drop(admission);
    admitted
        .run("query/eq", move || {
            let state = {
                let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
                let Node::Full(h) = &*node else {
                    return Err(Error("UnsupportedNodeRole"));
                };
                h.proof_state(&request.snapshot, "CursorMismatch")?
            };
            Ok(json!({"proof":hex(&super::host::eq_proof_bytes(&state, &request)?)}))
        })
        .await
}
async fn verified_account(rt: Shared, request: AccountQuery) -> Result<Json> {
    let actor = actor(&request.actor)?;
    let height = number(&request.height)?;
    let (genesis, header) = {
        let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        let Node::Light { light, .. } = &*node else {
            return Err(Error("UnsupportedNodeRole"));
        };
        (light.genesis.clone(), light.header(height)?.header)
    };
    let reply: Json = fetch(
        &rt.client,
        rt.peer.as_deref().ok_or(Error("InvalidRequest"))?,
        &format!(
            "/signed/v1/accounts/{}/proof?height={height}",
            hex(actor.as_bytes())
        ),
        None,
        None,
        RESPONSE_CAP,
    )
    .await?;
    let account_proof = unhex(
        reply["accountProof"]
            .as_str()
            .ok_or(Error("InvalidProof"))?,
        arkiv_db::proof::HOST_PROOF_CAP,
    )?;
    let supply_proof = unhex(
        reply["supplyProof"].as_str().ok_or(Error("InvalidProof"))?,
        arkiv_db::proof::HOST_PROOF_CAP,
    )?;
    let (account, burned, initial) =
        super::light::verify_account(&genesis, &header, actor, &account_proof, &supply_proof)?;
    // Confirm the accepted catalog is still usable after the network await.
    let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
    if node.certificate(height)?.header != header {
        return Err(Error("ChainConflict"));
    }
    Ok(
        json!({"actor":hex(actor.as_bytes()),"height":height.to_string(),"blockHash":hex(header.hash().as_slice()),"balanceWei":account.balance.to_string(),"nextNonce":account.nonce.to_string(),"gasPriceWei":crate::accounts::GAS_PRICE_WEI.to_string(),"initialSupplyWei":initial.to_string(),"burnedWei":burned.to_string(),"totalSupplyWei":(initial-burned).to_string(),"verificationStatus":{"status":"verified","profile":"glm-account-v1"}}),
    )
}
async fn verified(
    rt: Shared,
    r: VerifiedRequest,
    inspect: bool,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Json> {
    let (mut request, identity, genesis, header) = {
        let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        if !matches!(&*node, Node::Light { .. }) {
            return Err(Error("UnsupportedNodeRole"));
        }
        let header = node.certificate(number(&r.height)?)?.header;
        (
            resolve(node.identity(), &header, &r)?,
            node.identity().clone(),
            node.genesis().clone(),
            header,
        )
    };
    let previous = if let Some(cursor) = &r.cursor {
        let cache = rt
            .continuations
            .lock()
            .map_err(|_| Error("StorageUnavailable"))?;
        let (_, prior, token) = cache
            .iter()
            .find(|(c, _, _)| c == cursor)
            .ok_or(Error("CursorMismatch"))?;
        if prior.snapshot != request.snapshot
            || prior.query != request.query
            || prior.limit != request.limit
        {
            return Err(Error("CursorMismatch"));
        }
        request.continuation = Some(token.cursor().clone());
        Some(token.clone())
    } else {
        None
    };
    let reply: Json = fetch(
        &rt.client,
        rt.peer.as_deref().ok_or(Error("InvalidRequest"))?,
        "/signed/v1/query/eq",
        Some(json!({"request":pack(&request)})),
        None,
        RESPONSE_CAP,
    )
    .await?;
    let bytes = unhex(
        reply["proof"].as_str().ok_or(Error("InvalidRequest"))?,
        if inspect {
            crate::simulator::inspection::INSPECT_PROOF_CAP
        } else {
            RESPONSE_CAP / 2
        },
    )?;
    tokio::task::spawn_blocking(move||{let _permit=permit;
        let node=rt.node.try_lock().map_err(|_|Error("ServerBusy"))?;let Node::Light{light,..}=&*node else{return Err(Error("UnsupportedNodeRole"));};
        let verified=light.verify(&request,previous.as_ref(),&bytes)?;
        let VerifiedEq::Page(page)=verified else{return Err(Error("NamespaceNotFound"));};
        let rows=page.rows.iter().cloned().map(|r|crate::simulator::metadata::record_change(request.query.namespace,r)).collect::<Result<Vec<_>>>()?;
        let entities=if request.query.namespace.ordinal()==1 {page.rows.iter().map(|row|{let p=crate::arkiv::project(row).map_err(|_|Error("InvalidProof"))?;if p.created_at>header.height||p.updated_at>header.height||(p.expires_at!=u64::MAX&&p.expires_at<=header.height){return Err(Error("InvalidProof"));}Ok(json!({"key":hex(&p.key),"owner":hex(&p.owner),"creator":hex(&p.creator),"creationFlags":{"raw":p.creation_flags,"readonly":p.creation_flags & 1 != 0,"permissionlessExtension":p.creation_flags & 2 != 0},"createdAt":p.created_at.to_string(),"updatedAt":p.updated_at.to_string(),"expiresAt":p.expires_at.to_string(),"attributes":super::rpc_reads::verified_attributes(&p)?}))}).collect::<Result<Vec<_>>>()?}else{vec![]};
        let continuation=if let Some(token)=page.next {let mut input=encode(token.cursor());input.extend_from_slice(&request.limit.to_be_bytes());let id=hex(arkiv_db::digest("arkiv/signed-cursor/v1",&input).as_slice());let mut cache=rt.continuations.lock().map_err(|_|Error("StorageUnavailable"))?;if cache.len()>=64{cache.pop_front();}cache.push_back((id.clone(),request.clone(),token));Some(id)}else{None};
        let mut result=json!({"verification":"proof verified against authenticated single-proposer header","identity":identity,"query":{"namespace":r.namespace,"attribute":r.attribute,"valueType":r.value_type,"value":r.value,"limit":r.limit},"snapshot":snapshot(&header),"certificateBytes":pack(&node.certificate(header.height)?),"rows":rows,"entities":entities,"continuation":continuation,"postingCount":page.posting_count,"profile":"arkiv-entity-v3"});
        if inspect {let mut caps=proof_limits();caps.max_proof_bytes=crate::simulator::inspection::INSPECT_PROOF_CAP;let (_,inspection)=arkiv_db::proof::verify_eq_inspected(&request,previous.as_ref(),&bytes,caps,genesis.genesis.limits)?;result["canonicalRequest"]=json!(pack(&request));result["canonicalProof"]=json!(hex(&bytes));result["inspection"]=crate::simulator::inspection::json(&request,&inspection);}
        Ok(result)
    }).await.map_err(|_|Error("StorageUnavailable"))?
}
fn authorized(rt: &Runtime, headers: &HeaderMap) -> bool {
    rt.token.as_deref().is_some_and(|token| {
        headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some(format!("Bearer {token}").as_str())
    })
}
async fn private(
    State(rt): State<Shared>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !authorized(&rt, &headers) {
        return response(Err(Error("Unauthorized")));
    }
    let permit = match rt.slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return response(Err(Error("ServerBusy"))),
    };
    let result = async {
        let control: Option<Control> = if path == "control" {
            Some(parse(body, REQUEST_CAP).await?)
        } else {
            None
        };
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
            let Node::Full(h) = &mut *node else {
                return Err(Error("UnsupportedNodeRole"));
            };
            if let Some(height) = path.strip_prefix("replication/blocks/") {
                return Ok(json!({"blockBytes":pack(&h.block(number(height)?)?)}));
            }
            if path == "control" {
                match control.as_ref().map(|c| c.action.as_str()) {
                    Some("step") => {
                        h.produce()?;
                    }
                    Some("pause" | "resume") => {
                        if !h.is_producer() {
                            return Err(Error("ProducerOnly"));
                        }
                        h.paused = control.as_ref().is_some_and(|c| c.action == "pause");
                        if !matches!(h.health, "storage-fenced" | "chain-conflict") {
                            h.health = if h.paused { "paused" } else { "running" };
                        }
                    }
                    Some("retry") => {
                        h.retry_pending()?;
                    }
                    _ => return Err(Error("InvalidRequest")),
                }
                return Ok(status(&node));
            }
            Err(Error("InvalidRequest"))
        })
        .await
        .map_err(|_| Error("StorageUnavailable"))?
    }
    .await;
    response(result)
}
pub fn public_router(rt: Shared) -> Router {
    Router::new()
        .route("/docs", get(state_docs::html))
        .route("/docs/", get(state_docs::html))
        .route("/openapi.json", get(state_docs::openapi))
        .route(
            "/",
            axum::routing::post(super::rpc::http).get(super::rpc_ws::upgrade),
        )
        .route("/signed/v1/prefill/chunk", get(prefill_chunk))
        .route("/signed/v1/{*path}", get(http_read).post(http_post))
        .with_state(rt)
}
pub fn private_router(rt: Shared) -> Router {
    Router::new()
        .route("/signed/v1/{*path}", get(private).post(private))
        .with_state(rt)
}
pub async fn fetch<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    peer: &str,
    path: &str,
    body: Option<Json>,
    token: Option<&str>,
    cap: usize,
) -> Result<T> {
    // Bootstrap reads can race the peer's replication lock. Retry only an
    // explicit busy response, never a write or an ambiguous transport failure.
    if body.is_some() {
        return fetch_once(client, peer, path, body, token, cap).await;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        for attempt in 0..10 {
            match fetch_once(client, peer, path, None, token, cap).await {
                Err(Error("ServerBusy")) if attempt < 9 => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                result => return result,
            }
        }
        Err(Error("ServerBusy"))
    })
    .await
    .map_err(|_| Error("StorageUnavailable"))?
}
async fn fetch_once<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    peer: &str,
    path: &str,
    body: Option<Json>,
    token: Option<&str>,
    cap: usize,
) -> Result<T> {
    crate::simulator::runtime::validate_peer(peer)?;
    let url = format!("{}{}", peer.trim_end_matches('/'), path);
    let request = if let Some(body) = body {
        client.post(url).json(&body)
    } else {
        client.get(url)
    };
    let request = if let Some(token) = token {
        request.bearer_auth(token)
    } else {
        request
    };
    let mut response = request
        .send()
        .await
        .map_err(|_| Error("StorageUnavailable"))?;
    let status = response.status();
    let success = status.is_success();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error("StorageUnavailable"))?
    {
        if chunk.len() > cap.saturating_sub(bytes.len()) {
            return Err(Error("CapacityReached"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !success {
        let value: Json =
            serde_json::from_slice(&bytes).map_err(|_| Error("StorageUnavailable"))?;
        return Err(Error(match value["error"].as_str() {
            Some("ChainConflict") => "ChainConflict",
            Some("ProofLimitExceeded") => "ProofLimitExceeded",
            Some("BudgetExceeded") => "BudgetExceeded",
            Some("CursorMismatch") => "CursorMismatch",
            Some("HistoryPruned") if status == StatusCode::GONE => "HistoryPruned",
            Some("ServerBusy") if status == StatusCode::TOO_MANY_REQUESTS => "ServerBusy",
            _ => "PeerUnavailable",
        }));
    }
    serde_json::from_slice(&bytes).map_err(|_| Error("InvalidRequest"))
}
pub async fn sync_once(rt: Shared) -> Result<()> {
    let (head, identity, genesis, is_light) = {
        let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        (
            node.head(),
            node.identity().clone(),
            node.genesis().clone(),
            matches!(&*node, Node::Light { .. }),
        )
    };
    let peer = rt.peer.as_deref().ok_or(Error("InvalidRequest"))?;
    let response: Headers = fetch(
        &rt.client,
        peer,
        &format!(
            "/signed/v1/headers?afterHeight={}&afterHash={}&limit={}",
            head.height,
            hex(head.hash().as_slice()),
            if is_light { 64 } else { 1 }
        ),
        None,
        None,
        REQUEST_CAP,
    )
    .await?;
    if response.identity != identity || response.certificates.len() > if is_light { 64 } else { 1 }
    {
        return Err(Error("IdentityMismatch"));
    }
    let sampled: SignedHeader = unpack(&response.sampled, 512)?;
    sampled.authenticate(&genesis)?;
    let headers = response
        .certificates
        .iter()
        .map(|s| unpack(s, 512))
        .collect::<Result<Vec<SignedHeader>>>()?;
    for certificate in &headers {
        certificate.authenticate(&genesis)?;
    }
    if is_light {
        let mut node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        let Node::Light { light, .. } = &mut *node else {
            return Err(Error("InvalidRequest"));
        };
        return light.append(&headers, &sampled);
    }
    {
        let mut node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        if let Node::Full(host) = &mut *node {
            host.observe_certificate(&sampled)?;
            for certificate in &headers {
                // Retain every authenticated observation before fetching bodies:
                // an unavailable body must not erase evidence of equivocation.
                host.observe_certificate(certificate)?;
            }
        }
    }
    for certificate in headers {
        let reply: Json = fetch(
            &rt.client,
            rt.replication_peer
                .as_deref()
                .ok_or(Error("InvalidRequest"))?,
            &format!(
                "/signed/v1/replication/blocks/{}",
                certificate.header.height
            ),
            None,
            rt.token.as_deref(),
            RESPONSE_CAP,
        )
        .await?;
        let block: SignedBlock = unpack(
            reply["blockBytes"]
                .as_str()
                .ok_or(Error("InvalidRequest"))?,
            super::protocol::BODY_CAP as usize,
        )?;
        let mut node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        let Node::Full(host) = &mut *node else {
            return Err(Error("InvalidRequest"));
        };
        host.observe_pair(&certificate, &block.header)?;
        if block.header.header != certificate.header {
            return Err(Error("ChainConflict"));
        }
        host.import(block)?;
    }
    Ok(())
}

#[cfg(test)]
mod fetch_retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn explicit_busy_get_retries_but_posts_never_do() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let router = Router::new().route(
            "/",
            axum::routing::any(move || {
                let n = observed.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 2 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(json!({"error":"ServerBusy"})),
                        )
                    } else {
                        (StatusCode::OK, axum::Json(json!({"ok":true})))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let result: Json = fetch(&client, &peer, "/", None, None, 1024).await.unwrap();
        assert_eq!(result, json!({"ok":true}));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        calls.store(0, Ordering::SeqCst);
        let result: Result<Json> = fetch(
            &client,
            &peer,
            "/",
            Some(json!({"mutation":true})),
            None,
            1024,
        )
        .await;
        assert_eq!(result, Err(Error("ServerBusy")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }
}
