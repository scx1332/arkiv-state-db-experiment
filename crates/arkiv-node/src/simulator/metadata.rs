//! One execution's bounded, terminal after-images. No payloads in these DTOs.
use super::wire::*;
use crate::{
    AdminOutcome, BlockInputs, BlockOutcomes, OpOutcome, TxOp, TxStatus, exec::ExpiredRecord,
};
use arkiv_db::{
    Branch, Budget, CellKind, Effect, NamespaceId, Projection, RecordId, RecordKey, StorageError,
    StorageErrorKind, Value, digest, encode,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct Metadata {
    pub transactions: Vec<Transaction>,
    pub operations: Vec<Operation>,
    pub changes: Vec<Change>,
    pub spent: u64,
}
fn corrupt() -> StorageError {
    StorageError::new(
        StorageErrorKind::Corrupt,
        "metadata terminal state inconsistency",
    )
}
pub fn value_type(v: &Value) -> &'static str {
    v.tag().name()
}
pub fn value_digest(v: &Value) -> String {
    hex(digest("arkiv/simulator-value/v1", &encode(v)).as_slice())
}
pub fn record_change(namespace: NamespaceId, row: arkiv_db::Row) -> super::Result<Change> {
    let mut attributes = Vec::new();
    let mut fields = Vec::new();
    let mut expiry = u64::MAX;
    for (name, cell) in row.cells {
        if cell.kind == CellKind::Attribute {
            if name.as_str() == crate::EXPIRES_AT
                && let Value::U64(n) = cell.value
            {
                expiry = n;
            }
            if matches!(cell.value, Value::Bytes(_)) {
                return Err(super::Error("StorageCorrupt"));
            }
            let value = crate::value_json::to_json(&cell.value);
            attributes.push(Attribute {
                name: name.as_str().into(),
                value_type: value_type(&cell.value).into(),
                value,
            });
        } else {
            fields.push(Field {
                name: name.as_str().into(),
                value_type: value_type(&cell.value).into(),
                byte_length: cell.value.encoded_len().to_string(),
                digest: value_digest(&cell.value),
                reference: None,
            });
        }
    }
    Ok(Change::RecordUpsert {
        namespace_id: namespace.ordinal().to_string(),
        record_id: row.id.ordinal().to_string(),
        record_key: hex(row.key.ok_or(super::Error("StorageCorrupt"))?.as_bytes()),
        expires_at_height: expiry.to_string(),
        attributes,
        fields,
    })
}

