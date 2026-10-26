//! Fixed-price native GLM accounts. Absent records resolve to immutable genesis allocations.
//! The signed genesis hash authenticates those defaults; host-trie records override them.
use crate::{HostFault, OpOutcome, TxEnvelope, TxOp};
use alloy_primitives::U256;
use arkiv_db::{
    Branch, Budget, Decode, DecodeError, Decoder, Encode, Encoder, OwnerId, StorageError,
    StorageErrorKind,
};

pub const GAS_PRICE_WEI: u64 = 1_000_000_000;
/// Additional work for transfers inside multi-operation transactions.
pub const TRANSFER_GAS: u64 = 9_000;
pub const MAX_ALLOCATIONS: usize = 256;
pub const BURN_KEY: [u8; 32] = *b"arkiv/glm/burned/v1_____________";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountFault {
    InsufficientBalance,
    Overflow,
    Unsupported,
}
impl Encode for AccountFault {
    fn encode_to(&self, e: &mut Encoder) {
        e.u8(match self {
            Self::InsufficientBalance => 1,
            Self::Overflow => 2,
            Self::Unsupported => 3,
        });
    }
}
impl Decode for AccountFault {
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match d.u8()? {
            1 => Ok(Self::InsufficientBalance),
            2 => Ok(Self::Overflow),
            3 => Ok(Self::Unsupported),
            tag => Err(DecodeError::UnknownTag {
                kind: "AccountFault",
                tag,
            }),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountConfig {
    pub gas_price_wei: u64,
    pub allocations: Vec<(OwnerId, U256)>,
}
impl AccountConfig {
    pub fn new(mut allocations: Vec<(OwnerId, U256)>) -> Result<Self, AccountFault> {
        allocations.sort_by_key(|(actor, _)| *actor);
        let value = Self {
            gas_price_wei: GAS_PRICE_WEI,
            allocations,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), AccountFault> {
        if self.gas_price_wei != GAS_PRICE_WEI
            || self.allocations.len() > MAX_ALLOCATIONS
            || self.allocations.windows(2).any(|w| w[0].0 >= w[1].0)
            || self.allocations.iter().any(|(_, v)| v.is_zero())
        {
            return Err(AccountFault::Unsupported);
        }
        self.initial_supply()?;
        Ok(())
    }
    pub fn initial_supply(&self) -> Result<U256, AccountFault> {
        self.allocations.iter().try_fold(U256::ZERO, |sum, (_, n)| {
            sum.checked_add(*n).ok_or(AccountFault::Overflow)
        })
    }
    pub fn initial_balance(&self, actor: OwnerId) -> U256 {
        self.allocations
            .binary_search_by_key(&actor, |(a, _)| *a)
            .map(|i| self.allocations[i].1)
            .unwrap_or(U256::ZERO)
    }
    pub fn resolve(&self, actor: OwnerId, value: Option<Vec<u8>>) -> Result<Account, StorageError> {
        match value {
            None => Ok(Account {
                nonce: 0,
                balance: self.initial_balance(actor),
            }),
            Some(b) => arkiv_db::decode_exact(&b).map_err(|_| corrupt()),
        }
    }
    pub fn read(&self, branch: &Branch, actor: OwnerId) -> Result<Account, StorageError> {
        self.resolve(actor, branch.try_host_get(&account_key(actor))?)
    }
    pub fn reserve(&self, branch: &mut Branch, tx: &TxEnvelope) -> Result<(), crate::ExecError> {
        let mut account = self.read(branch, tx.actor)?;
        account.balance = account
            .balance
            .checked_sub(fee(tx.tx_budget))
            .ok_or(crate::ExecError::Account(AccountFault::InsufficientBalance))?;
        put(branch, tx.actor, account)?;
        Ok(())
    }
    pub fn settle(
        &self,
        branch: &mut Branch,
        tx: &TxEnvelope,
        spent: u64,
    ) -> Result<(), crate::ExecError> {
        let refund = fee(tx
            .tx_budget
            .checked_sub(spent)
            .ok_or(crate::ExecError::Account(AccountFault::Overflow))?);
        let mut account = self.read(branch, tx.actor)?;
        account.balance = account
            .balance
            .checked_add(refund)
            .ok_or(crate::ExecError::Account(AccountFault::Overflow))?;
        put(branch, tx.actor, account)?;
        let burnt = burned(branch.try_host_get(&BURN_KEY)?)?
            .checked_add(fee(spent))
            .ok_or(crate::ExecError::Account(AccountFault::Overflow))?;
        if burnt > self.initial_supply().map_err(crate::ExecError::Account)? {
            return Err(crate::ExecError::Account(AccountFault::Overflow));
        }
        branch
            .try_host_put(BURN_KEY, &burnt.to_be_bytes::<32>())?
            .map_err(|_| corrupt())?;
        Ok(())
    }
    /// Debits `amount` from `actor` and burns it: entity rent, which is not
    /// gas. The caller has checked the balance; a shortfall here is a broken
    /// invariant, never a user rejection.
    pub fn burn_from(
        &self,
        branch: &mut Branch,
        actor: OwnerId,
        amount: U256,
    ) -> Result<(), crate::ExecError> {
        let mut account = self.read(branch, actor)?;
        account.balance = account
            .balance
            .checked_sub(amount)
            .ok_or(crate::ExecError::Account(AccountFault::InsufficientBalance))?;
        put(branch, actor, account)?;
        let burnt = burned(branch.try_host_get(&BURN_KEY)?)?
            .checked_add(amount)
            .ok_or(crate::ExecError::Account(AccountFault::Overflow))?;
        if burnt > self.initial_supply().map_err(crate::ExecError::Account)? {
            return Err(crate::ExecError::Account(AccountFault::Overflow));
        }
        branch
            .try_host_put(BURN_KEY, &burnt.to_be_bytes::<32>())?
            .map_err(|_| corrupt())?;
        Ok(())
    }
    pub fn transfer(
        &self,
        branch: &mut Branch,
        actor: OwnerId,
        to: OwnerId,
        amount: U256,
        remaining: u64,
        intrinsic_covers_transfer: bool,
    ) -> Result<OpOutcome, StorageError> {
        // Standalone transfers are covered by transaction intrinsic gas. Batches
        // still meter each transfer, including failed balance checks.
        let gas = if intrinsic_covers_transfer {
            0
        } else {
            TRANSFER_GAS
        };
        let receipt = match branch.try_charge_host(gas, Budget::limit(remaining))? {
            Ok(r) => r,
            Err(r) => {
                return Ok(OpOutcome::Rejected {
                    reason: r.reason,
                    receipt: r.receipt,
                });
            }
        };
        let mut sender = self.read(branch, actor)?;
        let Some(debit) = sender.balance.checked_sub(amount) else {
            return Ok(OpOutcome::AccountRejected {
                fault: AccountFault::InsufficientBalance,
                receipt,
            });
        };
        if actor != to && !amount.is_zero() {
            let mut receiver = self.read(branch, to)?;
            let Some(credit) = receiver.balance.checked_add(amount) else {
                return Ok(OpOutcome::AccountRejected {
                    fault: AccountFault::Overflow,
                    receipt,
                });
            };
            sender.balance = debit;
            receiver.balance = credit;
            put(branch, actor, sender)?;
            put(branch, to, receiver)?;
        }
        Ok(OpOutcome::Applied {
            effect: arkiv_db::Effect::BalanceTransferred {
                to,
                amount: amount.to_be_bytes::<32>(),
            },
            receipt,
        })
    }
}
impl Encode for AccountConfig {
    fn encode_to(&self, e: &mut Encoder) {
        e.u64(self.gas_price_wei);
        e.u32(self.allocations.len() as u32);
        for (a, v) in &self.allocations {
            e.put(a);
            e.fixed(&v.to_be_bytes::<32>());
        }
    }
}
impl Decode for AccountConfig {
    const MIN_ENCODED_LEN: usize = 12;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let gas_price_wei = d.u64()?;
        let n = d.u32()? as usize;
        if n > MAX_ALLOCATIONS || n > d.remaining() / 52 {
            return Err(DecodeError::Invalid {
                kind: "AccountConfig",
                reason: "allocation count",
            });
        }
        let mut allocations = Vec::with_capacity(n);
        for _ in 0..n {
            allocations.push((d.get()?, U256::from_be_bytes(d.array::<32>()?)));
        }
        let v = Self {
            gas_price_wei,
            allocations,
        };
        v.validate().map_err(|_| DecodeError::Invalid {
            kind: "AccountConfig",
            reason: "invalid allocations or price",
        })?;
        Ok(v)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    pub nonce: u64,
    pub balance: U256,
}
impl Encode for Account {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(1);
        e.u64(self.nonce);
        e.fixed(&self.balance.to_be_bytes::<32>());
    }
}
impl Decode for Account {
    const MIN_ENCODED_LEN: usize = 44;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        if d.u32()? != 1 {
            return Err(DecodeError::Invalid {
                kind: "Account",
                reason: "version",
            });
        }
        Ok(Self {
            nonce: d.u64()?,
            balance: U256::from_be_bytes(d.array::<32>()?),
        })
    }
}
pub fn account_key(actor: OwnerId) -> [u8; 32] {
    let mut k = [0; 32];
    k[..12].copy_from_slice(b"arkiv/glm/a/");
    k[12..].copy_from_slice(actor.as_bytes());
    k
}
pub fn put(branch: &mut Branch, actor: OwnerId, account: Account) -> Result<(), StorageError> {
    branch
        .try_host_put(account_key(actor), &arkiv_db::encode(&account))?
        .map_err(|_| corrupt())?;
    Ok(())
}
pub fn burned(bytes: Option<Vec<u8>>) -> Result<U256, StorageError> {
    match bytes {
        None => Ok(U256::ZERO),
        Some(b) => Ok(U256::from_be_bytes(
            <[u8; 32]>::try_from(b).map_err(|_| corrupt())?,
        )),
    }
}
pub fn fee(gas: u64) -> U256 {
    U256::from(gas) * U256::from(GAS_PRICE_WEI)
}
/// Conservative mempool reservation: fees plus gross non-self outgoing value.
/// The pool adds the rent its admission simulation quoted
/// ([`crate::signed::pool::Pool::insert_funded`]); execution checks rent
/// again against the balance left after the gas reservation.
pub fn reservation(tx: &TxEnvelope) -> Result<U256, AccountFault> {
    tx.ops
        .iter()
        .try_fold(fee(tx.tx_budget), |sum, op| match op {
            TxOp::Transfer { to, amount } if *to != tx.actor => {
                sum.checked_add(*amount).ok_or(AccountFault::Overflow)
            }
            _ => Ok(sum),
        })
}
fn corrupt() -> StorageError {
    StorageError::new(StorageErrorKind::Corrupt, "account state")
}
pub fn unsupported() -> OpOutcome {
    OpOutcome::HostRejected {
        fault: HostFault::Account(AccountFault::Unsupported),
    }
}
