//! Bounded signed node transport. Equality and complete index-range proofs.

#[path = "range_api.rs"]
mod range_api;
#[path = "state_api.rs"]
mod state_api;
#[path = "state_docs.rs"]
mod state_docs;
use super::{
    Error, ErrorCode, Result,
    host::Host,
    light::Light,
    protocol::{SignedBlock, SignedGenesis, SignedHeader, SignedTx, ValidTx},
    reads::{ReadLimits, Reads},
};
use crate::health::Health;
use crate::http::{admit, json_body};
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
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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
    /// The node lock. Writers that keep the chain moving take it with
    /// [`Self::write_node`], reads that must not fail on a momentary holder
    /// with `read_node` and JSON-RPC requests with
    /// `rpc_node`. Transaction admission and control use
    /// `read_node` too.
    pub node: Mutex<Node>,
    /// Writers waiting in [`Self::write_node`]; reads yield to them.
    writers: AtomicUsize,
    /// Reads polling the node lock (at most one: see `read_turn`).
    waiting_reads: AtomicUsize,
    /// JSON-RPC requests polling the node lock (at most one).
    waiting_rpc: AtomicUsize,
    /// Whose turn the node lock is when a read and a JSON-RPC request both
    /// poll it: [`ANY`], [`READ`] or [`RPC`]. They alternate.
    next: AtomicU8,
    /// Queues reads for their turn to poll the node lock.
    read_turn: Mutex<()>,
    /// Queues JSON-RPC requests for their turn to poll the node lock.
    rpc_turn: Mutex<()>,
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
            return Err(ErrorCode::InvalidRequest.into());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let rpc = super::rpc::initial_state(&node)?;
        Ok(Arc::new(Self {
            node: Mutex::new(node),
            writers: AtomicUsize::new(0),
            waiting_reads: AtomicUsize::new(0),
            waiting_rpc: AtomicUsize::new(0),
            next: AtomicU8::new(ANY),
            read_turn: Mutex::new(()),
            rpc_turn: Mutex::new(()),
            peer,
            replication_peer,
            token,
            client,
            slots: Arc::new(Semaphore::new(READ_CONCURRENCY)),
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

    /// Reads polling the node lock in `read_node`.
    pub fn reads_waiting(&self) -> usize {
        self.waiting_reads.load(Ordering::SeqCst)
    }

    /// JSON-RPC requests polling the node lock in `rpc_node`.
    pub fn requests_waiting(&self) -> usize {
        self.waiting_rpc.load(Ordering::SeqCst)
    }

    /// Writers waiting for the node lock in [`Self::write_node`].
    pub fn writers_waiting(&self) -> usize {
        self.writers.load(Ordering::SeqCst)
    }

    /// Takes the node lock for a writer that keeps the chain moving (block
    /// production, follower sync), waiting for the current holder; call it
    /// from a blocking thread only. Writers are the only callers that queue
    /// on the mutex: reads and JSON-RPC requests poll it and yield while a
    /// writer waits, so a read load delays a writer by at most the request
    /// holding the lock.
    pub fn write_node(&self) -> Result<MutexGuard<'_, Node>> {
        let _writer = Count::new(&self.writers);
        self.node
            .lock()
            .map_err(|_| Error::from(ErrorCode::StorageUnavailable))
    }

    /// Takes the node lock for a read that must not fail just because the
    /// lock is held at that moment (a detached read's capture, a REST
    /// request, a light node's verified read); call it from a blocking
    /// thread only. It waits out a holder for about [`READ_LOCK_WAIT`], then
    /// answers `ServerBusy`. Reads take turns: one at a time polls the lock
    /// ([`Self::poll_node`]) while the others wait for theirs.
    pub(super) fn read_node(&self) -> Result<MutexGuard<'_, Node>> {
        let deadline = Instant::now() + READ_LOCK_WAIT;
        let _turn = self
            .read_turn
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        self.poll_node(Some(deadline), READ)
    }

    /// Takes the node lock for a JSON-RPC request, which waits for it
    /// without a deadline; from a blocking thread only. Requests take turns
    /// like reads, apart from them, so a flood of requests competes with
    /// reads like one read; when both wait they alternate.
    pub(super) fn rpc_node(&self) -> Result<MutexGuard<'_, Node>> {
        let _turn = self
            .rpc_turn
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        self.poll_node(None, RPC)
    }

    /// Polls the node lock for the read or JSON-RPC request (`kind`) whose
    /// turn it is, answering `ServerBusy` at `deadline` if any. It never
    /// queues on the mutex and does not try while a writer waits, nor while
    /// it is the other kind's turn and that one polls too. Between tries it
    /// yields for [`YIELD_FOR`], then sleeps from [`FIRST_PAUSE`] up to
    /// [`LONGEST_PAUSE`].
    fn poll_node(&self, deadline: Option<Instant>, kind: u8) -> Result<MutexGuard<'_, Node>> {
        let (mine, other) = if kind == READ {
            (&self.waiting_reads, &self.waiting_rpc)
        } else {
            (&self.waiting_rpc, &self.waiting_reads)
        };
        let mut waiting = None;
        let (started, mut pause) = (Instant::now(), FIRST_PAUSE);
        loop {
            let next = self.next.load(Ordering::SeqCst);
            let yields = next != ANY && next != kind && other.load(Ordering::SeqCst) > 0;
            if self.writers.load(Ordering::SeqCst) == 0 && !yields {
                match self.node.try_lock() {
                    Ok(node) => {
                        // Going ahead of the other kind passes it the turn.
                        let after = match (other.load(Ordering::SeqCst), kind) {
                            (0, _) => ANY,
                            (_, READ) => RPC,
                            _ => READ,
                        };
                        self.next.store(after, Ordering::SeqCst);
                        return Ok(node);
                    }
                    Err(TryLockError::Poisoned(_)) => {
                        return Err(ErrorCode::StorageUnavailable.into());
                    }
                    Err(TryLockError::WouldBlock) => {}
                }
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(ErrorCode::ServerBusy.into());
            }
            waiting.get_or_insert_with(|| Count::new(mine));
            if started.elapsed() < YIELD_FOR {
                std::thread::yield_now();
            } else {
                std::thread::sleep(pause);
                pause = (pause * 2).min(LONGEST_PAUSE);
            }
        }
    }
}

