//! The host boundary: one writer, a published body catalog, bounded polling.
use super::{
    DemoError, Result,
    auth::{HeaderChain, SignedHeader, TrustConfig},
    wire::*,
    workload,
};
use crate::{
    Block, BlockInputs, ChainSpec, MemoryChain, RequestIdentity, RequestIndex, Scope, Submission,
    TxEnvelope, validate,
};
use arkiv_db::proof::{
    EqQuery, EqRequest, ProofLimits, SnapshotRef, VerifiedContinuation, VerifiedEq, verify_eq,
};
use arkiv_db::{B256, Budget, CellName, NamespaceId, Projection, Query, Row, Term, Value, encode};
use ed25519_dalek::SigningKey;
use serde_json::{Value as Json, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Producer,
    Full,
    Light,
}
impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Producer => "producer",
            Self::Full => "full",
            Self::Light => "light",
        }
    }
}

pub struct Node {
    pub role: Role,
    pub headers: HeaderChain,
    /// Absent for light nodes. Light synchronization never requests bodies.
    pub chain: Option<MemoryChain>,
    signer: Option<SigningKey>,
    blocks: Vec<Option<SignedBlock>>,
    queue: VecDeque<TxEnvelope>,
    pending: RequestIndex,
    queued_bytes: usize,
    continuations: BTreeMap<(B256, B256), VerifiedContinuation>,
    pub capacity: usize,
    pub paused: bool,
    pub health: String,
    pub observed: u64,
    pub peer: Option<String>,
    pub seed: Option<u64>,
    pub response_cap: usize,
    last_progress: Instant,
}
pub type Shared = Arc<Mutex<Node>>;