pub fn capture(
    branch: &Branch,
    inputs: &BlockInputs,
    outcomes: &BlockOutcomes,
    expired: &[ExpiredRecord],
) -> std::result::Result<Metadata, StorageError> {
    let mut out = Metadata {
        spent: outcomes.spent,
        ..Default::default()
    };
    let mut namespaces = BTreeSet::new();
    let mut records: BTreeMap<(NamespaceId, RecordId), RecordKey> = BTreeMap::new();
    let mut raw: BTreeSet<(NamespaceId, Vec<u8>)> = BTreeSet::new();
    for (i, result) in outcomes.admin.iter().enumerate() {
        out.operations.push(Operation {
            phase: "admin".into(),
            group_position: i as u32,
            operation_position: 0,
            kind: "setPricing".into(),
            namespace_id: None,
            record_id: None,
            record_key: None,
            outcome: if matches!(result, AdminOutcome::Applied) {
                "applied"
            } else {
                "rejected"
            }
            .into(),
            receipt: None,
            reason: match result {
                AdminOutcome::Applied => None,
                AdminOutcome::Rejected(_) => Some("AdminRejected".into()),
            },
        });
    }
    if expired.len() != outcomes.expiry.len() {
        return Err(corrupt());
    }
    for (i, (entry, result)) in expired.iter().zip(&outcomes.expiry).enumerate() {
        if (entry.namespace, entry.id) != (result.namespace, result.record_id) {
            return Err(corrupt());
        }
        records.insert((entry.namespace, entry.id), entry.key.clone());
        out.operations.push(Operation {
            phase: "expiry".into(),
            group_position: 0,
            operation_position: i as u32,
            kind: "expireRecord".into(),
            namespace_id: Some(entry.namespace.ordinal().to_string()),
            record_id: Some(entry.id.ordinal().to_string()),
            record_key: Some(hex(entry.key.as_bytes())),
            outcome: "applied".into(),
            receipt: Some((&result.receipt).into()),
            reason: None,
        });
    }
    // A committed nonce is advanced once per successful create. Reconstruct
    // starting nonces from terminal state so even create-then-delete has a
    // deterministic touched key without storing a payload side channel.
    let mut create_nonces: BTreeMap<[u8; 20], u64> = BTreeMap::new();
    for (tx, result) in inputs.txs.iter().zip(&outcomes.txs) {
        if result.status == TxStatus::Committed {
            let count = tx
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        TxOp::Arkiv {
                            operation: crate::arkiv::ArkivOperation::Create { .. },
                            ..
                        }
                    )
                })
                .count() as u64;
            if count != 0 {
                *create_nonces.entry(*tx.actor.as_bytes()).or_default() += count;
            }
        }
    }
    for (actor, count) in &mut create_nonces {
        let bytes = branch
            .try_host_get(&crate::arkiv::nonce_key(actor))?
            .ok_or_else(corrupt)?;
        let terminal = u64::from_be_bytes(bytes.try_into().map_err(|_| corrupt())?);
        *count = terminal.checked_sub(*count).ok_or_else(corrupt)?;
    }
    for (txpos, (tx, result)) in inputs.txs.iter().zip(&outcomes.txs).enumerate() {
        out.transactions.push(Transaction {
            position: txpos as u32,
            digest: hex(tx.digest().as_slice()),
            actor: hex(tx.actor.as_bytes()),
            request_id: hex(&tx.request_id),
            budget_units: tx.tx_budget.to_string(),
            status: if result.status == TxStatus::Committed {
                "committed"
            } else {
                "failed"
            }
            .into(),
            spent_units: result.spent.to_string(),
            rent_wei: result.rent_wei.to_string(),
        });
        for (oppos, (op, result)) in tx.ops.iter().zip(&result.ops).enumerate() {
            let (status, receipt, effect, reason) = match result {
                OpOutcome::Applied { effect, receipt } => {
                    ("applied", Some(receipt), Some(effect), None)
                }
                OpOutcome::RolledBack { effect, receipt } => {
                    ("rolledBack", Some(receipt), Some(effect), None)
                }
                OpOutcome::AccountRejected { receipt, .. } => {
                    ("rejected", Some(receipt), None, Some("AccountRejected"))
                }
                OpOutcome::Rejected { receipt, .. } => {
                    ("rejected", Some(receipt), None, Some("EngineRejected"))
                }
                OpOutcome::AdmissionRejected { .. } => {
                    ("admissionRejected", None, None, Some("AdmissionRejected"))
                }
                OpOutcome::HostRejected { .. } => {
                    ("hostRejected", None, None, Some("HostRejected"))
                }
                OpOutcome::NotExecuted => ("notExecuted", None, None, None),
            };
            let mut ns = op.namespace();
            let mut id = None;
            let mut key = None;
            if let Some(effect) = effect {
                match effect {
                    Effect::NamespaceCreated(n) => ns = Some(*n),
                    Effect::RecordCreated { id: r }
                    | Effect::RecordPatched { id: r }
                    | Effect::RecordDeleted { id: r } => id = Some(*r),
                    _ => {}
                }
            }
            let mut arkiv_key = None;
            if let TxOp::Arkiv { operation, .. } = op {
                use crate::arkiv::ArkivOperation as A;
                arkiv_key = match operation {
                    A::Create { salt, .. } if status == "applied" => {
                        let nonce = create_nonces
                            .get_mut(tx.actor.as_bytes())
                            .ok_or_else(corrupt)?;
                        let key = crate::arkiv::derive_entity_key(
                            tx.chain_id,
                            tx.actor.as_bytes(),
                            *nonce,
                            *salt,
                        );
                        *nonce = nonce.checked_add(1).ok_or_else(corrupt)?;
                        Some(RecordKey::new(&key).ok_or_else(corrupt)?)
                    }
                    A::Create { .. } => None,
                    A::Patch { key, .. }
                    | A::ExtendExpiry { key, .. }
                    | A::TransferOwnership { key, .. }
                    | A::Delete { key } => Some(key.clone()),
                };
                key = arkiv_key.as_ref().map(|k| hex(k.as_bytes()));
            }
            let kind = match op {
                TxOp::Transfer { .. } => "transfer",
                TxOp::Arkiv { operation, .. } => match operation {
                    crate::arkiv::ArkivOperation::Create { .. } => "createRecord",
                    crate::arkiv::ArkivOperation::Delete { .. } => "deleteRecord",
                    _ => "patchRecord",
                },
                TxOp::CreateNamespace { .. } => "createNamespace",
                TxOp::TransferNamespace { .. } => "transferNamespace",
                TxOp::Put { key: k, .. } => {
                    key = Some(hex(k));
                    "put"
                }
                TxOp::Delete { key: k, .. } => {
                    key = Some(hex(k));
                    "delete"
                }
                TxOp::CreateRecord { key: k, .. } => {
                    key = Some(hex(k.as_bytes()));
                    "createRecord"
                }
                TxOp::PatchRecord { key: k, .. } => {
                    key = Some(hex(k.as_bytes()));
                    "patchRecord"
                }
                TxOp::DeleteRecord { key: k, .. } => {
                    key = Some(hex(k.as_bytes()));
                    "deleteRecord"
                }
            };
            if status == "applied"
                && let Some(ns) = ns
            {
                match op {
                    TxOp::Arkiv { .. } => {
                        records.insert(
                            (ns, id.ok_or_else(corrupt)?),
                            arkiv_key.ok_or_else(corrupt)?,
                        );
                    }
                    TxOp::Transfer { .. } => {}
                    TxOp::CreateNamespace { .. } | TxOp::TransferNamespace { .. } => {
                        namespaces.insert(ns);
                    }
                    TxOp::CreateRecord { key, .. }
                    | TxOp::PatchRecord { key, .. }
                    | TxOp::DeleteRecord { key, .. } => {
                        records.insert((ns, id.ok_or_else(corrupt)?), key.clone());
                    }
                    TxOp::Put { key, .. } | TxOp::Delete { key, .. } => {
                        raw.insert((ns, key.clone()));
                    }
                }
            }
            out.operations.push(Operation {
                phase: "user".into(),
                group_position: txpos as u32,
                operation_position: oppos as u32,
                kind: kind.into(),
                namespace_id: ns.map(|n| n.ordinal().to_string()),
                record_id: id.map(|n| n.ordinal().to_string()),
                record_key: key,
                outcome: status.into(),
                receipt: receipt.map(Into::into),
                reason: reason.map(Into::into),
            });
        }
    }
    for ns in namespaces {
        let d = branch
            .try_descriptor(ns, Budget::unlimited())?
            .map_err(|_| corrupt())?
            .value;
        out.changes.push(Change::NamespaceUpsert {
            namespace_id: ns.ordinal().to_string(),
            name: d.name.as_str().into(),
            owner: hex(d.owner.as_bytes()),
            revision: d.revision.0.to_string(),
        });
    }
    // At most one terminal lookup per touched key, even for multiple incarnations.
    let mut terminal = BTreeMap::new();
    for (&(ns, _), key) in &records {
        if let std::collections::btree_map::Entry::Vacant(e) = terminal.entry((ns, key.clone())) {
            e.insert(
                branch
                    .try_get_record(ns, key, &Projection::ALL, Budget::unlimited())?
                    .map_err(|_| corrupt())?
                    .value,
            );
        }
    }
    for ((ns, id), key) in records {
        match terminal.get(&(ns, key.clone())).and_then(Option::as_ref) {
            Some(row) if row.id == id => out
                .changes
                .push(record_change(ns, row.clone()).map_err(|_| corrupt())?),
            _ => out.changes.push(Change::RecordDelete {
                namespace_id: ns.ordinal().to_string(),
                record_id: id.ordinal().to_string(),
                record_key: hex(key.as_bytes()),
            }),
        }
    }
    for (ns, key) in raw {
        let value = branch
            .try_get(ns, &key, Budget::unlimited())?
            .map_err(|_| corrupt())?
            .value;
        out.changes.push(match value {
            Some(v) => Change::RawUpsert {
                namespace_id: ns.ordinal().to_string(),
                key: hex(&key),
                byte_length: v.len().to_string(),
                digest: value_digest(&Value::bytes(&v)),
            },
            None => Change::RawDelete {
                namespace_id: ns.ordinal().to_string(),
                key: hex(&key),
            },
        });
    }
    Ok(out)
}