/// Whose turn the node lock is ([`Runtime::poll_node`]): either kind's, a
/// read's or a JSON-RPC request's.
const ANY: u8 = 0;
const READ: u8 = 1;
const RPC: u8 = 2;
/// How long a read in [`Runtime::read_node`] waits out a held node lock:
/// far longer than producing or importing a block holds it.
const READ_LOCK_WAIT: Duration = Duration::from_secs(1);
/// How long a poller of the node lock yields before it starts sleeping:
/// most holders release it within microseconds.
const YIELD_FOR: Duration = Duration::from_micros(50);
/// A poller's first sleep, doubled after each failed try.
const FIRST_PAUSE: Duration = Duration::from_micros(20);
/// A poller's longest sleep between tries.
const LONGEST_PAUSE: Duration = Duration::from_micros(200);
/// Counts its holder in an [`AtomicUsize`] while it lives.
struct Count<'a>(&'a AtomicUsize);

impl<'a> Count<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for Count<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs `f` under [`Runtime::read_node`] on a blocking thread that keeps the
/// request's admission `permit` until it returns, even after cancellation.
async fn with_node<T: Send + 'static>(
    rt: &Shared,
    permit: &Arc<tokio::sync::OwnedSemaphorePermit>,
    f: impl FnOnce(&Node) -> Result<T> + Send + 'static,
) -> Result<T> {
    let (rt, permit) = (rt.clone(), permit.clone());
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f(&*rt.read_node()?)
    })
    .await
    .map_err(|_| ErrorCode::StorageUnavailable)?
}

/// Runs `f` under [`Runtime::write_node`] on a blocking thread.
async fn writing<T: Send + 'static>(
    rt: &Shared,
    f: impl FnOnce(&mut Node) -> Result<T> + Send + 'static,
) -> Result<T> {
    let rt = rt.clone();
    tokio::task::spawn_blocking(move || f(&mut *rt.write_node()?))
        .await
        .map_err(|_| ErrorCode::StorageUnavailable)?
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
            return Err(ErrorCode::IdentityMismatch.into());
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
    json!({
        "height": header.height.to_string(),
        "hash": hex(header.hash().as_slice()),
        "stateRoot": hex(header.state_root.as_slice()),
    })
}