fn compatible(spec: &ChainSpec) -> Result<()> {
    let p = spec.params();
    let l = spec.limits();
    let body_bound = u64::from(p.max_block_input_bytes)
        .checked_add(
            u64::from(p.max_ops_per_block)
                .checked_mul(256)
                .ok_or(DemoError::InvalidRequest)?,
        )
        .and_then(|n| n.checked_add(u64::from(p.expiry_per_block) * 128))
        .and_then(|n| n.checked_add(u64::from(p.max_admin_per_block) * 512))
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(4096))
        .ok_or(DemoError::InvalidRequest)?;
    if body_bound > RESPONSE_CAP as u64
        || u64::from(p.max_tx_bytes) * 2 + 1024 > REQUEST_CAP as u64
        || l.max_cells > 64
        || l.max_value_len > 65536
        || l.max_page > 4096
    {
        return Err(DemoError::InvalidRequest);
    }
    Ok(())
}
impl Node {
    pub fn producer(
        trust: TrustConfig,
        signer: SigningKey,
        capacity: usize,
        seed: Option<u64>,
    ) -> Result<Self> {
        let spec = trust.validate()?;
        compatible(&spec)?;
        if signer.verifying_key().to_bytes() != trust.public_key {
            return Err(DemoError::InvalidHeader);
        }
        let signed = SignedHeader::sign(trust.genesis_hash, *spec.genesis_header(), &signer);
        Ok(Self {
            role: Role::Producer,
            headers: HeaderChain::new(trust, signed, capacity)?,
            chain: Some(MemoryChain::new(spec)),
            signer: Some(signer),
            blocks: vec![None],
            queue: VecDeque::new(),
            pending: RequestIndex::new(),
            queued_bytes: 0,
            continuations: BTreeMap::new(),
            capacity,
            paused: false,
            health: "running".into(),
            observed: 0,
            peer: None,
            seed,
            response_cap: RESPONSE_CAP,
            last_progress: Instant::now(),
        })
    }
    pub fn follower(
        role: Role,
        trust: TrustConfig,
        genesis: SignedHeader,
        capacity: usize,
        peer: String,
    ) -> Result<Self> {
        if role == Role::Producer {
            return Err(DemoError::InvalidRequest);
        }
        let spec = trust.validate()?;
        compatible(&spec)?;
        let headers = HeaderChain::new(trust, genesis, capacity)?;
        // The light branch never constructs Engine or MemoryChain.
        let chain = if role == Role::Full {
            Some(MemoryChain::new(spec))
        } else {
            None
        };
        Ok(Self {
            role,
            headers,
            chain,
            signer: None,
            blocks: vec![None],
            queue: VecDeque::new(),
            pending: RequestIndex::new(),
            queued_bytes: 0,
            continuations: BTreeMap::new(),
            capacity,
            paused: false,
            health: "syncing".into(),
            observed: 0,
            peer: Some(peer),
            seed: None,
            response_cap: RESPONSE_CAP,
            last_progress: Instant::now(),
        })
    }
    pub fn height(&self) -> u64 {
        self.chain
            .as_ref()
            .map_or(self.headers.head().header.height, MemoryChain::height)
    }
    fn header(&self, height: u64) -> Result<&crate::Header> {
        if height > self.height() {
            return Err(DemoError::BlockNotSynced);
        }
        self.headers
            .at(height)
            .map(|h| &h.header)
            .ok_or(DemoError::UnknownBlock)
    }
    pub fn status(&self) -> Status {
        let h = self.header(self.height()).expect("accepted head");
        Status {
            role: self.role.name().into(),
            genesis_hash: self.headers.trust().genesis_hash.to_string(),
            height: h.height.to_string(),
            hash: h.hash().to_string(),
            root: h.state_root.expect("v2").to_string(),
            health: self.health.clone(),
            observed_peer_height: self.observed.to_string(),
            lag: self.observed.saturating_sub(h.height).to_string(),
            retained_from: "0".into(),
            retained_through: h.height.to_string(),
            capacity: self.capacity.to_string(),
            memory: "retained in memory; all history, no pruning".into(),
            trust: if self.role == Role::Light {
                "producer authenticated; single-producer development chain"
            } else {
                "execution checked; single-producer development chain"
            }
            .into(),
            protocol_version: 2,
            authentication_version: 1,
            proof_profiles: vec!["eq-page-v1".into()],
            pending: self.queue.len().to_string(),
            conflict_evidence: self.headers.conflicts().iter().map(packed).collect(),
        }
    }
    pub fn submit(&mut self, tx: TxEnvelope) -> Result<Json> {
        if self.role != Role::Producer {
            return Err(DemoError::InvalidRequest);
        }
        let chain = self.chain.as_ref().expect("producer");
        validate::check_tx(chain.spec(), &tx).map_err(|_| DemoError::InvalidRequest)?;
        let identity = RequestIdentity {
            scope: Scope::Tx,
            chain_id: tx.chain_id,
            actor: tx.actor,
            request_id: tx.request_id,
        };
        let bytes = encode(&tx).len();
        let mut index = self.pending.clone();
        let submission = index.submit(identity, tx.digest());
        match submission {
            Submission::Conflict { .. } => return Err(DemoError::RequestConflict),
            Submission::Accepted => {
                if self.queue.len() >= QUEUE_COUNT
                    || self
                        .queued_bytes
                        .checked_add(bytes)
                        .is_none_or(|n| n > QUEUE_BYTES)
                {
                    return Err(DemoError::QueueFull);
                }
                self.pending = index;
                self.queued_bytes += bytes;
                self.queue.push_back(tx);
            }
            _ => {}
        }
        let mut result = request_status_json(self.pending.status(&identity));
        result["identity"] = Json::String(packed(&identity));
        result["durability"] = Json::String("pending/included in this volatile run".into());
        Ok(result)
    }
    /// Timer and manual triggers share this path under the same mutex.
    pub fn produce(&mut self) -> Result<SignedBlock> {
        if self.role != Role::Producer {
            return Err(DemoError::InvalidRequest);
        }
        let height = self
            .height()
            .checked_add(1)
            .ok_or(DemoError::CapacityReached)?;
        if height >= self.capacity as u64 {
            self.health = "CapacityReached".into();
            return Err(DemoError::CapacityReached);
        }
        let chain = self.chain.as_ref().expect("producer");
        let mut inputs = if let Some(seed) = self.seed {
            workload::inputs(chain.spec().chain_id(), height, seed)
        } else {
            BlockInputs::default()
        };
        let work_count = inputs.txs.len();
        inputs.txs.extend(self.queue.iter().cloned());
        let selected = crate::select(chain.spec().params(), &inputs.admin, &inputs.txs);
        inputs.txs.truncate(selected.txs);
        let included_pending = selected.txs.saturating_sub(work_count);
        let timestamp = chain
            .head()
            .timestamp_ms
            .checked_add(1000)
            .ok_or(DemoError::ProductionFailed)?;
        let prepared = chain
            .prepare(timestamp, inputs)
            .map_err(|_| DemoError::ProductionFailed)?;
        let mut signed = SignedHeader {
            genesis_hash: self.headers.trust().genesis_hash,
            header: prepared.block().header,
            signature: [0; 64],
        };
        let mut body = SignedBlock {
            block: packed(prepared.block()),
            signed_header: packed(&signed),
        };
        let response = RpcResponse::from_result(json(&Blocks {
            blocks: vec![body.clone()],
            sampled_height: u64::MAX.to_string(),
            sampled_hash: signed.header.hash().to_string(),
        }));
        // Fixed-size signature plus longest sampled-height metadata ensures
        // this body remains servable even after the chain grows. Check the
        // exact expanded JSON size before signing.
        if serde_json::to_vec(&response)
            .map_err(|_| DemoError::ProductionFailed)?
            .len()
            > self.response_cap
        {
            return Err(DemoError::ProofLimitExceeded);
        }
        let mut pending = self.pending.clone();
        pending
            .commit_block(chain.spec().chain_id(), height, &prepared.block().inputs)
            .map_err(|_| DemoError::RequestConflict)?;
        self.blocks
            .try_reserve(1)
            .map_err(|_| DemoError::StorageUnavailable)?;
        signed = SignedHeader::sign(
            signed.genesis_hash,
            signed.header,
            self.signer.as_ref().expect("producer key"),
        );
        body.signed_header = packed(&signed);
        self.chain
            .as_mut()
            .expect("producer")
            .finalize(prepared)
            .map_err(|_| DemoError::ProductionFailed)?;
        self.headers.accept(signed)?;
        self.blocks.push(Some(body.clone()));
        self.pending = pending;
        for _ in 0..included_pending {
            if let Some(tx) = self.queue.pop_front() {
                self.queued_bytes -= encode(&tx).len();
            }
        }
        self.observed = height;
        self.health = "running".into();
        Ok(body)
    }
    pub fn apply_body(&mut self, body: SignedBlock) -> Result<bool> {
        if self.role != Role::Full {
            return Err(DemoError::InvalidRequest);
        }
        let signed: SignedHeader = unpack(&body.signed_header, 1024)?;
        signed.authenticate(self.headers.trust())?;
        if signed.header.height > self.height()
            && self.height().checked_add(1) != Some(signed.header.height)
        {
            return Err(DemoError::InvalidHeader);
        }
        if signed.header.height >= self.capacity as u64 {
            return Err(DemoError::CapacityReached);
        }
        // Retain authenticated equivocation evidence even when its accompanying
        // body is unavailable or malformed. This never advances executed head.
        self.headers.accept(signed.clone())?;
        let block: Block = unpack(&body.block, RESPONSE_CAP / 2)?;
        if block.header != signed.header
            || block.inputs.digest() != block.header.inputs_digest
            || block.outcomes.digest() != block.header.outcomes_digest
        {
            return Err(DemoError::InvalidHeader);
        }
        if signed.header.height <= self.height() {
            return Ok(false);
        }
        self.chain
            .as_mut()
            .expect("full engine")
            .apply(&block)
            .map_err(|_| DemoError::InvalidHeader)?;
        self.blocks.push(Some(body));
        Ok(true)
    }
    fn check_snapshot(&self, request: &EqRequest) -> Result<()> {
        let s = &request.snapshot;
        let h = self.header(s.height)?;
        if s.genesis_hash != self.headers.trust().genesis_hash
            || s.block_hash != h.hash()
            || Some(s.state_root) != h.state_root
        {
            return Err(DemoError::InvalidRequest);
        }
        Ok(())
    }
    fn resolve(
        &self,
        height: String,
        namespace: String,
        attribute: String,
        value_type: String,
        value: String,
        limit: u32,
    ) -> Result<Json> {
        let height = if height == "latest" {
            self.height()
        } else {
            number(&height)?
        };
        let h = self.header(height)?;
        let value = match value_type.as_str() {
            "bool" => Value::Bool(match value.as_str() {
                "true" => true,
                "false" => false,
                _ => return Err(DemoError::InvalidRequest),
            }),
            "u64" => Value::U64(number(&value)?),
            "i64" => Value::I64(value.parse().map_err(|_| DemoError::InvalidRequest)?),
            "str" => Value::str(&value),
            "bytes" => return Err(DemoError::UnsupportedProof),
            other => match arkiv_db::TypeTag::from_name(other) {
                Some(_) => crate::value_json::from_json(other, &Json::String(value))
                    .ok_or(DemoError::InvalidRequest)?,
                None => return Err(DemoError::UnsupportedProof),
            },
        };
        let request = EqRequest {
            snapshot: SnapshotRef {
                genesis_hash: self.headers.trust().genesis_hash,
                height,
                block_hash: h.hash(),
                state_root: h.state_root.expect("v2"),
            },
            query: EqQuery {
                namespace: NamespaceId::from_ordinal(number(&namespace)?)
                    .ok_or(DemoError::InvalidRequest)?,
                attribute: CellName::new(&attribute).map_err(|_| DemoError::InvalidRequest)?,
                value,
            },
            limit,
            continuation: None,
        };
        Ok(
            json!({"request":packed(&request), "snapshot":packed(&request.snapshot), "height": height.to_string(), "root": request.snapshot.state_root.to_string()}),
        )
    }
    fn query_local(&self, request: &EqRequest) -> Result<Json> {
        self.check_snapshot(request)?;
        let chain = self.chain.as_ref().ok_or(DemoError::UnsupportedProof)?;
        if request.limit == 0
            || request.limit > 64
            || request.limit as usize > chain.spec().limits().max_page
        {
            return Err(DemoError::InvalidRequest);
        }
        let snapshot = chain
            .snapshot(request.snapshot.height)
            .ok_or(DemoError::HistoryUnavailable)?;
        let q = Query::term(Term::eq(
            request.query.attribute.clone(),
            request.query.value.clone(),
        ));
        let mut cursor = None;
        let mut rows = Vec::new();
        let mut spent = 0u64;
        // This is intentionally the ordinary engine read, independent of proof assembly.
        loop {
            let read = snapshot
                .try_query(
                    request.query.namespace,
                    &q,
                    &Projection::ALL,
                    chain.spec().limits().max_page.min(64),
                    cursor.as_ref(),
                    Budget::limit(1_000_000u64.saturating_sub(spent)),
                )
                .map_err(|_| DemoError::StorageUnavailable)?;
            let read = match read {
                Ok(read) => read,
                Err(e) if e.reason == arkiv_db::Rejection::UnknownNamespace => {
                    return Ok(
                        json!({"outcome":"NamespaceNotFound", "execution":"execution checked"}),
                    );
                }
                Err(_) => return Err(DemoError::BudgetExceeded),
            };
            spent = spent
                .checked_add(read.receipt.spent.units())
                .ok_or(DemoError::BudgetExceeded)?;
            rows.extend(read.value.rows);
            cursor = read.value.next;
            if rows.len() > 4096 {
                return Err(DemoError::ProofLimitExceeded);
            }
            if cursor.is_none() {
                break;
            }
        }
        let after = request.continuation.as_ref().map(|c| c.last_id);
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| after.is_none_or(|a| r.id > a))
            .take(request.limit as usize)
            .collect();
        Ok(json!({"outcome":"Page", "rows":rows_json(&rows), "execution":"execution checked"}))
    }
    pub fn handle(&mut self, request: Request) -> Result<Json> {
        match request {
            Request::Status {} => json(&self.status()),
            Request::Genesis {} => json(&GenesisResponse {
                signed_header: packed(self.headers.at(0).expect("genesis")),
            }),
            Request::Pause { paused } => {
                if self.role == Role::Producer {
                    return Err(DemoError::InvalidRequest);
                }
                self.paused = paused;
                self.health = if paused { "paused" } else { "syncing" }.into();
                Ok(json!({"paused": paused}))
            }
            Request::ProduceBlock {} => json(&self.produce()?),
            Request::Submit { envelope } => self.submit(unpack(&envelope, REQUEST_CAP / 2)?),
            Request::RequestStatus { identity } => {
                if self.role != Role::Producer {
                    return Err(DemoError::InvalidRequest);
                }
                let identity = unpack(&identity, 128)?;
                Ok(request_status_json(self.pending.status(&identity)))
            }
            Request::Headers {
                after_height,
                after_hash,
                limit,
            } => {
                let after = number(&after_height)?;
                if limit == 0
                    || limit > HEADER_BATCH
                    || self.header(after)?.hash() != unpack_hash(&after_hash)?
                {
                    return Err(DemoError::InvalidRequest);
                }
                let end = after.saturating_add(u64::from(limit)).min(self.height());
                let headers = ((after + 1)..=end)
                    .map(|h| packed(self.headers.at(h).expect("retained")))
                    .collect();
                json(&Headers {
                    headers,
                    sampled_height: self.height().to_string(),
                    sampled_hash: self.header(self.height())?.hash().to_string(),
                })
            }
            Request::Blocks { from_height, limit } => {
                if self.role == Role::Light {
                    return Err(DemoError::HistoryUnavailable);
                }
                let from = number(&from_height)?;
                if from == 0 || limit == 0 || limit > BLOCK_BATCH {
                    return Err(DemoError::InvalidRequest);
                }
                let mut result = Blocks {
                    blocks: Vec::new(),
                    sampled_height: self.height().to_string(),
                    sampled_hash: self.header(self.height())?.hash().to_string(),
                };
                for height in from..=from.saturating_add(u64::from(limit) - 1).min(self.height()) {
                    let body = self
                        .blocks
                        .get(height as usize)
                        .and_then(Clone::clone)
                        .ok_or(DemoError::HistoryUnavailable)?;
                    result.blocks.push(body);
                    if serde_json::to_vec(&RpcResponse::from_result(json(&result)))
                        .map_err(|_| DemoError::StorageUnavailable)?
                        .len()
                        > self.response_cap
                    {
                        result.blocks.pop();
                        break;
                    }
                }
                if from <= self.height() && result.blocks.is_empty() {
                    return Err(DemoError::HistoryUnavailable);
                }
                json(&result)
            }
            Request::ResolveQuery {
                height,
                namespace,
                attribute,
                value_type,
                value,
                limit,
            } => self.resolve(height, namespace, attribute, value_type, value, limit),
            Request::QueryLocal { request } => {
                self.query_local(&unpack(&request, REQUEST_CAP / 2)?)
            }
            Request::QueryEq { request, proof } => {
                if proof != "eq-page-v1" {
                    return Err(DemoError::UnsupportedProof);
                }
                let request: EqRequest = unpack(&request, REQUEST_CAP / 2)?;
                self.check_snapshot(&request)?;
                let chain = self.chain.as_ref().ok_or(DemoError::UnsupportedProof)?;
                let snapshot = chain
                    .snapshot(request.snapshot.height)
                    .ok_or(DemoError::HistoryUnavailable)?;
                let generated = snapshot
                    .prove_eq(&request, proof_limits(), Budget::limit(100_000_000))
                    .map_err(proof_error)?;
                json(&Proof {
                    proof: hex(&generated.bytes),
                    receipt: packed(&generated.receipt),
                })
            }
            Request::QueryVerified { .. } => Err(DemoError::InvalidRequest),
        }
    }
}
fn request_status_json(status: crate::RequestStatus) -> Json {
    match status {
        crate::RequestStatus::Unknown => json!({"status":"unknown"}),
        crate::RequestStatus::Pending { digest } => {
            json!({"status":"pending", "digest":digest.to_string()})
        }
        crate::RequestStatus::Included(inclusion) => {
            json!({"status":"included", "height":inclusion.height.to_string(), "index":inclusion.index, "digest":inclusion.digest.to_string()})
        }
    }
}
fn unpack_hash(s: &str) -> Result<B256> {
    let bytes = unhex(s, 32)?;
    if bytes.len() != 32 {
        return Err(DemoError::InvalidRequest);
    }
    Ok(B256::from_slice(&bytes))
}
fn proof_limits() -> ProofLimits {
    ProofLimits {
        max_proof_bytes: (RESPONSE_CAP - 4096) / 2,
        ..ProofLimits::default()
    }
}
fn proof_error(error: arkiv_db::proof::ProofError) -> DemoError {
    use arkiv_db::proof::ProofError as E;
    match error {
        E::UnsupportedVersion => DemoError::UnsupportedVersion,
        E::UnsupportedProof => DemoError::UnsupportedProof,
        E::InvalidRequest => DemoError::InvalidRequest,
        E::CursorMismatch => DemoError::CursorMismatch,
        E::ProofLimitExceeded => DemoError::ProofLimitExceeded,
        E::BudgetExceeded => DemoError::BudgetExceeded,
        E::StorageUnavailable => DemoError::StorageUnavailable,
        E::InvalidProof => DemoError::InvalidProof,
    }
}
pub fn rows_json(rows: &[Row]) -> Json {
    Json::Array(rows.iter().map(|row| json!({"id": row.id.ordinal().to_string(), "key":row.key.as_ref().map(|k| hex(k.as_bytes())), "cells": row.cells.iter().map(|(name,cell)| {
        // Text in every case: booleans as true/false, the rest as their
        // value_json text.
        let tag = cell.value.tag().name();
        let value = match crate::value_json::to_json(&cell.value) { Json::String(text) => text, other => other.to_string() };
        json!({"name":name.as_str(),"kind":if cell.is_attribute(){"attribute"}else{"field"},"type":tag,"value":value})
    }).collect::<Vec<_>>() })).collect())
}

