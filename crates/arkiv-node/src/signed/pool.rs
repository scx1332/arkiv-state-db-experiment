//! Bounded, volatile admission state. Selection never removes an entry; only a
//! definite commit or an eviction ([`Pool::evict_from`],
//! [`Pool::evict_unaffordable`]) does, and an evicted transaction is
//! remembered in a bounded [`Dropped`] registry with its reason.
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
/// Blocks of height drift the admission rent quote covers: each operation is
/// reserved at the larger of its rent now and this many blocks later.
pub const RENT_QUOTE_MARGIN_BLOCKS: u64 = 64;
/// Evicted transactions remembered for status and receipt lookups.
pub const DROPPED_CAP: usize = 1024;

/// Transactions that left the pool without a block, newest last: bounded and
/// volatile, so a client polling for one gets a definite refusal instead of
/// silence.
#[derive(Debug, Default)]
pub struct Dropped {
    order: std::collections::VecDeque<String>,
    by_hash: BTreeMap<String, (OwnerId, RequestId, &'static str)>,
    by_request: BTreeMap<(OwnerId, RequestId), String>,
}
impl Dropped {
    fn record(&mut self, tx: &SignedTx, reason: &'static str) {
        let hash = crate::simulator::wire::hex(tx.hash().as_slice());
        let request = (tx.tx.actor, tx.tx.request_id);
        if self
            .by_hash
            .insert(hash.clone(), (request.0, request.1, reason))
            .is_none()
        {
            self.order.push_back(hash.clone());
        }
        self.by_request.insert(request, hash);
        while self.order.len() > DROPPED_CAP {
            if let Some(old) = self.order.pop_front()
                && let Some((actor, request, _)) = self.by_hash.remove(&old)
                && self.by_request.get(&(actor, request)) == Some(&old)
            {
                self.by_request.remove(&(actor, request));
            }
        }
    }
    fn forget(&mut self, tx: &SignedTx) {
        let hash = crate::simulator::wire::hex(tx.hash().as_slice());
        if self.by_hash.remove(&hash).is_some() {
            self.order.retain(|h| *h != hash);
            self.by_request.remove(&(tx.tx.actor, tx.tx.request_id));
        }
    }
    /// Why the transaction with this hash was dropped.
    pub fn reason(&self, hash: &str) -> Option<&'static str> {
        self.by_hash
            .get(&hash.to_ascii_lowercase())
            .map(|(_, _, reason)| *reason)
    }
    /// The hash and reason of a dropped request.
    pub fn request(&self, actor: OwnerId, request: RequestId) -> Option<(&str, &'static str)> {
        let hash = self.by_request.get(&(actor, request))?;
        Some((hash, self.by_hash.get(hash)?.2))
    }
}

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
    /// The rent quoted at admission ([`super::host::Host::quote`]),
    /// reserved beside its fee and value.
    rent: alloy_primitives::U256,
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
    /// Transactions evicted without a block.
    pub dropped: Dropped,
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
            dropped: Dropped::default(),
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

    /// Funds the sender's pending transactions hold: their fees at the
    /// declared gas, their outgoing value and their quoted rent.
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
                .and_then(|sum| sum.checked_add(entry.rent))
                .ok_or(Error("BalanceOverflow"))
            })
    }
    /// The cheap admission checks of `tx`, before any state read: identity,
    /// duplicates and conflicts, the nonce window, sizes, pool capacity, and
    /// funds for every pending reservation of its sender plus its own fee
    /// and value. `Some(Duplicate)` for an exact pending retry, `None` when
    /// only its rent remains to be checked ([`Self::insert_funded`]).
    pub fn admissible(
        &self,
        tx: &SignedTx,
        committed_next: u64,
        balance: alloy_primitives::U256,
    ) -> Result<Option<Admission>> {
        if let Some(admission) = self.check(tx, committed_next)? {
            return Ok(Some(admission));
        }
        self.funded(tx, balance, alloy_primitives::U256::ZERO)?;
        Ok(None)
    }
    /// Admits `tx` if `balance` covers every pending reservation of its
    /// sender plus its own fee, value and `rent`, its rent quoted on the head
    /// state. Execution checks rent again: an operation whose rent the sender
    /// cannot pay then fails deterministically.
    pub fn insert_funded(
        &mut self,
        tx: SignedTx,
        next: u64,
        balance: alloy_primitives::U256,
        rent: alloy_primitives::U256,
    ) -> Result<Admission> {
        // Request retries and request conflicts bypass the new reservation. A valid
        // pending retry must not reserve the same funds twice.
        if self.pending(tx.tx.actor, tx.tx.request_id).is_none() {
            self.funded(&tx, balance, rent)?;
        }
        if let Some(admission) = self.check(&tx, next)? {
            return Ok(admission);
        }
        self.insert_checked(tx, rent)
    }
    fn funded(
        &self,
        tx: &SignedTx,
        balance: alloy_primitives::U256,
        rent: alloy_primitives::U256,
    ) -> Result<()> {
        let need = self
            .reserved(tx.tx.actor)?
            .checked_add(
                crate::accounts::reservation(&tx.tx).map_err(|_| Error("BalanceOverflow"))?,
            )
            .and_then(|need| need.checked_add(rent))
            .ok_or(Error("BalanceOverflow"))?;
        if need > balance {
            return Err(Error("InsufficientBalance"));
        }
        Ok(())
    }
    /// Removes `actor`'s transactions from `nonce` on, remembering them as
    /// dropped for `reason`: the one at `nonce` can never be included, and
    /// its successors cannot run before it. Never called for a transaction of
    /// a block awaiting commit.
    pub fn evict_from(&mut self, actor: OwnerId, nonce: u64, reason: &'static str) -> usize {
        let Some(entries) = self.senders.get_mut(&actor) else {
            return 0;
        };
        let evicted = entries.split_off(&nonce);
        if entries.is_empty() {
            self.senders.remove(&actor);
        }
        for entry in evicted.values() {
            self.requests.remove(&(actor, entry.tx.tx.request_id));
            self.count -= 1;
            self.bytes -= entry.bytes;
            self.dropped.record(&entry.tx, reason);
        }
        evicted.len()
    }
    /// After a commit: for every sender, keeps its pending transactions in
    /// nonce order while `balance_of` covers their cumulative reservations,
    /// and evicts the first that no longer fits with its successors
    /// (`InsufficientBalance`). Rent can exceed its quote, so a committed
    /// transaction can leave less than its successors reserved; evicting
    /// them here keeps every selected sender able to cover its reservation.
    /// Returns the number evicted.
    pub fn evict_unaffordable(
        &mut self,
        mut balance_of: impl FnMut(OwnerId) -> Result<alloy_primitives::U256>,
    ) -> Result<usize> {
        let mut cut = Vec::new();
        for (actor, entries) in &self.senders {
            let balance = balance_of(*actor)?;
            let mut need = alloy_primitives::U256::ZERO;
            for (nonce, entry) in entries {
                need = crate::accounts::reservation(&entry.tx.tx)
                    .ok()
                    .and_then(|fee| need.checked_add(fee))
                    .and_then(|sum| sum.checked_add(entry.rent))
                    .unwrap_or(alloy_primitives::U256::MAX);
                if need > balance {
                    cut.push((*actor, *nonce));
                    break;
                }
            }
        }
        Ok(cut
            .into_iter()
            .map(|(actor, nonce)| self.evict_from(actor, nonce, "InsufficientBalance"))
            .sum())
    }
    /// The structural checks of `tx`; `Some(Duplicate)` for an exact pending
    /// retry.
    fn check(&self, tx: &SignedTx, committed_next: u64) -> Result<Option<Admission>> {
        if tx.genesis_hash != self.genesis_hash || tx.tx.chain_id != self.chain_id {
            return Err(Error("IdentityMismatch"));
        }
        let hash = tx.hash();
        let actor = tx.tx.actor;
        let request = tx.tx.request_id;
        if let Some(existing) = self.pending(actor, request) {
            return if existing.hash() == hash {
                Ok(Some(Admission::Duplicate))
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
        let bytes = encode(tx).len();
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
        self.arrival
            .checked_add(1)
            .ok_or(Error("ArrivalExhausted"))?;
        Ok(None)
    }
    fn insert_checked(&mut self, tx: SignedTx, rent: alloy_primitives::U256) -> Result<Admission> {
        let hash = tx.hash();
        let actor = tx.tx.actor;
        let bytes = encode(&tx).len();
        self.dropped.forget(&tx);
        self.requests.insert((actor, tx.tx.request_id), tx.nonce);
        self.senders.entry(actor).or_default().insert(
            tx.nonce,
            Entry {
                tx,
                hash,
                bytes,
                arrival: self.arrival,
                rent,
            },
        );
        self.arrival += 1;
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