pub fn status(node: &Node) -> Json {
    let genesis = node.genesis();
    // A permanent-prefill launch (versions 3 and 4) also holds rent-free
    // permanent genesis rows; live permanent entities pay the capped rent.
    let permanent_prefill = genesis
        .prefill
        .as_ref()
        .is_some_and(|p| p.permanent_prefill());
    let params = genesis.genesis.params;
    let cap_blocks = crate::gas::RentClock::of(&params).map(|clock| clock.cap_blocks());
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
        "rentRule":"glm-rent-v3",
        // Rent is GLM, not gas: a separate debit from the sender's balance,
        // burned, outside the transaction gas budget and the block gas limit.
        "rentCharging":"sender-balance-outside-gas", "rentPolicy":"burn",
        "rentWeiPerStorageUnitDay":crate::gas::RENT_WEI_PER_UNIT_DAY.to_string(),
        "rentBlockPeriodMs":params.block_period_ms.to_string(),
        "maxRentLifetimeSeconds":params.max_rent_lifetime_seconds.to_string(),
        "maxRentLifetimeBlocks":cap_blocks.map(|n| n.to_string()),
        "rentReceiptField":"rentFee",
        // Rent is bounded by the sender's balance only, not by anything the
        // transaction signs; the pool reserves a quote with a height margin.
        "rentBound":"sender-balance",
        "rentQuoteMarginBlocks":crate::signed::pool::RENT_QUOTE_MARGIN_BLOCKS.to_string(),
        "maxNewStorageUnitsPerBlock":params.max_new_storage_units_per_block.to_string(),
        "permanentEntitiesSupported":true, "refunds":false, "storageRefunds":false,
        "unusedGasRefunds":true,
        "scope":"live-state logical occupancy; archive history excluded"
    });
    let read_mode = if matches!(node, Node::Light { .. }) {
        "forwarded-unverified"
    } else {
        "local-unverified"
    };
    capabilities["sdkRpcProfile"] = json!({
        "id": "arkiv-sdk-native-v1",
        "endpoint": "/",
        "transports": ["http-jsonrpc", "websocket-jsonrpc"],
        "ethereumCompatibleConsensus": false,
        "completeEthereumApi": false,
        "signedWrites": true,
        "transactionFormats": ["eip155-legacy", "eip1559-empty-access-list"],
        "queryLanguage": "bounded-typed-boolean",
        "queryProofs": false,
        "queryOperators": [
            "equality",
            "inequality",
            "ranges",
            "prefix",
            "and",
            "or",
            "not",
            "exists",
            "typeof",
        ],
        "subscriptions": ["newHeads", "logs"],
        "readMode": read_mode,
        "unsupported": [
            "arbitrary-evm",
            "contract-creation",
            "nonempty-access-lists",
            "ethereum-mpt-roots",
        ],
        "feePolicy": "fixed-one-gwei; signed-prices-are-ceilings; no-priority-payment",
    });
    // Every lifetime of at least `maxRentLifetimeSeconds`, permanent
    // (`expiresAt` = u64::MAX) included, pays the same capped rent.
    capabilities["gas"]["permanentEntities"] = json!({
        "expiresAt": u64::MAX.to_string(),
        "expirySweep": false,
        "liveCreate": true,
        "liveExtendToPermanent": "owner-only",
        "liveUpdate": true,
        "liveDelete": true,
        "extendPermanent": "owner-only no-op-success; a finite expiry is ExpiryShortened",
        "permissionlessExtensionLimit": concat!(
            "height + maxRentLifetimeBlocks; further, permanence included, ",
            "is owner-only (PermanenceOwnerOnly)",
        ),
        "rent": "capped-at-maxRentLifetimeSeconds",
        "genesisRows": if permanent_prefill {"sponsored-without-rent"} else {"none"},
    });
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
    let body_retention = if matches!(node, Node::Light { .. }) {
        "headers-only"
    } else {
        "all"
    };
    let mut value = json!({
        "apiVersion": 1,
        "feedVersion": 1,
        "identity": node.identity(),
        "sourceId": node.identity().source_id,
        "runId": node.identity().run_id,
        "genesisHash": node.identity().genesis_hash,
        "chainId": node.identity().chain_id,
        "sourceKind": "arkiv-native-signed",
        "authentication": "signed-proposer-v1",
        "proposer": hex(&node.genesis().proposer),
        "consensus": false,
        "finality": false,
        "role": role,
        "health": health,
        "paused": paused,
        "head": snapshot(&node.head()),
        "durability": "durable",
        "bodyRetention": body_retention,
        "proofProfiles": [crate::simulator::profile::PROOF_PROFILE, range_api::PROFILE],
        "capabilities": capabilities,
    });
    // State retention of this node (a light node keeps no state: null).
    let retention = match node {
        Node::Full(h) => crate::retention::status(&h.engine),
        Node::Light { .. } => {
            json!({
                "stateRetention": null,
                "stateRetentionWindow": null,
                "stateFloor": null,
                "receiptFloor": null,
                "stateGc": null,
            })
        }
    };
    if let (Some(value), Json::Object(fields)) = (value.as_object_mut(), retention) {
        value.extend(fields);
    }
    value
}

