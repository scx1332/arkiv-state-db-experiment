//! Versioned Arkiv entity host. Authority and counters live in committed engine state.
mod schema;
use crate::{ExecError, Expiry, HostFault, OpOutcome};
use arkiv_db::{
    Branch, Budget, Cell, CellName, Decode, DecodeError, Decoder, Encode, Encoder, HostKey,
    NamespaceId, Projection, RecordKey, Value, WriteOp,
};
pub use schema::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArkivFault {
    Name = 1,
    Value = 2,
    Flags = 3,
    NotFound = 4,
    Expired = 5,
    NotOwner = 6,
    Readonly = 7,
    Owner = 8,
    NonceExhausted = 9,
    Corrupt = 10,
    Namespace = 11,
    Duplicate = 12,
    GasOverflow = 13,
    /// Retired: live permanent entities are supported since protocol 4. The
    /// tag stays reserved and is never produced.
    PermanentStorageUnsupported = 14,
    UnsupportedGasOperation = 15,
    /// The `bytes` cells of the resulting entity would hold more than
    /// [`MAX_ENTITY_BYTES`].
    EntityTooLarge = 16,
    /// The sender's balance, after its transaction's gas reservation, cannot
    /// pay the operation's rent. Nothing of the operation applies.
    InsufficientRentBalance = 17,
}
impl Encode for ArkivFault {
    fn encode_to(&self, e: &mut Encoder) {
        e.u8(*self as u8)
    }
}
impl Decode for ArkivFault {
    const MIN_ENCODED_LEN: usize = 1;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => Self::Name,
            2 => Self::Value,
            3 => Self::Flags,
            4 => Self::NotFound,
            5 => Self::Expired,
            6 => Self::NotOwner,
            7 => Self::Readonly,
            8 => Self::Owner,
            9 => Self::NonceExhausted,
            10 => Self::Corrupt,
            11 => Self::Namespace,
            12 => Self::Duplicate,
            13 => Self::GasOverflow,
            14 => Self::PermanentStorageUnsupported,
            15 => Self::UnsupportedGasOperation,
            16 => Self::EntityTooLarge,
            17 => Self::InsufficientRentBalance,
            tag => {
                return Err(DecodeError::UnknownTag {
                    kind: "ArkivFault",
                    tag,
                });
            }
        })
    }
}
/// One entity operation. User attributes travel as engine cells: an indexed
/// attribute (ABI `typeId` with [`INDEXED_FLAG`]) is a [`Cell::attribute`],
/// an unindexed one a [`Cell::field`], so the flag is carried, validated,
/// executed and stored as the cell kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArkivOperation {
    Create {
        salt: u128,
        flags: u8,
        expiry: Expiry,
        attributes: Vec<(CellName, Cell)>,
    },
    /// Sets (each with the indexed flag of its new triple) and removes
    /// attributes.
    Patch {
        key: RecordKey,
        set: Vec<(CellName, Cell)>,
        remove: Vec<CellName>,
    },
    ExtendExpiry {
        key: RecordKey,
        expiry: Expiry,
    },
    TransferOwnership {
        key: RecordKey,
        owner: [u8; 20],
    },
    Delete {
        key: RecordKey,
    },
}
impl ArkivOperation {
    pub fn key(&self) -> Option<&RecordKey> {
        match self {
            Self::Create { .. } => None,
            Self::Patch { key, .. }
            | Self::ExtendExpiry { key, .. }
            | Self::TransferOwnership { key, .. }
            | Self::Delete { key } => Some(key),
        }
    }
    pub fn expiry(&self) -> Option<&Expiry> {
        match self {
            Self::Create { expiry, .. } | Self::ExtendExpiry { expiry, .. } => Some(expiry),
            _ => None,
        }
    }
    pub fn validate(&self) -> Result<(), ArkivFault> {
        if self.key().is_some_and(|k| k.as_bytes().len() != 32) {
            return Err(ArkivFault::Value);
        }
        let mut names = std::collections::BTreeSet::new();
        let attrs = match self {
            Self::Create {
                flags, attributes, ..
            } => {
                if flags & !3 != 0 {
                    return Err(ArkivFault::Flags);
                }
                attributes.as_slice()
            }
            Self::Patch { set, .. } => set.as_slice(),
            _ => &[],
        };
        if attrs.len() > MAX_ATTRIBUTES {
            return Err(ArkivFault::Value);
        }
        if let Self::Patch { remove, .. } = self
            && attrs.len() + remove.len() > MAX_ATTRIBUTES
        {
            return Err(ArkivFault::Value);
        }
        for (name, cell) in attrs {
            if !names.insert(name) {
                return Err(ArkivFault::Duplicate);
            }
            validate_user_cell(name, cell)?;
        }
        // A create's values are the whole entity; a patch's set is part of it.
        if entity_bytes(attrs) > MAX_ENTITY_BYTES {
            return Err(ArkivFault::EntityTooLarge);
        }
        if let Self::Patch { remove, .. } = self {
            for name in remove {
                if !names.insert(name) {
                    return Err(ArkivFault::Duplicate);
                }
                validate_remove(name)?;
            }
        }
        Ok(())
    }
}
impl Encode for ArkivOperation {
    fn encode_to(&self, e: &mut Encoder) {
        match self {
            Self::Create {
                salt,
                flags,
                expiry,
                attributes,
            } => {
                e.u8(1);
                e.fixed(&salt.to_be_bytes());
                e.u8(*flags);
                e.put(expiry);
                e.put(attributes)
            }
            Self::Patch { key, set, remove } => {
                e.u8(2);
                e.put(key);
                e.put(set);
                e.put(remove)
            }
            Self::ExtendExpiry { key, expiry } => {
                e.u8(3);
                e.put(key);
                e.put(expiry)
            }
            Self::TransferOwnership { key, owner } => {
                e.u8(4);
                e.put(key);
                e.fixed(owner)
            }
            Self::Delete { key } => {
                e.u8(5);
                e.put(key)
            }
        }
    }
}
impl Decode for ArkivOperation {
    const MIN_ENCODED_LEN: usize = 1;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => Self::Create {
                salt: u128::from_be_bytes(d.array()?),
                flags: d.u8()?,
                expiry: d.get()?,
                attributes: d.get()?,
            },
            2 => Self::Patch {
                key: d.get()?,
                set: d.get()?,
                remove: d.get()?,
            },
            3 => Self::ExtendExpiry {
                key: d.get()?,
                expiry: d.get()?,
            },
            4 => Self::TransferOwnership {
                key: d.get()?,
                owner: d.array()?,
            },
            5 => Self::Delete { key: d.get()? },
            tag => {
                return Err(DecodeError::UnknownTag {
                    kind: "ArkivOperation",
                    tag,
                });
            }
        })
    }
}
pub const ARKIV_ADDRESS: [u8; 20] = [
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x44,
];
pub fn entity_namespace() -> NamespaceId {
    NamespaceId::from_bytes(1u64.to_be_bytes()).expect("namespace one")
}
pub fn nonce_key(caller: &[u8; 20]) -> HostKey {
    let mut key = [0u8; 32];
    key[..12].copy_from_slice(b"arkiv/nonce/");
    key[12..].copy_from_slice(caller);
    key
}
pub fn derive_entity_key(chain_id: u64, caller: &[u8; 20], nonce: u64, salt: u128) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(96);
    bytes.extend_from_slice(&[0; 24]);
    bytes.extend_from_slice(&chain_id.to_be_bytes());
    bytes.extend_from_slice(&ARKIV_ADDRESS);
    bytes.extend_from_slice(caller);
    bytes.extend_from_slice(&nonce.to_be_bytes());
    bytes.extend_from_slice(&salt.to_be_bytes());
    alloy_primitives::keccak256(bytes).0
}
#[derive(Clone, Copy, Debug)]
pub struct ArkivExecutionContext<'a> {
    pub caller: [u8; 20],
    pub block_height: u64,
    pub chain_id: u64,
    /// The chain's rent clock ([`crate::gas::RentClock::of`] its params):
    /// rent is never charged past `block_height + cap_blocks`.
    pub rent: crate::gas::RentClock,
    pub expiry_per_block: u32,
    /// The accounts that pay rent: the caller's balance is debited and the
    /// rent burned. `None` quotes rent without debiting it (estimation,
    /// chains without accounts and height-zero construction).
    pub payer: Option<&'a crate::accounts::AccountConfig>,
    /// Waives the rent of a `Create` with the permanent expiry (`u64::MAX`).
    /// Only height-zero construction of a permanent-prefill launch sets it
    /// (its permanent rows are explicitly sponsored); live blocks pass
    /// `false`, so a live permanent create pays the capped rent like any
    /// lifetime at or beyond the cap.
    pub allow_permanent_create: bool,
}
/// One executed operation and the rent it charged: nonzero only when the
/// operation applied (a payer was debited that much, or would have been).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArkivOutcome {
    pub outcome: OpOutcome,
    pub rent_wei: alloy_primitives::U256,
}
impl From<OpOutcome> for ArkivOutcome {
    fn from(outcome: OpOutcome) -> Self {
        Self {
            outcome,
            rent_wei: alloy_primitives::U256::ZERO,
        }
    }
}
fn name(s: &str) -> CellName {
    CellName::new(s).expect("system name")
}
fn corrupt() -> ExecError {
    ExecError::Storage(arkiv_db::StorageError::new(
        arkiv_db::StorageErrorKind::Corrupt,
        "Arkiv committed state is malformed",
    ))
}
fn count(branch: &Branch, key: &HostKey) -> Result<u64, ExecError> {
    match branch.try_host_get(key)? {
        None => Ok(0),
        Some(v) => Ok(u64::from_be_bytes(
            v.as_slice().try_into().map_err(|_| corrupt())?,
        )),
    }
}
fn put_count(branch: &mut Branch, key: HostKey, value: u64) -> Result<(), ExecError> {
    if value == 0 {
        branch.try_host_remove(&key)?;
    } else {
        branch
            .try_host_put(key, &value.to_be_bytes())?
            .map_err(|_| corrupt())?;
    }
    Ok(())
}
fn rejected(fault: ArkivFault) -> OpOutcome {
    OpOutcome::HostRejected {
        fault: HostFault::Arkiv(fault),
    }
}
pub(crate) fn expiry(exp: &Expiry, c: &ArkivExecutionContext<'_>) -> Result<u64, HostFault> {
    let floor = c
        .block_height
        .checked_add(exp.min_lifetime)
        .ok_or(HostFault::ExpiryInvalid(crate::ExpiryFault::Overflow))?;
    let target = exp.expires_at.max(floor);
    if target <= c.block_height {
        return Err(HostFault::ExpiryInvalid(crate::ExpiryFault::DeadOnArrival));
    }
    // No upper bound: any finite expiry, or the permanent `u64::MAX`, is a
    // valid lifetime; the rent cap bounds what it costs.
    Ok(target)
}
/// Canonical creation cells shared by live execution and bulk genesis: the
/// [`SYSTEM_CELLS`] host cells, then the user attributes as given. Callers
/// validate the operation and expiry before invoking this helper.
pub(crate) fn create_cells(
    key: [u8; 32],
    creator: [u8; 20],
    height: u64,
    expires: u64,
    flags: u8,
    attributes: &[(CellName, Cell)],
) -> Vec<(CellName, Cell)> {
    let mut cells = vec![
        (name("$key"), Cell::attribute(Value::Key(key))),
        (name("$creator"), Cell::attribute(Value::Addr(creator))),
        (name("$owner"), Cell::attribute(Value::Addr(creator))),
        (name("$createdAt"), Cell::attribute(Value::U64(height))),
        (name("$updatedAt"), Cell::attribute(Value::U64(height))),
        (name("$expiresAt"), Cell::attribute(Value::U64(expires))),
        (
            name("$creationFlags"),
            Cell::field(Value::U64(u64::from(flags))),
        ),
    ];
    debug_assert!(
        attributes
            .iter()
            .all(|(n, c)| validate_user_cell(n, c).is_ok())
    );
    cells.extend_from_slice(attributes);
    cells
}
/// Execute one operation; the enclosing transaction checkpoint provides atomic batches.
/// Engine/storage errors remain fatal and may never be downgraded to user rejections.
///
/// Under GLM pricing the operation's rent ([`crate::gas::rent`]) is quoted
/// before the engine write. It is not gas: the engine keeps the whole
/// `budget`. With a payer the caller must hold the rent after its
/// transaction's gas reservation, or the operation is refused with
/// [`ArkivFault::InsufficientRentBalance`] before anything is written;
/// after a successful write the rent is debited and burned.
pub fn execute_operation(
    branch: &mut Branch,
    namespace: NamespaceId,
    c: &ArkivExecutionContext<'_>,
    op: &ArkivOperation,
    budget: Budget,
) -> Result<ArkivOutcome, ExecError> {
    // GLM disables generic record mutations in every namespace, so all of
    // them can safely use Arkiv ownership and rent. Chains without GLM
    // pricing allow generic writes outside namespace one, so entities stay
    // confined to it there.
    if namespace != entity_namespace() && !crate::gas::is_active(branch.pricing()) {
        return Ok(rejected(ArkivFault::Namespace).into());
    }
    if let Err(f) = op.validate() {
        return Ok(rejected(f).into());
    }
    let mut current_cells = Vec::new();
    let current = if let Some(key) = op.key() {
        let row =
            match branch.try_get_record(namespace, key, &Projection::ALL, Budget::unlimited())? {
                Ok(read) => read.value,
                Err(rejected) if rejected.reason == arkiv_db::Rejection::UnknownNamespace => {
                    return Ok(self::rejected(ArkivFault::NotFound).into());
                }
                Err(_) => return Err(corrupt()),
            };
        let Some(row) = row else {
            return Ok(rejected(ArkivFault::NotFound).into());
        };
        let projected = project(&row).map_err(|_| corrupt())?;
        if projected.expires_at <= c.block_height {
            return Ok(rejected(ArkivFault::Expired).into());
        }
        if projected.owner != c.caller
            && !(matches!(op, ArkivOperation::ExtendExpiry { .. })
                && projected.creation_flags & 2 != 0)
        {
            return Ok(rejected(ArkivFault::NotOwner).into());
        }
        current_cells = row.cells;
        Some(projected)
    } else {
        None
    };
    let old = current.as_ref().map(|p| p.expires_at);
    let new = if let Some(exp) = op.expiry() {
        let target = match expiry(exp, c) {
            Ok(t) => t,
            Err(fault) => return Ok(OpOutcome::HostRejected { fault }.into()),
        };
        // Permissionless extension (creation flag 2) admits anyone's finite
        // extension; only the owner may make an entity permanent.
        if target == u64::MAX && current.as_ref().is_some_and(|p| p.owner != c.caller) {
            return Ok(rejected(ArkivFault::NotOwner).into());
        }
        if old.is_some_and(|v| target < v) {
            return Ok(OpOutcome::HostRejected {
                fault: HostFault::ExpiryShortened,
            }
            .into());
        }
        if target != u64::MAX
            && old != Some(target)
            && count(branch, &crate::exec::total_key(target))? >= u64::from(c.expiry_per_block)
        {
            return Ok(OpOutcome::HostRejected {
                fault: HostFault::ExpiryCapacityFull,
            }
            .into());
        }
        Some(target)
    } else {
        old
    };
    let mut next_nonce = None;
    let write = match op {
        ArkivOperation::Create {
            salt,
            flags,
            attributes,
            ..
        } => {
            let nonce = count(branch, &nonce_key(&c.caller))?;
            let Some(next) = nonce.checked_add(1) else {
                return Ok(rejected(ArkivFault::NonceExhausted).into());
            };
            next_nonce = Some(next);
            let key = derive_entity_key(c.chain_id, &c.caller, nonce, *salt);
            let cells = create_cells(
                key,
                c.caller,
                c.block_height,
                new.expect("create expiry"),
                *flags,
                attributes,
            );
            WriteOp::CreateRecord {
                namespace,
                key: RecordKey::new(&key).expect("32-byte key"),
                cells,
            }
        }
        ArkivOperation::Delete { key } => WriteOp::DeleteRecord {
            namespace,
            key: key.clone(),
        },
        _ => {
            let p = current.as_ref().expect("mutation entity");
            let mut set = vec![(
                name("$updatedAt"),
                Cell::attribute(Value::U64(c.block_height)),
            )];
            let mut remove = Vec::new();
            match op {
                ArkivOperation::Patch {
                    set: changes,
                    remove: deleted,
                    ..
                } => {
                    if p.creation_flags & 1 != 0 {
                        return Ok(rejected(ArkivFault::Readonly).into());
                    }
                    set.extend_from_slice(changes);
                    remove.extend_from_slice(deleted);
                }
                ArkivOperation::ExtendExpiry { .. } => set.push((
                    name("$expiresAt"),
                    Cell::attribute(Value::U64(new.expect("extension"))),
                )),
                ArkivOperation::TransferOwnership { owner, .. } => {
                    if *owner == [0; 20] || *owner == p.owner {
                        return Ok(rejected(ArkivFault::Owner).into());
                    }
                    set.push((name("$owner"), Cell::attribute(Value::Addr(*owner))))
                }
                _ => unreachable!(),
            }
            WriteOp::PatchRecord {
                namespace,
                key: op.key().expect("mutation key").clone(),
                set,
                remove,
            }
        }
    };
    // The entity as this operation leaves it (empty for a delete).
    let after_cells = match &write {
        WriteOp::CreateRecord { cells, .. } => cells.clone(),
        WriteOp::PatchRecord { set, remove, .. } => patched(&current_cells, set, remove),
        _ => Vec::new(),
    };
    if entity_bytes(&after_cells) > MAX_ENTITY_BYTES {
        return Ok(rejected(ArkivFault::EntityTooLarge).into());
    }
    // Genesis permanent prefill: no time-based rent for a creation. Decided
    // before quoting, so a sponsored row never meets a quote at all.
    let waived = c.allow_permanent_create
        && new == Some(u64::MAX)
        && matches!(op, ArkivOperation::Create { .. });
    let rent = if crate::gas::is_active(branch.pricing()) && !waived {
        let quoted = (|| {
            let before = if current.is_some() {
                crate::gas::storage_units(&current_cells)?.total
            } else {
                0
            };
            let deleted = matches!(write, WriteOp::DeleteRecord { .. });
            let after = if deleted {
                0
            } else {
                crate::gas::storage_units(&after_cells)?.total
            };
            crate::gas::rent(
                before,
                after,
                old,
                if deleted { None } else { new },
                c.block_height,
                c.rent,
            )
        })();
        match quoted {
            Ok(quote) => alloy_primitives::U256::from(quote.wei),
            Err(_) => return Ok(rejected(ArkivFault::GasOverflow).into()),
        }
    } else {
        alloy_primitives::U256::ZERO
    };
    let payer = arkiv_db::OwnerId::new(c.caller);
    if let Some(accounts) = c.payer
        && !rent.is_zero()
        && accounts.read(branch, payer)?.balance < rent
    {
        return Ok(rejected(ArkivFault::InsufficientRentBalance).into());
    }
    let result = match branch.try_execute_host(write, budget)? {
        Ok(r) => r,
        Err(r) => {
            return Ok(OpOutcome::Rejected {
                reason: r.reason,
                receipt: r.receipt,
            }
            .into());
        }
    };
    if let Some(accounts) = c.payer
        && !rent.is_zero()
    {
        accounts.burn_from(branch, payer, rent)?;
    }
    if let Some(n) = next_nonce {
        put_count(branch, nonce_key(&c.caller), n)?;
    }
    let after = if matches!(op, ArkivOperation::Delete { .. }) {
        None
    } else {
        new
    };
    if old != after {
        if let Some(h) = old.filter(|h| *h != u64::MAX) {
            for key in [
                crate::exec::due_key(h, namespace),
                crate::exec::total_key(h),
            ] {
                let n = count(branch, &key)?.checked_sub(1).ok_or_else(corrupt)?;
                put_count(branch, key, n)?;
            }
        }
        if let Some(h) = after.filter(|h| *h != u64::MAX) {
            for key in [
                crate::exec::due_key(h, namespace),
                crate::exec::total_key(h),
            ] {
                let n = count(branch, &key)?.checked_add(1).ok_or_else(corrupt)?;
                put_count(branch, key, n)?;
            }
        }
    }
    Ok(ArkivOutcome {
        outcome: OpOutcome::Applied {
            effect: result.effect,
            receipt: result.receipt,
        },
        rent_wei: rent,
    })
}
/// `cells` after a patch that sets `set` and removes `remove`.
fn patched(
    cells: &[(CellName, Cell)],
    set: &[(CellName, Cell)],
    remove: &[CellName],
) -> Vec<(CellName, Cell)> {
    let mut cells = cells.to_vec();
    cells.retain(|(name, _)| !remove.contains(name) && !set.iter().any(|(n, _)| n == name));
    cells.extend(set.iter().cloned());
    cells
}
/// The rent `op` would charge applied alone at `c.block_height` on `branch`,
/// in wei: an admission quote, not consensus. It reads at most the one
/// entity the operation names (a create reads nothing) and checks neither
/// authority nor capacity; an operation that could not apply quotes zero.
/// Execution prices every operation again ([`execute_operation`]); where
/// both apply to the same state, they agree.
pub fn quote_rent(
    branch: &Branch,
    namespace: NamespaceId,
    c: &ArkivExecutionContext<'_>,
    op: &ArkivOperation,
) -> Result<alloy_primitives::U256, ExecError> {
    let zero = alloy_primitives::U256::ZERO;
    if !crate::gas::is_active(branch.pricing()) || op.validate().is_err() {
        return Ok(zero);
    }
    let (current_cells, old) = match op.key() {
        None => (Vec::new(), None),
        Some(key) => {
            let row = match branch.try_get_record(
                namespace,
                key,
                &Projection::ALL,
                Budget::unlimited(),
            )? {
                Ok(read) => read.value,
                Err(_) => None,
            };
            let Some(row) = row else { return Ok(zero) };
            let Ok(projected) = project(&row) else {
                return Ok(zero);
            };
            if projected.expires_at <= c.block_height {
                return Ok(zero);
            }
            (row.cells, Some(projected.expires_at))
        }
    };
    let new = match op.expiry() {
        Some(exp) => match expiry(exp, c) {
            Ok(target) if old.is_none_or(|v| target >= v) => Some(target),
            _ => return Ok(zero),
        },
        None => old,
    };
    let stamp = (
        name("$updatedAt"),
        Cell::attribute(Value::U64(c.block_height)),
    );
    let after_cells = match op {
        ArkivOperation::Create {
            flags, attributes, ..
        } => create_cells(
            // Every system cell has a fixed size, so the key does not matter.
            [0; 32],
            c.caller,
            c.block_height,
            new.unwrap_or(u64::MAX),
            *flags,
            attributes,
        ),
        ArkivOperation::Patch { set, remove, .. } => {
            let mut changes = vec![stamp];
            changes.extend_from_slice(set);
            patched(&current_cells, &changes, remove)
        }
        ArkivOperation::ExtendExpiry { .. } => patched(
            &current_cells,
            &[
                stamp,
                (
                    name("$expiresAt"),
                    Cell::attribute(Value::U64(new.unwrap_or(u64::MAX))),
                ),
            ],
            &[],
        ),
        ArkivOperation::TransferOwnership { .. } | ArkivOperation::Delete { .. } => {
            return Ok(zero);
        }
    };
    let units = |cells: &[(CellName, Cell)]| crate::gas::storage_units(cells).map(|u| u.total);
    let before = if op.key().is_some() {
        units(&current_cells)
    } else {
        Ok(0)
    };
    let quote = before.and_then(|before| {
        crate::gas::rent(
            before,
            units(&after_cells)?,
            old,
            new,
            c.block_height,
            c.rent,
        )
    });
    Ok(quote.map_or(zero, |q| alloy_primitives::U256::from(q.wei)))
}
/// Atomic batch on a branch. Deterministic failure rolls back all cells, indexes,
/// creation counters, due schedules and rent debits, retaining normal engine
/// charge receipts. Returns the outcomes and the rent the batch charged (zero
/// when it failed).
pub fn execute_batch(
    branch: &mut Branch,
    namespace: NamespaceId,
    c: &ArkivExecutionContext<'_>,
    ops: &[ArkivOperation],
    budget: u64,
) -> Result<(Vec<OpOutcome>, alloy_primitives::U256), ExecError> {
    let checkpoint = branch.checkpoint();
    let mut outcomes = Vec::new();
    let mut remaining = budget;
    let mut rent = alloy_primitives::U256::ZERO;
    for op in ops {
        let result = execute_operation(branch, namespace, c, op, Budget::limit(remaining));
        let executed = match result {
            Ok(o) => o,
            Err(e) => {
                branch.rollback_to(checkpoint).map_err(|_| corrupt())?;
                return Err(e);
            }
        };
        let outcome = executed.outcome;
        remaining = remaining.saturating_sub(outcome.spent());
        rent = rent.checked_add(executed.rent_wei).ok_or_else(corrupt)?;
        let failed = !matches!(outcome, OpOutcome::Applied { .. });
        outcomes.push(outcome);
        if failed {
            branch.rollback_to(checkpoint).map_err(|_| corrupt())?;
            for o in &mut outcomes {
                if let OpOutcome::Applied { effect, receipt } = o {
                    *o = OpOutcome::RolledBack {
                        effect: *effect,
                        receipt: receipt.clone(),
                    }
                }
            }
            outcomes.resize(ops.len(), OpOutcome::NotExecuted);
            return Ok((outcomes, alloy_primitives::U256::ZERO));
        }
    }
    Ok((outcomes, rent))
}

#[cfg(test)]
mod tests;