#[derive(Clone)]
struct App {
    node: Shared,
    client: reqwest::Client,
    requests: Arc<tokio::sync::Semaphore>,
    proofs: Arc<tokio::sync::Semaphore>,
    verification: Arc<tokio::sync::Semaphore>,
}
async fn verified_query(app: &App, text: String, peer: Option<String>) -> Result<Json> {
    let _permit = app
        .verification
        .clone()
        .try_acquire_owned()
        .map_err(|_| DemoError::ServerBusy)?;
    let request: EqRequest = unpack(&text, REQUEST_CAP / 2)?;
    let key = (request.snapshot.block_hash, request.query.digest());
    let (peer, previous, limits) = {
        let node = app.node.try_lock().map_err(|_| DemoError::ServerBusy)?;
        if node.role != Role::Light {
            return Err(DemoError::InvalidRequest);
        }
        node.check_snapshot(&request)?;
        let previous = if request.continuation.is_some() {
            Some(
                node.continuations
                    .get(&key)
                    .cloned()
                    .ok_or(DemoError::CursorMismatch)?,
            )
        } else {
            None
        };
        (
            peer.or_else(|| node.peer.clone())
                .ok_or(DemoError::HistoryUnavailable)?,
            previous,
            node.headers.trust().genesis.limits,
        )
    };
    let started = Instant::now();
    let proof: Proof = parse(
        rpc(
            &app.client,
            &peer,
            Request::QueryEq {
                request: text,
                proof: "eq-page-v1".into(),
            },
        )
        .await?,
    )?;
    let response_micros = started.elapsed().as_micros().to_string();
    let bytes = unhex(&proof.proof, RESPONSE_CAP / 2)?;
    let byte_count = bytes.len();
    let started = Instant::now();
    let verified = verify_eq(&request, previous.as_ref(), &bytes, proof_limits(), limits)
        .map_err(proof_error)?;
    let verification_micros = started.elapsed().as_micros().to_string();
    let mut node = app.node.lock().map_err(|_| DemoError::StorageUnavailable)?;
    // Never cache response rows. Continuation state is bounded to 64 walks.
    node.continuations.remove(&key);
    let mut next_request = request.clone();
    let (outcome, rows, posting_count, next) = match verified {
        VerifiedEq::NamespaceAbsent => ("NamespaceNotFound", Vec::new(), 0, None),
        VerifiedEq::Page(page) => {
            let next = page.next.map(|token| {
                next_request.continuation = Some(token.cursor().clone());
                while node.continuations.len() >= 64 {
                    node.continuations.pop_first();
                }
                node.continuations.insert(key, token);
                packed(&next_request)
            });
            ("Page", page.rows, page.posting_count, next)
        }
    };
    Ok(
        json!({"outcome":outcome, "rows":rows_json(&rows), "nextRequest":next, "height":request.snapshot.height.to_string(), "blockHash":request.snapshot.block_hash.to_string(), "root":request.snapshot.state_root.to_string(), "verification":"query proof verified", "authentication":"producer authenticated", "proofBytes":byte_count.to_string(), "postingCount":posting_count.to_string(), "verificationMicros":verification_micros, "responseMicros":response_micros}),
    )
}
async fn dispatch(app: App, request: Request) -> Result<Json> {
    if let Request::QueryVerified { request, peer } = request {
        return verified_query(&app, request, peer).await;
    }
    let proof_permit = if matches!(
        request,
        Request::QueryEq { .. } | Request::QueryLocal { .. }
    ) {
        Some(
            app.proofs
                .clone()
                .try_acquire_owned()
                .map_err(|_| DemoError::ServerBusy)?,
        )
    } else {
        None
    };
    tokio::task::spawn_blocking(move || {
        let _permit = proof_permit;
        app.node
            .lock()
            .map_err(|_| DemoError::StorageUnavailable)?
            .handle(request)
    })
    .await
    .map_err(|_| DemoError::StorageUnavailable)?
}
async fn http_handler(
    axum::extract::State(app): axum::extract::State<App>,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let result = async {
        let _permit = app
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| DemoError::ServerBusy)?;
        let bytes = axum::body::to_bytes(request.into_body(), REQUEST_CAP)
            .await
            .map_err(|_| DemoError::ProofLimitExceeded)?;
        let request: RpcRequest =
            serde_json::from_slice(&bytes).map_err(|_| DemoError::InvalidRequest)?;
        if request.rpc_version != 1 {
            return Err(DemoError::UnsupportedVersion);
        }
        dispatch(app, request.request).await
    };
    let result = tokio::time::timeout(DEADLINE, result)
        .await
        .unwrap_or(Err(DemoError::Transport));
    let response = RpcResponse::from_result(result);
    let bytes = serde_json::to_vec(&response).unwrap_or_default();
    let bytes = if bytes.len() > RESPONSE_CAP {
        serde_json::to_vec(&RpcResponse::from_result(Err(
            DemoError::ProofLimitExceeded,
        )))
        .expect("error response")
    } else {
        bytes
    };
    ([("content-type", "application/json")], bytes).into_response()
}
/// Serve loopback by default (the CLI requires an explicit address to change it).
pub async fn serve(node: Shared, listen: &str, interval: Duration) -> Result<()> {
    let app = App {
        node: node.clone(),
        client: client()?,
        requests: Arc::new(tokio::sync::Semaphore::new(64)),
        proofs: Arc::new(tokio::sync::Semaphore::new(4)),
        verification: Arc::new(tokio::sync::Semaphore::new(2)),
    };
    let role = node.lock().map_err(|_| DemoError::StorageUnavailable)?.role;
    if role == Role::Producer && !interval.is_zero() {
        let writer = node.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            timer.tick().await;
            loop {
                timer.tick().await;
                let writer = writer.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let mut node = writer.lock().expect("writer mutex");
                    if node.health == "CapacityReached" {
                        return;
                    }
                    if let Err(e) = node.produce() {
                        node.health = e.to_string();
                    }
                })
                .await;
            }
        });
    } else if role != Role::Producer {
        let follower = node.clone();
        let client = app.client.clone();
        tokio::spawn(async move {
            loop {
                let result = sync_once(&follower, &client).await;
                let caught = match result {
                    Ok(progress) => !progress,
                    Err(error) => {
                        let mut node = follower.lock().expect("follower mutex");
                        if node.health != "paused" {
                            node.health = match error {
                                DemoError::Transport => "stalled".into(),
                                _ => error.to_string(),
                            };
                        }
                        true
                    }
                };
                if caught {
                    tokio::time::sleep(interval.max(Duration::from_millis(50)).min(DEADLINE)).await;
                } else {
                    tokio::task::yield_now().await;
                }
            }
        });
    }
    let router = axum::Router::new()
        .route("/rpc", axum::routing::post(http_handler))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|_| DemoError::Transport)?;
    axum::serve(listener, router)
        .await
        .map_err(|_| DemoError::Transport)
}
pub async fn sync_once(node: &Shared, client: &reqwest::Client) -> Result<bool> {
    let (role, peer, height, hash) = {
        let node = node.lock().map_err(|_| DemoError::StorageUnavailable)?;
        if node.paused
            || matches!(
                node.health.as_str(),
                "ChainConflict" | "InvalidHeader" | "CapacityReached"
            )
        {
            return Ok(false);
        }
        (
            node.role,
            node.peer.clone().ok_or(DemoError::InvalidRequest)?,
            node.height(),
            node.header(node.height())?.hash(),
        )
    };
    let mut progress = false;
    if role == Role::Light {
        let reply: Headers = parse(
            rpc(
                client,
                &peer,
                Request::Headers {
                    after_height: height.to_string(),
                    after_hash: hash.to_string(),
                    limit: HEADER_BATCH,
                },
            )
            .await?,
        )?;
        if reply.headers.len() > HEADER_BATCH as usize {
            return Err(DemoError::InvalidHeader);
        }
        let sampled = number(&reply.sampled_height)?;
        let sampled_hash = unpack_hash(&reply.sampled_hash)?;
        let mut node = node.lock().map_err(|_| DemoError::StorageUnavailable)?;
        if node.paused {
            return Ok(false);
        }
        for text in reply.headers {
            let signed: SignedHeader = unpack(&text, 1024)?;
            signed.authenticate(node.headers.trust())?;
            if node
                .height()
                .checked_add(1)
                .is_some_and(|next| signed.header.height > next)
            {
                // Refetch from the last accepted anchor; never skip the gap.
                return Err(DemoError::BlockNotSynced);
            }
            progress |= node.headers.accept(signed)?;
        }
        record_sample(&mut node, sampled, sampled_hash, progress)?;
    } else {
        let reply: Blocks = parse(
            rpc(
                client,
                &peer,
                Request::Blocks {
                    from_height: height
                        .checked_add(1)
                        .ok_or(DemoError::InvalidHeader)?
                        .to_string(),
                    limit: BLOCK_BATCH,
                },
            )
            .await?,
        )?;
        if reply.blocks.len() > BLOCK_BATCH as usize {
            return Err(DemoError::InvalidHeader);
        }
        let sampled = number(&reply.sampled_height)?;
        let sampled_hash = unpack_hash(&reply.sampled_hash)?;
        let mut node = node.lock().map_err(|_| DemoError::StorageUnavailable)?;
        if node.paused {
            return Ok(false);
        }
        for body in reply.blocks {
            let signed: SignedHeader = unpack(&body.signed_header, 1024)?;
            signed.authenticate(node.headers.trust())?;
            if node
                .height()
                .checked_add(1)
                .is_some_and(|next| signed.header.height > next)
            {
                return Err(DemoError::BlockNotSynced);
            }
            progress |= node.apply_body(body)?;
        }
        record_sample(&mut node, sampled, sampled_hash, progress)?;
    }
    Ok(progress)
}

fn record_sample(node: &mut Node, sampled: u64, sampled_hash: B256, progress: bool) -> Result<()> {
    // Sampled head is advisory. A valid old prefix is stale information,
    // not an authenticated fork or permission to roll back local history.
    if sampled <= node.height() && node.header(sampled)?.hash() != sampled_hash {
        return Err(DemoError::InvalidRequest);
    }
    node.observed = node.observed.max(sampled).max(node.height());
    if progress {
        node.last_progress = Instant::now();
    }
    node.health = if sampled < node.height() || node.last_progress.elapsed() >= DEADLINE {
        "stalled"
    } else if node.height() < sampled {
        "syncing"
    } else {
        "following"
    }
    .into();
    Ok(())
}