/// The HTTP status of a refused request.
fn http_status(code: ErrorCode) -> StatusCode {
    use ErrorCode::*;
    match code {
        Unauthorized => StatusCode::UNAUTHORIZED,
        HistoryPruned => StatusCode::GONE,
        ServerBusy | PoolFull => StatusCode::TOO_MANY_REQUESTS,
        StorageUnavailable | HistoryUnavailable | CommitUncertain | ReadDeadlineExceeded => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

fn response(result: Result<Json>) -> Response {
    crate::http::json_response(result, RESPONSE_CAP, http_status)
}

fn actor(s: &str) -> Result<OwnerId> {
    Ok(OwnerId::new(
        unhex(s, 20)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidRequest)?,
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
            Node::Full(host) => {
                let stream = host
                    .prefill_stream()
                    .ok_or(ErrorCode::PrefillBundleRequired)?;
                Ok(json!({"prefillBytes": hex(stream)}))
            }
            _ => Err(ErrorCode::FullNodeRequired.into()),
        };
    }
    if path == "headers" || path == "feed/blocks" {
        let limit = q.limit.unwrap_or(HEADER_BATCH);
        if !(1..=HEADER_BATCH).contains(&limit) {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let start = if let Some(after) = &q.after_height {
            let n = number(after)?;
            if n > node.head().height {
                return Err(ErrorCode::BlockNotSynced.into());
            }
            if q.after_hash.as_deref().map(hash).transpose()?
                != Some(node.certificate(n)?.header.hash())
            {
                return Err(ErrorCode::ChainConflict.into());
            }
            n.checked_add(1).ok_or(ErrorCode::CapacityReached)?
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
            return Err(ErrorCode::UnsupportedNodeRole.into());
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
        return Ok(json!({
            "identity": node.identity(),
            "blocks": blocks,
            "sampled": pack(&node.certificate(node.head().height)?),
        }));
    }
    if let Some(height) = path.strip_prefix("feed/blocks/") {
        let Node::Full(h) = node else {
            return Err(ErrorCode::UnsupportedNodeRole.into());
        };
        return Ok(json!(h.feed(number(height)?)?));
    }
    let parts: Vec<_> = path.split('/').collect();
    if let ["accounts", address] | ["accounts", address, "proof"] = parts.as_slice() {
        let Node::Full(h) = node else {
            return Err(ErrorCode::UnsupportedNodeRole.into());
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
            return Err(ErrorCode::UnsupportedNodeRole.into());
        };
        let address = actor(address)?;
        let committed = h.committed_nonce(address)?;
        return Ok(json!({
            "actor": hex(address.as_bytes()),
            "committedNextNonce": committed.to_string(),
            "pendingNextNonce": h.pool.contiguous_next(address, committed).to_string(),
            "sampled": snapshot(&h.head),
        }));
    }
    if let ["requests", address, id] = parts.as_slice() {
        let Node::Full(h) = node else {
            return Err(ErrorCode::UnsupportedNodeRole.into());
        };
        return h.request_status(
            actor(address)?,
            unhex(id, 32)?
                .try_into()
                .map_err(|_| ErrorCode::InvalidRequest)?,
        );
    }
    Err(ErrorCode::InvalidRequest.into())
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
    let result: Result<Vec<u8>> = async {
        if q.length == 0 || q.length > CHUNK_CAP {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let permit = admit(&rt.slots)?;
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            use std::io::{Read, Seek, SeekFrom};
            let _permit = permit;
            let (path, bytes) = {
                let node = rt.read_node()?;
                let Node::Full(host) = &*node else {
                    return Err(ErrorCode::FullNodeRequired.into());
                };
                let launch = host
                    .genesis
                    .prefill
                    .as_ref()
                    .ok_or(ErrorCode::PrefillBundleRequired)?;
                (
                    host.prefill_file()
                        .ok_or(ErrorCode::PrefillBundleRequired)?
                        .to_owned(),
                    launch.canonical_bytes,
                )
            };
            let end = q
                .offset
                .checked_add(u64::from(q.length))
                .ok_or(ErrorCode::InvalidRequest)?;
            if end > bytes {
                return Err(ErrorCode::InvalidRequest.into());
            }
            let unavailable = |error: std::io::Error| {
                ErrorCode::StorageUnavailable.caused_by(format_args!("{}: {error}", path.display()))
            };
            let mut file = std::fs::File::open(&path).map_err(unavailable)?;
            if file.metadata().map_err(unavailable)?.len() != bytes {
                return Err(ErrorCode::InvalidPrefillBundle.into());
            }
            file.seek(SeekFrom::Start(q.offset)).map_err(unavailable)?;
            let mut buffer = vec![0; q.length as usize];
            file.read_exact(&mut buffer).map_err(unavailable)?;
            Ok(buffer)
        })
        .await
        .unwrap_or(Err(ErrorCode::StorageUnavailable.into()))
    }
    .await;
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
    let worker = rt.clone();
    let result: Result<Json> = async {
        let permit = admit(&rt.slots)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // Followers poll headers and accounts here: wait out a momentary
            // holder instead of refusing them.
            let node = worker.read_node()?;
            read(&node, &path, &q)
        })
        .await
        .unwrap_or(Err(ErrorCode::StorageUnavailable.into()))
    }
    .await;
    response(result)
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
    let admission = match admit(&rt.slots) {
        Ok(permit) => permit,
        Err(busy) => return response(Err(busy)),
    };
    // Detached reads hold their admission permit only while the body is
    // read; they are then evaluated under a read permit, outside the lock.
    if matches!(path.as_str(), "state/explore" | "query/eq" | "query/range") {
        let result = match path.as_str() {
            "state/explore" => state_api::handle(rt.clone(), body, admission).await,
            "query/range" => range_api::proof(rt.clone(), body, admission).await,
            _ => eq_proof(rt.clone(), body, admission).await,
        };
        return response(result);
    }
    let permit = Arc::new(admission);
    let result: Result<Json> = async {
        if matches!(
            path.as_str(),
            "query/range/resolve" | "query/range/verified" | "query/range/inspect"
        ) {
            return range_api::handle(rt.clone(), &path, body, permit.clone()).await;
        }
        if path == "query/account" {
            let request: AccountQuery = json_body(body, REQUEST_CAP).await?;
            return verified_account(rt.clone(), request, permit.clone()).await;
        }
        if path == "query/classify" {
            let r: Classify = json_body(body, REQUEST_CAP).await?;
            return Ok(json!(query_language::classify(&r.query)));
        }
        if path == "query/text" {
            let r: TextQuery = json_body(body, REQUEST_CAP).await?;
            let classification = query_language::classify(&r.query);
            let fail = if classification.language.status != "valid" {
                Some("QueryLanguageError")
            } else if r.require_proof && classification.proof.status != "supported" {
                Some("UnsupportedProof")
            } else if classification.execution.status != "supported" {
                Some("UnsupportedQueryExecution")
            } else {
                None
            };
            if let Some(code) = fail {
                return Ok(json!({
                    "error": code,
                    "queryText": r.query,
                    "classification": classification,
                    "verificationStatus": {"status": "not_executed"},
                }));
            }
            let s = classification
                .selection
                .as_ref()
                .ok_or(ErrorCode::UnsupportedQueryExecution)?;
            let request = VerifiedRequest {
                height: r.height,
                namespace: r.namespace,
                attribute: s.attribute.clone(),
                value_type: s.value_type.clone(),
                value: s.value.clone(),
                limit: r.limit,
                cursor: r.cursor,
            };
            return match verified(rt.clone(), request, true, permit.clone()).await {
                Ok(mut result) => {
                    result["classification"] = json!(classification);
                    result["queryText"] = json!(r.query);
                    result["verificationStatus"] = json!({"status": "verified"});
                    Ok(result)
                }
                Err(e) => {
                    let status = if matches!(
                        e.code,
                        ErrorCode::ProofLimitExceeded | ErrorCode::BudgetExceeded
                    ) {
                        "resource_limit"
                    } else {
                        "failed"
                    };
                    Ok(json!({
                        "error": e.code.as_str(),
                        "queryText": r.query,
                        "classification": classification,
                        "verificationStatus": {"status": status},
                    }))
                }
            };
        }
        if path == "query/verified" || path == "query/inspect" {
            let request = json_body(body, REQUEST_CAP).await?;
            return verified(rt.clone(), request, path == "query/inspect", permit.clone()).await;
        }
        let cap = if path == "transactions" {
            2 * super::protocol::TX_CAP as usize + 4096
        } else {
            REQUEST_CAP
        };
        let bytes = tokio::time::timeout(Duration::from_secs(5), to_bytes(body, cap))
            .await
            .map_err(|_| ErrorCode::InvalidRequest)?
            .map_err(|_| ErrorCode::InvalidRequest)?;
        let worker_permit = permit.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = worker_permit;
            if path == "transactions" {
                let r: Submission =
                    serde_json::from_slice(&bytes).map_err(|_| ErrorCode::InvalidRequest)?;
                let tx: SignedTx = unpack(&r.transaction_bytes, super::protocol::TX_CAP as usize)?;
                let genesis = rt.read_node()?.genesis().clone();
                let tx = ValidTx::new(tx, &genesis)?;
                let mut node = rt.read_node()?;
                let Node::Full(h) = &mut *node else {
                    return Err(ErrorCode::ProducerOnly.into());
                };
                return h.admit(tx);
            }
            if path == "query/resolve" {
                let r: VerifiedRequest =
                    serde_json::from_slice(&bytes).map_err(|_| ErrorCode::InvalidRequest)?;
                if r.cursor.is_some() {
                    return Err(ErrorCode::CursorMismatch.into());
                }
                let node = rt.read_node()?;
                let header = node.certificate(number(&r.height)?)?.header;
                let request = resolve(node.identity(), &header, &r)?;
                return Ok(json!({"request": pack(&request)}));
            }
            Err(ErrorCode::InvalidRequest.into())
        })
        .await
        .map_err(|_| ErrorCode::StorageUnavailable)?
    }
    .await;
    drop(permit);
    response(result)
}

