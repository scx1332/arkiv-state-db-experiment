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
    PermanentStorageUnsupported = 14,
    UnsupportedGasOperation = 15,
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
            tag => {
                return Err(DecodeError::UnknownTag {
                    kind: "ArkivFault",
                    tag,
                });
            }
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArkivOperation {
    Create {
        salt: u128,
        flags: u8,
        expiry: Expiry,
        attributes: Vec<(CellName, Value)>,
    },
    Patch {
        key: RecordKey,
        set: Vec<(CellName, Value)>,
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
        if attrs.len() > 32 {
            return Err(ArkivFault::Value);
        }
        if let Self::Patch { remove, .. } = self
            && attrs.len() + remove.len() > 32
        {
            return Err(ArkivFault::Value);
        }
        for (name, value) in attrs {
            if !names.insert(name) {
                return Err(ArkivFault::Duplicate);
            }
            user_cell(name, value)?;
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
pub struct ArkivExecutionContext {
    pub caller: [u8; 20],
    pub block_height: u64,
    pub chain_id: u64,
    pub max_lifetime: u64,
    pub expiry_per_block: u32,
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
pub(crate) fn expiry(exp: &Expiry, c: &ArkivExecutionContext) -> Result<u64, HostFault> {
    let floor = c
        .block_height
        .checked_add(exp.min_lifetime)
        .ok_or(HostFault::ExpiryInvalid(crate::ExpiryFault::Overflow))?;
    let target = exp.expires_at.max(floor);
    if target <= c.block_height {
        return Err(HostFault::ExpiryInvalid(crate::ExpiryFault::DeadOnArrival));
    }
    if target != u64::MAX && target - c.block_height > c.max_lifetime {
        return Err(HostFault::ExpiryInvalid(crate::ExpiryFault::TooFar));
    }
    Ok(target)
}
/// Canonical creation cells shared by live execution and bulk genesis.
/// Callers validate the operation and expiry before invoking this helper.
pub(crate) fn create_cells(
    key: [u8; 32],
    creator: [u8; 20],
    height: u64,
    expires: u64,
    flags: u8,
    attributes: &[(CellName, Value)],
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
    for (n, v) in attributes {
        cells.push((n.clone(), user_cell(n, v).expect("validated")))
    }
    if !attributes.iter().any(|(n, _)| n.as_str() == "$payload") {
        cells.push((name("$payload"), Cell::field(Value::bytes(&[]))))
    }
    if !attributes.iter().any(|(n, _)| n.as_str() == "$contentType") {
        cells.push((name("$contentType"), Cell::attribute(Value::str(""))))
    }
    cells
}
/// Execute one operation; the enclosing transaction checkpoint provides atomic batches.
/// Engine/storage errors remain fatal and may never be downgraded to user rejections.
pub fn execute_operation(
    branch: &mut Branch,
    namespace: NamespaceId,
    c: &ArkivExecutionContext,
    op: &ArkivOperation,
    budget: Budget,
) -> Result<OpOutcome, ExecError> {
    // GLM disables generic record mutations in every namespace, so all of
    // them can safely use Arkiv ownership and rent. Chains without GLM
    // pricing allow generic writes outside namespace one, so entities stay
    // confined to it there.
    if namespace != entity_namespace() && !crate::gas::is_active(branch.pricing()) {
        return Ok(rejected(ArkivFault::Namespace));
    }
    if let Err(f) = op.validate() {
        return Ok(rejected(f));
    }
    let mut current_cells = Vec::new();
    let current = if let Some(key) = op.key() {
        let row =
            match branch.try_get_record(namespace, key, &Projection::ALL, Budget::unlimited())? {
                Ok(read) => read.value,
                Err(rejected) if rejected.reason == arkiv_db::Rejection::UnknownNamespace => {
                    return Ok(self::rejected(ArkivFault::NotFound));
                }
                Err(_) => return Err(corrupt()),
            };
        let Some(row) = row else {
            return Ok(rejected(ArkivFault::NotFound));
        };
        let projected = project(&row).map_err(|_| corrupt())?;
        if projected.expires_at <= c.block_height {
            return Ok(rejected(ArkivFault::Expired));
        }
        if projected.owner != c.caller
            && !(matches!(op, ArkivOperation::ExtendExpiry { .. })
                && projected.creation_flags & 2 != 0)
        {
            return Ok(rejected(ArkivFault::NotOwner));
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
            Err(fault) => return Ok(OpOutcome::HostRejected { fault }),
        };
        if old.is_some_and(|v| target < v) {
            return Ok(OpOutcome::HostRejected {
                fault: HostFault::ExpiryShortened,
            });
        }
        if target != u64::MAX
            && old != Some(target)
            && count(branch, &crate::exec::total_key(target))? >= u64::from(c.expiry_per_block)
        {
            return Ok(OpOutcome::HostRejected {
                fault: HostFault::ExpiryCapacityFull,
            });
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
                return Ok(rejected(ArkivFault::NonceExhausted));
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
                        return Ok(rejected(ArkivFault::Readonly));
                    }
                    for (n, v) in changes {
                        set.push((n.clone(), user_cell(n, v).expect("validated")))
                    }
                    for n in deleted {
                        match n.as_str() {
                            "$payload" => set.push((n.clone(), Cell::field(Value::bytes(&[])))),
                            "$contentType" => {
                                set.push((n.clone(), Cell::attribute(Value::str(""))))
                            }
                            _ => remove.push(n.clone()),
                        }
                    }
                }
                ArkivOperation::ExtendExpiry { .. } => set.push((
                    name("$expiresAt"),
                    Cell::attribute(Value::U64(new.expect("extension"))),
                )),
                ArkivOperation::TransferOwnership { owner, .. } => {
                    if *owner == [0; 20] || *owner == p.owner {
                        return Ok(rejected(ArkivFault::Owner));
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
    let rent = if crate::gas::is_active(branch.pricing()) {
        let quoted = (|| {
            let before = if current.is_some() {
                crate::gas::storage_units(&current_cells)?.total
            } else {
                0
            };
            let after_cells = match &write {
                WriteOp::CreateRecord { cells, .. } => cells.clone(),
                WriteOp::PatchRecord { set, remove, .. } => {
                    let mut cells = current_cells.clone();
                    cells.retain(|(name, _)| {
                        !remove.contains(name) && !set.iter().any(|(n, _)| n == name)
                    });
                    cells.extend(set.clone());
                    cells
                }
                _ => Vec::new(),
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
            )
        })();
        match quoted {
            Ok(quote) => quote.gas,
            Err(crate::gas::GasError::PermanentUnsupported) => {
                return Ok(rejected(ArkivFault::PermanentStorageUnsupported));
            }
            Err(_) => return Ok(rejected(ArkivFault::GasOverflow)),
        }
    } else {
        0
    };
    // Reserve rent before engine execution, then charge it only on success.
    let available = budget
        .limit_units()
        .map_or(branch.remaining().units(), |v| v.units());
    let Some(engine_budget) = available.checked_sub(rent) else {
        let failure = branch
            .try_charge_host(rent, budget)?
            .expect_err("rent exceeds available gas");
        return Ok(OpOutcome::Rejected {
            reason: failure.reason,
            receipt: failure.receipt,
        });
    };
    let mut result = match branch.try_execute_host(
        write,
        if rent == 0 {
            budget
        } else {
            Budget::limit(engine_budget)
        },
    )? {
        Ok(r) => r,
        Err(r) => {
            return Ok(OpOutcome::Rejected {
                reason: r.reason,
                receipt: r.receipt,
            });
        }
    };
    if rent != 0 {
        let charged = branch
            .try_charge_host(rent, Budget::limit(rent))?
            .map_err(|_| corrupt())?;
        result.receipt.spent = result
            .receipt
            .spent
            .checked_add(charged.spent)
            .ok_or_else(corrupt)?;
        // The engine-only ledger cannot represent protocol rent.
        result.receipt.ledger = None;
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
    Ok(OpOutcome::Applied {
        effect: result.effect,
        receipt: result.receipt,
    })
}
/// Atomic batch on a branch. Deterministic failure rolls back all cells, indexes,
/// creation counters and due schedules, retaining normal engine charge receipts.
pub fn execute_batch(
    branch: &mut Branch,
    namespace: NamespaceId,
    c: &ArkivExecutionContext,
    ops: &[ArkivOperation],
    budget: u64,
) -> Result<Vec<OpOutcome>, ExecError> {
    let checkpoint = branch.checkpoint();
    let mut outcomes = Vec::new();
    let mut remaining = budget;
    for op in ops {
        let result = execute_operation(branch, namespace, c, op, Budget::limit(remaining));
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                branch.rollback_to(checkpoint).map_err(|_| corrupt())?;
                return Err(e);
            }
        };
        remaining = remaining.saturating_sub(outcome.spent());
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
            break;
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests;
