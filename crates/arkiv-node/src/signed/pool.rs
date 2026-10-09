//! Bounded, volatile admission state. Selection never removes an entry.
use super::{
    Error, Result,
    protocol::{SignedGenesis, SignedInputs, SignedLimits, SignedTx},
};
use crate::{ChainParams, RequestId};
use arkiv_db::{B256, OwnerId, encode};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
};

pub const MAX_ENTRIES: usize = 1024;
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SENDERS: usize = 256;
pub const MAX_PER_SENDER: usize = 16;
pub const NONCE_WINDOW: u64 = 16;

/// Local capacity settings may tighten the v1 ceilings, never expand them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
    pub max_senders: usize,
    pub max_per_sender: usize,
}
impl Default for PoolLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_bytes: MAX_BYTES,
            max_senders: MAX_SENDERS,
            max_per_sender: MAX_PER_SENDER,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Inserted,
    Duplicate,
}

#[derive(Debug)]
struct Entry {
    tx: SignedTx,
    hash: B256,
    bytes: usize,
    arrival: u64,
}

#[derive(Debug)]
pub struct Pool {
    limits: PoolLimits,
    signed_limits: SignedLimits,
    params: ChainParams,
    genesis_hash: B256,
    chain_id: u64,
    senders: BTreeMap<OwnerId, BTreeMap<u64, Entry>>,
    requests: BTreeMap<(OwnerId, RequestId), u64>,
    count: usize,
    bytes: usize,
    arrival: u64,
}
impl Pool {
    pub fn new(genesis: &SignedGenesis) -> Result<Self> {
        Self::with_limits(genesis, PoolLimits::default())
    }
    pub fn with_limits(genesis: &SignedGenesis, limits: PoolLimits) -> Result<Self> {
        genesis.validate()?;
        if limits.max_entries == 0
            || limits.max_entries > MAX_ENTRIES
            || limits.max_bytes == 0
            || limits.max_bytes > MAX_BYTES
            || limits.max_senders == 0
            || limits.max_senders > MAX_SENDERS
            || limits.max_per_sender == 0
            || limits.max_per_sender > MAX_PER_SENDER
        {
            return Err(Error("InvalidPoolLimits"));
        }
        Ok(Self {
            limits,
            signed_limits: genesis.limits,
            params: genesis.genesis.params,
            genesis_hash: genesis.hash(),
            chain_id: genesis.genesis.chain_id,
            senders: BTreeMap::new(),
            requests: BTreeMap::new(),
            count: 0,
            bytes: 0,
            arrival: 0,
        })
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn sender_count(&self) -> usize {
        self.senders.len()
    }
    pub fn by_hash(&self, hash: &str) -> Option<&SignedTx> {
        self.senders
            .values()
            .flat_map(|v| v.values())
            .find(|e| crate::simulator::wire::hex(e.hash.as_slice()).eq_ignore_ascii_case(hash))
            .map(|e| &e.tx)
    }
    pub fn pending(&self, actor: OwnerId, request: RequestId) -> Option<&SignedTx> {
        let nonce = self.requests.get(&(actor, request))?;
        self.senders.get(&actor)?.get(nonce).map(|entry| &entry.tx)
    }
    pub fn contiguous_next(&self, actor: OwnerId, committed_next: u64) -> u64 {
        let Some(entries) = self.senders.get(&actor) else {
            return committed_next;
        };
        let mut next = committed_next;
        while entries.contains_key(&next) {
            let Some(incremented) = next.checked_add(1) else {
                break;
            };
            next = incremented;
        }
        next
    }

    pub fn reserved(&self, actor: OwnerId) -> Result<alloy_primitives::U256> {
        self.senders
            .get(&actor)
            .into_iter()
            .flat_map(|entries| entries.values())
            .try_fold(alloy_primitives::U256::ZERO, |sum, entry| {
                sum.checked_add(
                    crate::accounts::reservation(&entry.tx.tx)
                        .map_err(|_| Error("BalanceOverflow"))?,
                )
                .ok_or(Error("BalanceOverflow"))
            })
    }
    pub fn insert_funded(
        &mut self,
        tx: SignedTx,
        next: u64,
        balance: alloy_primitives::U256,
    ) -> Result<Admission> {
        // Request retries and request conflicts bypass the new reservation. A valid
        // pending retry must not reserve the same funds twice.
        if self.pending(tx.tx.actor, tx.tx.request_id).is_none() {
            let need = self
                .reserved(tx.tx.actor)?
                .checked_add(
                    crate::accounts::reservation(&tx.tx).map_err(|_| Error("BalanceOverflow"))?,
                )
                .ok_or(Error("BalanceOverflow"))?;
            if need > balance {
                return Err(Error("InsufficientBalance"));
            }
        }
        self.insert_inner(tx, next)
    }
    fn insert_inner(&mut self, tx: SignedTx, committed_next: u64) -> Result<Admission> {
        if tx.genesis_hash != self.genesis_hash || tx.tx.chain_id != self.chain_id {
            return Err(Error("IdentityMismatch"));
        }
        let hash = tx.hash();
        let actor = tx.tx.actor;
        let request = tx.tx.request_id;
        if let Some(existing) = self.pending(actor, request) {
            return if existing.hash() == hash {
                Ok(Admission::Duplicate)
            } else {
                Err(Error("RequestConflict"))
            };
        }
        if self
            .senders
            .get(&actor)
            .is_some_and(|entries| entries.contains_key(&tx.nonce))
        {
            return Err(Error("NonceConflict"));
        }
        if tx.nonce == u64::MAX {
            return Err(Error("NonceExhausted"));
        }
        if tx.nonce < committed_next {
            return Err(Error("NonceTooLow"));
        }
        if tx.nonce > committed_next.saturating_add(NONCE_WINDOW - 1) {
            return Err(Error("NonceTooHigh"));
        }
        let bytes = encode(&tx).len();
        let native_bytes = encode(&tx.tx).len();
        if bytes > self.signed_limits.max_tx_bytes as usize
            || bytes + 8 > self.signed_limits.max_input_bytes as usize
            || native_bytes > self.params.max_tx_bytes as usize
            || native_bytes + 8 > self.params.max_block_input_bytes as usize
            || tx.tx.ops.is_empty()
            || tx.tx.ops.len() > self.params.max_ops_per_tx as usize
            || tx.tx.ops.len() > self.params.max_ops_per_block as usize
            || tx.tx.tx_budget == 0
            || tx.tx.tx_budget > self.params.user_budget
        {
            return Err(Error("TransactionDoesNotFit"));
        }
        if self.count >= self.limits.max_entries
            || self
                .bytes
                .checked_add(bytes)
                .is_none_or(|n| n > self.limits.max_bytes)
            || self
                .senders
                .get(&actor)
                .is_some_and(|entries| entries.len() >= self.limits.max_per_sender)
            || (!self.senders.contains_key(&actor) && self.senders.len() >= self.limits.max_senders)
        {
            return Err(Error("PoolFull"));
        }
        let next_arrival = self
            .arrival
            .checked_add(1)
            .ok_or(Error("ArrivalExhausted"))?;
        self.requests.insert((actor, request), tx.nonce);
        self.senders.entry(actor).or_default().insert(
            tx.nonce,
            Entry {
                tx,
                hash,
                bytes,
                arrival: self.arrival,
            },
        );
        self.arrival = next_arrival;
        self.count += 1;
        self.bytes += bytes;
        Ok(Admission::Inserted)
    }

    /// Choose contiguous sender heads by arrival. A head that cannot fit blocks
    /// its descendants for this candidate, while other senders remain eligible.
    /// Work is bounded by sender count plus at most one visit per retained entry.
    pub fn select(
        &self,
        mut committed_nonce: impl FnMut(OwnerId) -> Result<u64>,
    ) -> Result<SignedInputs> {
        let mut ready = BinaryHeap::new();
        for (actor, entries) in &self.senders {
            let nonce = committed_nonce(*actor)?;
            if let Some(entry) = entries.get(&nonce) {
                ready.push(Reverse((entry.arrival, *actor, nonce)));
            }
        }
        let mut selected = SignedInputs::default();
        let mut budget = 0u64;
        let mut ops = 0usize;
        let mut bytes = 8usize;
        let mut native_bytes = 8usize;
        while let Some(Reverse((_, actor, nonce))) = ready.pop() {
            if selected.txs.len() >= self.params.max_txs_per_block as usize {
                break;
            }
            let entries = &self.senders[&actor];
            let entry = &entries[&nonce];
            let Some(next_budget) = budget.checked_add(entry.tx.tx.tx_budget) else {
                continue;
            };
            let next_ops = ops + entry.tx.tx.ops.len();
            let next_bytes = bytes + entry.bytes;
            let next_native_bytes = native_bytes + encode(&entry.tx.tx).len();
            if next_budget > self.params.user_budget
                || next_ops > self.params.max_ops_per_block as usize
                || next_bytes > self.signed_limits.max_input_bytes as usize
                || next_native_bytes > self.params.max_block_input_bytes as usize
            {
                continue;
            }
            selected.txs.push(entry.tx.clone());
            budget = next_budget;
            ops = next_ops;
            bytes = next_bytes;
            native_bytes = next_native_bytes;
            if let Some(next) = nonce.checked_add(1).and_then(|n| entries.get(&n)) {
                ready.push(Reverse((next.arrival, actor, next.tx.nonce)));
            }
        }
        Ok(selected)
    }

    /// Remove only the exact transactions of a definitely acknowledged commit.
    /// Calling this on a candidate before commit would violate the host contract.
    pub fn remove_committed(&mut self, inputs: &SignedInputs) {
        for tx in &inputs.txs {
            let actor = tx.tx.actor;
            if let Some(entries) = self.senders.get_mut(&actor) {
                if entries
                    .get(&tx.nonce)
                    .is_some_and(|entry| entry.hash == tx.hash())
                {
                    let entry = entries.remove(&tx.nonce).expect("entry checked above");
                    self.requests.remove(&(actor, entry.tx.tx.request_id));
                    self.count -= 1;
                    self.bytes -= entry.bytes;
                }
                if entries.is_empty() {
                    self.senders.remove(&actor);
                }
            }
        }
    }
}