/// `query/eq` on a full node, a detached read (see [`super::reads`]).
async fn eq_proof(
    rt: Shared,
    body: Body,
    admission: tokio::sync::OwnedSemaphorePermit,
) -> Result<Json> {
    let r: ProofRequest = json_body(body, REQUEST_CAP).await?;
    let request: EqRequest = unpack(&r.request, 4096)?;
    let admitted = rt.reads.admit()?;
    drop(admission);
    admitted
        .run("query/eq", move || {
            let state = {
                let node = rt.read_node()?;
                let Node::Full(h) = &*node else {
                    return Err(ErrorCode::UnsupportedNodeRole.into());
                };
                h.proof_state(&request.snapshot, ErrorCode::CursorMismatch)?
            };
            Ok(json!({"proof":hex(&super::host::eq_proof_bytes(&state, &request)?)}))
        })
        .await
}

async fn verified_account(
    rt: Shared,
    request: AccountQuery,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Json> {
    let actor = actor(&request.actor)?;
    let height = number(&request.height)?;
    let (genesis, header) = with_node(&rt, &permit, move |node| {
        let Node::Light { light, .. } = node else {
            return Err(ErrorCode::UnsupportedNodeRole.into());
        };
        Ok((light.genesis.clone(), light.header(height)?.header))
    })
    .await?;
    let reply: Json = fetch(
        &rt.client,
        rt.peer.as_deref().ok_or(ErrorCode::InvalidRequest)?,
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
            .ok_or(ErrorCode::InvalidProof)?,
        arkiv_db::proof::HOST_PROOF_CAP,
    )?;
    let supply_proof = unhex(
        reply["supplyProof"]
            .as_str()
            .ok_or(ErrorCode::InvalidProof)?,
        arkiv_db::proof::HOST_PROOF_CAP,
    )?;
    let (account, burned, initial) =
        super::light::verify_account(&genesis, &header, actor, &account_proof, &supply_proof)?;
    // Confirm the accepted catalog is still usable after the network await.
    let accepted = with_node(&rt, &permit, move |node| {
        Ok(node.certificate(height)?.header)
    });
    if accepted.await? != header {
        return Err(ErrorCode::ChainConflict.into());
    }
    Ok(json!({
        "actor": hex(actor.as_bytes()),
        "height": height.to_string(),
        "blockHash": hex(header.hash().as_slice()),
        "balanceWei": account.balance.to_string(),
        "nextNonce": account.nonce.to_string(),
        "gasPriceWei": crate::accounts::GAS_PRICE_WEI.to_string(),
        "initialSupplyWei": initial.to_string(),
        "burnedWei": burned.to_string(),
        "totalSupplyWei": (initial-burned).to_string(),
        "verificationStatus": {"status": "verified", "profile": "glm-account-v1"},
    }))
}

async fn verified(
    rt: Shared,
    r: VerifiedRequest,
    inspect: bool,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Json> {
    let selection = r.clone();
    let (mut request, identity, genesis, certificate) = with_node(&rt, &permit, move |node| {
        if !matches!(node, Node::Light { .. }) {
            return Err(ErrorCode::UnsupportedNodeRole.into());
        }
        let certificate = node.certificate(number(&selection.height)?)?;
        Ok((
            resolve(node.identity(), &certificate.header, &selection)?,
            node.identity().clone(),
            node.genesis().clone(),
            certificate,
        ))
    })
    .await?;
    let header = certificate.header;
    let previous = if let Some(cursor) = &r.cursor {
        let cache = rt
            .continuations
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let (_, prior, token) = cache
            .iter()
            .find(|(c, _, _)| c == cursor)
            .ok_or(ErrorCode::CursorMismatch)?;
        if prior.snapshot != request.snapshot
            || prior.query != request.query
            || prior.limit != request.limit
        {
            return Err(ErrorCode::CursorMismatch.into());
        }
        request.continuation = Some(token.cursor().clone());
        Some(token.clone())
    } else {
        None
    };
    let reply: Json = fetch(
        &rt.client,
        rt.peer.as_deref().ok_or(ErrorCode::InvalidRequest)?,
        "/signed/v1/query/eq",
        Some(json!({"request":pack(&request)})),
        None,
        RESPONSE_CAP,
    )
    .await?;
    let bytes = unhex(
        reply["proof"].as_str().ok_or(ErrorCode::InvalidRequest)?,
        if inspect {
            crate::simulator::inspection::INSPECT_PROOF_CAP
        } else {
            RESPONSE_CAP / 2
        },
    )?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // Verified outside the node lock against the captured header, which
        // the catalog must still hold afterwards.
        let verified =
            super::light::verify_eq_at(&genesis, &header, &request, previous.as_ref(), &bytes)?;
        if rt.read_node()?.certificate(header.height)?.header != header {
            return Err(ErrorCode::ChainConflict.into());
        }
        let VerifiedEq::Page(page) = verified else {
            return Err(ErrorCode::NamespaceNotFound.into());
        };
        let rows = page
            .rows
            .iter()
            .cloned()
            .map(|r| crate::simulator::metadata::record_change(request.query.namespace, r))
            .collect::<Result<Vec<_>>>()?;
        let entities = if request.query.namespace.ordinal() == 1 {
            page.rows
                .iter()
                .map(|row| crate::entity_json::verified(row, header.height))
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![]
        };
        let continuation = if let Some(token) = page.next {
            let mut input = encode(token.cursor());
            input.extend_from_slice(&request.limit.to_be_bytes());
            let id = hex(arkiv_db::digest("arkiv/signed-cursor/v1", &input).as_slice());
            let mut cache = rt
                .continuations
                .lock()
                .map_err(|_| ErrorCode::StorageUnavailable)?;
            if cache.len() >= 64 {
                cache.pop_front();
            }
            cache.push_back((id.clone(), request.clone(), token));
            Some(id)
        } else {
            None
        };
        let mut result = json!({
            "verification": "proof verified against authenticated single-proposer header",
            "identity": identity,
            "query": {
                "namespace": r.namespace,
                "attribute": r.attribute,
                "valueType": r.value_type,
                "value": r.value,
                "limit": r.limit,
            },
            "snapshot": snapshot(&header),
            "certificateBytes": pack(&certificate),
            "rows": rows,
            "entities": entities,
            "continuation": continuation,
            "postingCount": page.posting_count,
            "profile": "arkiv-entity-v3",
        });
        if inspect {
            let mut caps = proof_limits();
            caps.max_proof_bytes = crate::simulator::inspection::INSPECT_PROOF_CAP;
            let (_, inspection) = arkiv_db::proof::verify_eq_inspected(
                &request,
                previous.as_ref(),
                &bytes,
                caps,
                genesis.genesis.limits,
            )?;
            result["canonicalRequest"] = json!(pack(&request));
            result["canonicalProof"] = json!(hex(&bytes));
            result["inspection"] = crate::simulator::inspection::json(&request, &inspection);
        }
        Ok(result)
    })
    .await
    .map_err(|_| ErrorCode::StorageUnavailable)?
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
    let result = async {
        if !authorized(&rt, &headers) {
            return Err(ErrorCode::Unauthorized.into());
        }
        let permit = admit(&rt.slots)?;
        let control: Option<Control> = if path == "control" {
            Some(json_body(body, REQUEST_CAP).await?)
        } else {
            None
        };
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut node = rt.read_node()?;
            let Node::Full(h) = &mut *node else {
                return Err(ErrorCode::UnsupportedNodeRole.into());
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
                            return Err(ErrorCode::ProducerOnly.into());
                        }
                        h.paused = control.as_ref().is_some_and(|c| c.action == "pause");
                        if !matches!(h.health, Health::StorageFenced | Health::ChainConflict) {
                            h.health = if h.paused {
                                Health::Paused
                            } else {
                                Health::Running
                            };
                        }
                    }
                    Some("retry") => {
                        h.retry_pending()?;
                    }
                    _ => return Err(ErrorCode::InvalidRequest.into()),
                }
                return Ok(status(&node));
            }
            Err(ErrorCode::InvalidRequest.into())
        })
        .await
        .map_err(|_| ErrorCode::StorageUnavailable)?
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
                Err(Error {
                    code: ErrorCode::ServerBusy,
                    ..
                }) if attempt < 9 => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                result => return result,
            }
        }
        Err(ErrorCode::ServerBusy.into())
    })
    .await
    .map_err(|_| ErrorCode::StorageUnavailable)?
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
    let unreachable = |error: reqwest::Error| ErrorCode::StorageUnavailable.caused_by(error);
    let mut response = request.send().await.map_err(unreachable)?;
    let status = response.status();
    let success = status.is_success();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
        if chunk.len() > cap.saturating_sub(bytes.len()) {
            return Err(ErrorCode::CapacityReached.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if !success {
        let value: Json =
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::StorageUnavailable)?;
        use ErrorCode::*;
        return Err(match value["error"].as_str().and_then(ErrorCode::parse) {
            Some(code @ (ChainConflict | ProofLimitExceeded | BudgetExceeded | CursorMismatch)) => {
                code
            }
            Some(HistoryPruned) if status == StatusCode::GONE => HistoryPruned,
            Some(ServerBusy) if status == StatusCode::TOO_MANY_REQUESTS => ServerBusy,
            _ => PeerUnavailable,
        }
        .into());
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::from(ErrorCode::InvalidRequest))
}

/// One follower poll. Its lock sections are writes ([`Runtime::write_node`]):
/// reads never starve a follower's sync.
pub async fn sync_once(rt: Shared) -> Result<()> {
    let (head, identity, genesis, is_light) = writing(&rt, |node| {
        Ok((
            node.head(),
            node.identity().clone(),
            node.genesis().clone(),
            matches!(node, Node::Light { .. }),
        ))
    })
    .await?;
    let peer = rt.peer.as_deref().ok_or(ErrorCode::InvalidRequest)?;
    let response: Headers = fetch(
        &rt.client,
        peer,
        &format!(
            "/signed/v1/headers?afterHeight={}&afterHash={}&limit={}",
            head.height,
            hex(head.hash().as_slice()),
            if is_light { HEADER_BATCH } else { 1 }
        ),
        None,
        None,
        REQUEST_CAP,
    )
    .await?;
    if response.identity != identity
        || response.certificates.len() > if is_light { HEADER_BATCH } else { 1 }
    {
        return Err(ErrorCode::IdentityMismatch.into());
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
        return writing(&rt, move |node| {
            let Node::Light { light, .. } = node else {
                return Err(ErrorCode::InvalidRequest.into());
            };
            light.append(&headers, &sampled)
        })
        .await;
    }
    let observed = headers.clone();
    writing(&rt, move |node| {
        if let Node::Full(host) = node {
            host.observe_certificate(&sampled)?;
            for certificate in &observed {
                // Retain every authenticated observation before fetching bodies:
                // an unavailable body must not erase evidence of equivocation.
                host.observe_certificate(certificate)?;
            }
        }
        Ok(())
    })
    .await?;
    for certificate in headers {
        let reply: Json = fetch(
            &rt.client,
            rt.replication_peer
                .as_deref()
                .ok_or(ErrorCode::InvalidRequest)?,
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
                .ok_or(ErrorCode::InvalidRequest)?,
            super::protocol::BODY_CAP as usize,
        )?;
        writing(&rt, move |node| {
            let Node::Full(host) = node else {
                return Err(ErrorCode::InvalidRequest.into());
            };
            host.observe_pair(&certificate, &block.header)?;
            if block.header.header != certificate.header {
                return Err(ErrorCode::ChainConflict.into());
            }
            host.import(block)
        })
        .await?;
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
        assert_eq!(result, Err(ErrorCode::ServerBusy.into()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }
}
