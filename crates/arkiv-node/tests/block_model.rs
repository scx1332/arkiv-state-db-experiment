//! Seeded block histories against an independent oracle.
//!
//! For each seed a generator builds 60 blocks of inputs — multi-operation
//! transactions over raw entries, typed records and namespaces, operations
//! that fail part way through a transaction, exact and one-short
//! transaction budgets, same-block ownership transfers, `SetPricing` admin
//! inputs (applied, stale, breaking the reservation, overflowing), and
//! expiries at the next height, at capacity, permanent, dead on arrival,
//! too far and shortened — and produces them on a `MemoryChain`.
//!
//! The oracle never calls the engine's pricing or execution. It has:
//!
//! - its own weight table per schedule (`Weights`, named fields; the
//!   engine's `PriceSchedule` is built from it, never read back), and the
//!   model-3 charge table transcribed from the brief as lists of atomic
//!   charge groups with the semantic check points between them;
//! - a `BTreeMap` model of every namespace (descriptor, raw entries,
//!   records with their cells) with its own id allocators, and term newness
//!   derived from the live records;
//! - its own due schedule (`(height, namespace) → count`, `height →
//!   total`), updated by the rules of the brief, and its own expiry
//!   resolution, extend-only and capacity checks;
//! - the reservation and sweep-bound arithmetic of the brief for admin
//!   inputs, and the pricing in force.
//!
//! Before each block is produced the oracle predicts its complete
//! `BlockOutcomes` (every admin outcome, expiry deletion with its receipt,
//! transaction status, operation outcome and receipt, and the block's
//! spend); the produced block must equal it. After each block the
//! snapshot is compared with the model: descriptors, raw maps, every record
//! (and the absence of every other key), a full-scan evaluation of a query
//! catalogue (results through cursor walks, and `count`), the host-state
//! due schedule entry by entry, and the pricing in force. At the end the
//! whole chain is replayed on a fresh engine and every historical height
//! of both chains is compared with the model's state at that height.
//!
//! Coverage counters assert that every seed set reached every outcome kind
//! the brief lists.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use arkiv_db::{
    Budget, Cell, CellKind, CellName, Cost, Effect, Limits, Literal, NamespaceName, Outcome,
    OwnerId, PriceSchedule, Projection, Query, Receipt, ReceiptKind, RecordId, RecordKey,
    Rejection, Revision, Snapshot, Term, TypeTag, Value,
};
use arkiv_node::{
    AdminInput, AdminOutcome, AdminRejection, Block, BlockInputs, BlockOutcomes, ChainParams,
    ChainSpec, Expiry, ExpiryDeletion, ExpiryFault, Genesis, HostFault, MemoryChain, OpOutcome,
    TxEnvelope, TxOp, TxOutcome, TxStatus, replay,
};
use support::{
    ADMIN, ALICE, BOB, CAROL, CHAIN, due_key, genesis_with, host_entries, ns, request, total_key,
};

const SEEDS: [u64; 4] = [
    0x0000_0001,
    0x9e37_79b9_7f4a_7c15,
    0xdead_beef,
    0x0123_4567_89ab,
];
const BLOCKS: u64 = 60;
const ACTORS: [OwnerId; 3] = [ALICE, BOB, CAROL];

/// The Arkiv entity namespace: host-controlled, so static validation
/// refuses every generic operation on it. Block 1 creates it (owned by
/// `ADMIN`, never one of [`ACTORS`]) and the generator never addresses it.
const ENTITY_NAMESPACE: u64 = 1;

/// Five cells, 16-byte values, pages of two (so sweeps and walks read
/// several pages).
const LIMITS: Limits = Limits {
    namespace_cap: 1 << 32,
    max_cells: 5,
    max_str_len: 16,
    max_bytes_len: 16,
    max_page: 2,
    max_groups: 8,
    max_literals: 8,
    ..Limits::DEFAULT
};

/// The block budget is `user_budget + expiry_per_block ×` the dearest
/// deletion among the valid schedules the generator proposes: under
/// `reads_x100`, `DirectoryLookup 100 + PointRead 300 + RecordDelete 20 +
/// 5 × CellRemove 4 + 3 × RootUpdate 2 + 10 × (IndexRemove 6 + 2 ×
/// RootUpdate 2)` = 546, so 50,000 + 3 × 546 = 51,638.
const PARAMS: ChainParams = ChainParams {
    block_budget: 51_638,
    user_budget: 50_000,
    expiry_per_block: 3,
    max_admin_per_block: 2,
    max_txs_per_block: 6,
    max_ops_per_tx: 5,
    max_ops_per_block: 24,
    max_tx_bytes: 2_048,
    max_block_input_bytes: 16_384,
    max_lifetime: 12,
    max_rent_lifetime: arkiv_node::gas::DEFAULT_RENT_CAP_BLOCKS,
};

/// Budget of transactions that should not run out.
const AMPLE: u64 = 6_000;

// ---------------------------------------------------------------------------
// Weights: the oracle's own price table.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Weights {
    id: u32,
    directory_lookup: u64,
    namespace_create: u64,
    namespace_transfer: u64,
    put_record: u64,
    delete_record: u64,
    write_key_byte: u64,
    write_value_byte: u64,
    point_read: u64,
    scan_seek: u64,
    scan_entry: u64,
    read_byte: u64,
    record_create: u64,
    record_patch: u64,
    record_delete: u64,
    cell_write: u64,
    cell_byte: u64,
    cell_remove: u64,
    index_insert: u64,
    index_term_create: u64,
    index_remove: u64,
    query_group: u64,
    term_seek: u64,
    term_visit: u64,
    posting_entry: u64,
    row_visit: u64,
    record_probe: u64,
    result_row: u64,
    root_update: u64,
    row_byte: u64,
    proof_node: u64,
    proof_byte: u64,
}

/// The fixture weights, transcribed from the charge table.
const FIXTURE: Weights = Weights {
    id: 1,
    directory_lookup: 1,
    namespace_create: 50,
    namespace_transfer: 20,
    put_record: 10,
    delete_record: 5,
    write_key_byte: 2,
    write_value_byte: 1,
    point_read: 3,
    scan_seek: 4,
    scan_entry: 2,
    read_byte: 1,
    record_create: 20,
    record_patch: 10,
    record_delete: 20,
    cell_write: 4,
    cell_byte: 1,
    cell_remove: 4,
    index_insert: 6,
    index_term_create: 30,
    index_remove: 6,
    query_group: 5,
    term_seek: 4,
    term_visit: 3,
    posting_entry: 1,
    row_visit: 1,
    record_probe: 2,
    result_row: 3,
    root_update: 2,
    row_byte: 1,
    proof_node: 2,
    proof_byte: 1,
};

impl Weights {
    /// The engine input: one weight per class in the documented order
    /// (`DirectoryLookup`, `NamespaceCreate`, …, `ProofByte`).
    fn schedule(&self) -> PriceSchedule {
        PriceSchedule::new(
            self.id,
            [
                self.directory_lookup,
                self.namespace_create,
                self.namespace_transfer,
                self.put_record,
                self.delete_record,
                self.write_key_byte,
                self.write_value_byte,
                self.point_read,
                self.scan_seek,
                self.scan_entry,
                self.read_byte,
                self.record_create,
                self.record_patch,
                self.record_delete,
                self.cell_write,
                self.cell_byte,
                self.cell_remove,
                self.index_insert,
                self.index_term_create,
                self.index_remove,
                self.query_group,
                self.term_seek,
                self.term_visit,
                self.posting_entry,
                self.row_visit,
                self.record_probe,
                self.result_row,
                self.root_update,
                self.row_byte,
                self.proof_node,
                self.proof_byte,
            ],
        )
    }

    fn uniform(id: u32, weight: u64) -> Self {
        Self {
            id,
            directory_lookup: weight,
            namespace_create: weight,
            namespace_transfer: weight,
            put_record: weight,
            delete_record: weight,
            write_key_byte: weight,
            write_value_byte: weight,
            point_read: weight,
            scan_seek: weight,
            scan_entry: weight,
            read_byte: weight,
            record_create: weight,
            record_patch: weight,
            record_delete: weight,
            cell_write: weight,
            cell_byte: weight,
            cell_remove: weight,
            index_insert: weight,
            index_term_create: weight,
            index_remove: weight,
            query_group: weight,
            term_seek: weight,
            term_visit: weight,
            posting_entry: weight,
            row_visit: weight,
            record_probe: weight,
            result_row: weight,
            root_update: weight,
            row_byte: weight,
            proof_node: weight,
            proof_byte: weight,
        }
    }

    /// Every read class ×100.
    fn reads_x100(id: u32) -> Self {
        Self {
            id,
            directory_lookup: 100,
            point_read: 300,
            scan_seek: 400,
            scan_entry: 200,
            read_byte: 100,
            query_group: 500,
            term_seek: 400,
            term_visit: 300,
            posting_entry: 100,
            row_visit: 100,
            record_probe: 200,
            result_row: 300,
            ..FIXTURE
        }
    }

    /// Cheaper term creation, dearer bytes and roots.
    fn reshaped(id: u32) -> Self {
        Self {
            id,
            index_term_create: 7,
            cell_byte: 2,
            root_update: 3,
            row_byte: 2,
            ..FIXTURE
        }
    }

    /// Deletions too dear for the block budget's reservation.
    fn too_dear(id: u32) -> Self {
        Self {
            id,
            record_delete: 10_000,
            ..FIXTURE
        }
    }

    /// Weights whose reservation arithmetic overflows.
    fn overflowing(id: u32) -> Self {
        Self {
            id,
            cell_remove: u64::MAX / 2,
            ..FIXTURE
        }
    }

    fn insert(&self, new: bool) -> u64 {
        self.index_insert + u64::from(new) * self.index_term_create + 2 * self.root_update
    }

    fn remove(&self) -> u64 {
        self.index_remove + 2 * self.root_update
    }

    /// The brief's reservation: `block_budget ≥ user_budget +
    /// expiry_per_block × (DirectoryLookup + PointRead + RecordDelete +
    /// c·CellRemove + 3·RootUpdate + 2a·(IndexRemove + 2·RootUpdate))` with
    /// `c = a = max_cells`, and a representable sweep read bound
    /// (`max(DirectoryLookup + TermSeek + TermVisit + 8·ReadByte,
    /// DirectoryLookup + QueryGroup + TermSeek + (max_page + 1)·PostingEntry
    /// + max_page·(ResultRow + 32·ReadByte))`), all with checked arithmetic.
    fn reservation(&self) -> Result<(), AdminRejection> {
        let c = LIMITS.max_cells as u64;
        let page = LIMITS.max_page as u64;
        let overflow = AdminRejection::ReservationOverflow;
        let terms = self
            .directory_lookup
            .checked_add(self.term_seek)
            .and_then(|sum| sum.checked_add(self.term_visit))
            .and_then(|sum| sum.checked_add(self.read_byte.checked_mul(8)?));
        let query = self
            .read_byte
            .checked_mul(32)
            .and_then(|bytes| bytes.checked_add(self.result_row))
            .and_then(|row| row.checked_mul(page))
            .and_then(|rows| {
                rows.checked_add(self.posting_entry.checked_mul(page + 1)?)?
                    .checked_add(self.directory_lookup)?
                    .checked_add(self.query_group)?
                    .checked_add(self.term_seek)
            });
        terms.zip(query).ok_or(overflow)?;
        let removal = self
            .root_update
            .checked_mul(2)
            .and_then(|roots| roots.checked_add(self.index_remove))
            .and_then(|each| each.checked_mul(2 * c));
        let delete = self
            .cell_remove
            .checked_mul(c)
            .and_then(|cells| cells.checked_add(self.directory_lookup))
            .and_then(|sum| sum.checked_add(self.point_read))
            .and_then(|sum| sum.checked_add(self.record_delete))
            .and_then(|sum| sum.checked_add(self.root_update.checked_mul(3)?))
            .and_then(|sum| sum.checked_add(removal?));
        let required = delete
            .and_then(|each| each.checked_mul(u64::from(PARAMS.expiry_per_block)))
            .and_then(|reserved| reserved.checked_add(PARAMS.user_budget))
            .ok_or(overflow)?;
        if PARAMS.block_budget < required {
            return Err(AdminRejection::ReservationViolated);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The record model.

type Cells = BTreeMap<CellName, Cell>;

#[derive(Clone, Debug)]
struct Rec {
    id: u64,
    cells: Cells,
}

#[derive(Clone, Debug)]
struct NsModel {
    name: NamespaceName,
    owner: OwnerId,
    revision: u64,
    raw: BTreeMap<Vec<u8>, Vec<u8>>,
    records: BTreeMap<Vec<u8>, Rec>,
    next_record: u64,
}

/// A record's lifetime: finite height or permanent (ordered after every
/// height).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Life {
    Until(u64),
    Forever,
}

fn expires_at() -> CellName {
    CellName::new("$expiresAt").unwrap()
}

fn count(length: usize) -> u64 {
    u64::try_from(length).unwrap()
}

/// Encoded value length: 1 for Bool, 8 for the integers, the byte length
/// for Str and Bytes.
fn encoded_len(value: &Value) -> u64 {
    match value {
        Value::Bool(_) => 1,
        Value::I64(_) | Value::U64(_) => 8,
        Value::Str(text) => count(text.len()),
        Value::Bytes(bytes) => count(bytes.len()),
        _ => panic!("the generic model generates only bool, i64, u64, str and bytes"),
    }
}

fn cell_bytes(name: &CellName, cell: &Cell) -> u64 {
    count(name.len()) + encoded_len(&cell.value)
}

/// An index term of one name.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TermId {
    Value(Value),
    Existence(TypeTag),
}

fn terms_of(cell: &Cell) -> Vec<TermId> {
    if cell.kind == CellKind::Attribute {
        vec![
            TermId::Value(cell.value.clone()),
            TermId::Existence(cell.value.tag()),
        ]
    } else {
        Vec::new()
    }
}

impl NsModel {
    /// Whether a live record other than `except` holds `term` of `name`.
    fn holds(&self, name: &CellName, term: &TermId, except: Option<u64>) -> bool {
        self.records.values().any(|rec| {
            Some(rec.id) != except
                && rec
                    .cells
                    .get(name)
                    .is_some_and(|cell| terms_of(cell).contains(term))
        })
    }

    fn lifetime(&self, key: &[u8]) -> Option<Life> {
        let rec = self.records.get(key)?;
        Some(match rec.cells.get(&expires_at()) {
            Some(Cell {
                value: Value::U64(height),
                ..
            }) if *height != u64::MAX => Life::Until(*height),
            _ => Life::Forever,
        })
    }
}

/// Walks the charge groups. A group that would push the running total past
/// `limit` stops the operation with OutOfBudget; a semantic rejection fires
/// right after the group it follows.
fn verdict(charges: &[u64], semantic: Option<(usize, Rejection)>, limit: u64) -> (Outcome, u64) {
    let mut spent = 0u64;
    for (index, &charge) in charges.iter().enumerate() {
        if spent + charge > limit {
            return (Outcome::Rejected(Rejection::OutOfBudget), spent);
        }
        spent += charge;
        if let Some((at, reason)) = semantic
            && at == index
        {
            return (Outcome::Rejected(reason), spent);
        }
    }
    (Outcome::Applied, spent)
}

/// Units an operation outcome the oracle predicted charges its
/// transaction: its receipt's spend, or nothing without a receipt.
fn charged(outcome: &OpOutcome) -> u64 {
    match outcome {
        OpOutcome::AccountRejected { receipt, .. }
        | OpOutcome::Applied { receipt, .. }
        | OpOutcome::RolledBack { receipt, .. }
        | OpOutcome::Rejected { receipt, .. } => receipt.spent.units(),
        OpOutcome::AdmissionRejected { .. }
        | OpOutcome::HostRejected { .. }
        | OpOutcome::NotExecuted => 0,
    }
}

#[derive(Clone, Debug)]
struct Model {
    namespaces: BTreeMap<u64, NsModel>,
    next_namespace: u64,
    /// The schedule in force for the current block.
    in_force: Weights,
    /// The schedule committed so far in the current block (in force next).
    pending: Weights,
    due: BTreeMap<(u64, u64), u64>,
    totals: BTreeMap<u64, u64>,
}

impl Model {
    fn new() -> Self {
        Self {
            namespaces: BTreeMap::new(),
            next_namespace: 1,
            in_force: FIXTURE,
            pending: FIXTURE,
            due: BTreeMap::new(),
            totals: BTreeMap::new(),
        }
    }

    fn receipt(&self, spent: u64, outcome: Outcome) -> Receipt {
        Receipt {
            kind: ReceiptKind::Write,
            model_version: 3,
            schedule_id: self.in_force.id,
            spent: Cost::new(spent),
            outcome,
            ledger: None,
        }
    }

    /// Adds `delta` (±1) to the due count of `namespace` at `height` and to
    /// the height's total; a count reaching zero is removed.
    fn schedule_one(&mut self, height: u64, namespace: u64, delta: i64) {
        let due = self.due.entry((height, namespace)).or_insert(0);
        *due = due
            .checked_add_signed(delta)
            .expect("model counts stay valid");
        if *due == 0 {
            self.due.remove(&(height, namespace));
        }
        let total = self.totals.entry(height).or_insert(0);
        *total = total
            .checked_add_signed(delta)
            .expect("model totals stay valid");
        if *total == 0 {
            self.totals.remove(&height);
        }
    }

    // -----------------------------------------------------------------------
    // Admin inputs.

    fn admin(&mut self, weights: &Weights) -> AdminOutcome {
        if let Err(reason) = weights.reservation() {
            return AdminOutcome::Rejected(reason);
        }
        if weights.id <= self.pending.id {
            return AdminOutcome::Rejected(AdminRejection::StaleSchedule {
                current: self.pending.id,
            });
        }
        self.pending = *weights;
        AdminOutcome::Applied
    }

    // -----------------------------------------------------------------------
    // The expiry sweep.

    fn delete_charges(&self, cells: &Cells) -> Vec<u64> {
        let w = &self.in_force;
        let mut charges = vec![
            w.directory_lookup,
            w.point_read,
            w.record_delete + w.cell_remove * count(cells.len()) + 3 * w.root_update,
        ];
        for cell in cells.values() {
            if cell.kind == CellKind::Attribute {
                charges.push(w.remove());
                charges.push(w.remove());
            }
        }
        charges
    }

    fn sweep(&mut self, height: u64, block_remaining: &mut u64) -> Vec<ExpiryDeletion> {
        let mut deletions = Vec::new();
        let due: Vec<((u64, u64), u64)> = self
            .due
            .range(..=(height, u64::MAX))
            .map(|(key, n)| (*key, *n))
            .collect();
        for ((at, namespace), n) in due {
            let state = self
                .namespaces
                .get(&namespace)
                .expect("due namespace exists");
            let mut expiring: Vec<(u64, Vec<u8>)> = state
                .records
                .iter()
                .filter(|(_, rec)| {
                    rec.cells.get(&expires_at()) == Some(&Cell::attribute(Value::U64(at)))
                })
                .map(|(key, rec)| (rec.id, key.clone()))
                .collect();
            expiring.sort();
            assert_eq!(count(expiring.len()), n, "oracle schedule consistent");
            for (id, key) in expiring {
                let cells = self.namespaces[&namespace].records[&key].cells.clone();
                let (outcome, spent) =
                    verdict(&self.delete_charges(&cells), None, *block_remaining);
                assert_eq!(outcome, Outcome::Applied, "the reservation covers expiry");
                *block_remaining -= spent;
                deletions.push(ExpiryDeletion {
                    namespace: ns(namespace),
                    record_id: RecordId::from_ordinal(id).unwrap(),
                    receipt: self.receipt(spent, Outcome::Applied),
                });
                self.namespaces
                    .get_mut(&namespace)
                    .unwrap()
                    .records
                    .remove(&key);
                self.schedule_one(at, namespace, -1);
            }
        }
        deletions
    }

    // -----------------------------------------------------------------------
    // User operations.

    /// Resolves an expiry pair at `height` per the brief.
    fn resolve(expiry: &Expiry, height: u64) -> Result<Life, ExpiryFault> {
        let floor = height
            .checked_add(expiry.min_lifetime)
            .ok_or(ExpiryFault::Overflow)?;
        let e = expiry.expires_at.max(floor);
        if e == u64::MAX {
            return Ok(Life::Forever);
        }
        if e <= height {
            return Err(ExpiryFault::DeadOnArrival);
        }
        if e > height + PARAMS.max_lifetime {
            return Err(ExpiryFault::TooFar);
        }
        Ok(Life::Until(e))
    }

    /// Runs one operation with `limit` units; applies its changes to the
    /// model when it applies.
    fn op(&mut self, actor: OwnerId, op: &TxOp, height: u64, limit: u64) -> OpOutcome {
        // Host pre-step for record operations.
        let (namespace, record_key, is_patch) = match op {
            TxOp::CreateRecord { namespace, key, .. } => (Some(*namespace), Some(key), false),
            TxOp::PatchRecord { namespace, key, .. } => (Some(*namespace), Some(key), true),
            TxOp::DeleteRecord { namespace, key } => (Some(*namespace), Some(key), false),
            _ => (None, None, false),
        };
        let old = match (namespace, record_key) {
            (Some(namespace), Some(key)) => self
                .namespaces
                .get(&namespace.ordinal())
                .and_then(|state| state.lifetime(key.as_bytes())),
            _ => None,
        };
        let new = match op {
            TxOp::CreateRecord {
                expiry: Some(expiry),
                ..
            }
            | TxOp::PatchRecord {
                expiry: Some(expiry),
                ..
            } => {
                let life = match Self::resolve(expiry, height) {
                    Ok(life) => life,
                    Err(fault) => {
                        return OpOutcome::HostRejected {
                            fault: HostFault::ExpiryInvalid(fault),
                        };
                    }
                };
                if is_patch && old.is_some_and(|old| life < old) {
                    return OpOutcome::HostRejected {
                        fault: HostFault::ExpiryShortened,
                    };
                }
                if let Life::Until(e) = life {
                    let total = self.totals.get(&e).copied().unwrap_or(0);
                    let after = total + 1 - u64::from(old == Some(life));
                    if after > u64::from(PARAMS.expiry_per_block) {
                        return OpOutcome::HostRejected {
                            fault: HostFault::ExpiryCapacityFull,
                        };
                    }
                }
                Some(life)
            }
            _ => None,
        };
        let injected = new.map(|life| match life {
            Life::Until(e) => Value::U64(e),
            Life::Forever => Value::U64(u64::MAX),
        });
        let outcome = self.engine(actor, op, injected, limit);
        if let OpOutcome::Applied { .. } = outcome {
            // Only record operations (the pre-step's `namespace`) move the
            // due schedule.
            let namespace = namespace.map(|id| id.ordinal());
            let change = match op {
                TxOp::DeleteRecord { .. } => Some((old, None)),
                _ if new.is_some() && new != old => Some((old, new)),
                _ => None,
            };
            if let (Some(namespace), Some((before, after))) = (namespace, change) {
                if let Some(Life::Until(h)) = before {
                    self.schedule_one(h, namespace, -1);
                }
                if let Some(Life::Until(h)) = after {
                    self.schedule_one(h, namespace, 1);
                }
            }
        }
        outcome
    }

    fn owned(&self, namespace: u64, actor: OwnerId) -> Result<(), Rejection> {
        match self.namespaces.get(&namespace) {
            None => Err(Rejection::UnknownNamespace),
            Some(state) if state.owner != actor => Err(Rejection::Unauthorized),
            Some(_) => Ok(()),
        }
    }

    /// The engine operation per the model-3 charge table.
    fn engine(
        &mut self,
        actor: OwnerId,
        op: &TxOp,
        injected: Option<Value>,
        limit: u64,
    ) -> OpOutcome {
        let w = self.in_force;
        let (charges, semantic, effect): (Vec<u64>, Option<(usize, Rejection)>, Option<Effect>) =
            match op {
                TxOp::Arkiv { .. } => {
                    panic!("the generic model never generates Arkiv operations")
                }
                TxOp::Transfer { .. } => {
                    return OpOutcome::HostRejected {
                        fault: HostFault::Account(arkiv_node::accounts::AccountFault::Unsupported),
                    };
                }
                TxOp::CreateNamespace { .. } => (
                    vec![w.directory_lookup, w.namespace_create + w.root_update],
                    None,
                    Some(Effect::NamespaceCreated(ns(self.next_namespace))),
                ),
                TxOp::TransferNamespace {
                    namespace,
                    expected_revision,
                    ..
                } => {
                    let semantic = self.owned(namespace.ordinal(), actor).err().or_else(|| {
                        let current = self.namespaces[&namespace.ordinal()].revision;
                        (current != expected_revision.0).then_some(Rejection::StaleRevision {
                            expected: *expected_revision,
                            current: Revision(current),
                        })
                    });
                    let revision = self
                        .namespaces
                        .get(&namespace.ordinal())
                        .map_or(0, |state| state.revision + 1);
                    (
                        vec![w.directory_lookup, w.namespace_transfer + w.root_update],
                        semantic.map(|reason| (0, reason)),
                        Some(Effect::Transferred {
                            revision: Revision(revision),
                        }),
                    )
                }
                TxOp::Put {
                    namespace,
                    key,
                    value,
                } => {
                    let replaced = self
                        .namespaces
                        .get(&namespace.ordinal())
                        .is_some_and(|state| state.raw.contains_key(key));
                    (
                        vec![
                            w.directory_lookup,
                            w.put_record
                                + w.write_key_byte * count(key.len())
                                + w.write_value_byte * count(value.len())
                                + 2 * w.root_update,
                        ],
                        self.owned(namespace.ordinal(), actor)
                            .err()
                            .map(|reason| (0, reason)),
                        Some(Effect::Put { replaced }),
                    )
                }
                TxOp::Delete { namespace, key } => {
                    let existed = self
                        .namespaces
                        .get(&namespace.ordinal())
                        .is_some_and(|state| state.raw.contains_key(key));
                    (
                        vec![
                            w.directory_lookup,
                            w.delete_record
                                + w.write_key_byte * count(key.len())
                                + 2 * w.root_update,
                        ],
                        self.owned(namespace.ordinal(), actor)
                            .err()
                            .map(|reason| (0, reason)),
                        Some(Effect::Deleted { existed }),
                    )
                }
                TxOp::CreateRecord {
                    namespace,
                    key,
                    cells,
                    ..
                } => self.create_charges(actor, namespace.ordinal(), key, cells, injected.clone()),
                TxOp::PatchRecord {
                    namespace,
                    key,
                    set,
                    remove,
                    ..
                } => self.patch_charges(
                    actor,
                    namespace.ordinal(),
                    key,
                    set,
                    remove,
                    injected.clone(),
                ),
                TxOp::DeleteRecord { namespace, key } => {
                    if let Err(reason) = self.owned(namespace.ordinal(), actor) {
                        (vec![w.directory_lookup], Some((0, reason)), None)
                    } else {
                        let state = &self.namespaces[&namespace.ordinal()];
                        match state.records.get(key.as_bytes()) {
                            None => (
                                vec![w.directory_lookup, w.point_read],
                                Some((1, Rejection::NotFound)),
                                None,
                            ),
                            Some(rec) => (
                                self.delete_charges(&rec.cells),
                                None,
                                Some(Effect::RecordDeleted {
                                    id: RecordId::from_ordinal(rec.id).unwrap(),
                                }),
                            ),
                        }
                    }
                }
            };
        let (outcome, spent) = verdict(&charges, semantic, limit);
        match outcome {
            Outcome::Rejected(reason) => OpOutcome::Rejected {
                reason,
                receipt: self.receipt(spent, outcome),
            },
            Outcome::Applied => {
                let effect = effect.expect("an applied operation has an effect");
                self.apply(actor, op, injected);
                OpOutcome::Applied {
                    effect,
                    receipt: self.receipt(spent, outcome),
                }
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn create_charges(
        &self,
        actor: OwnerId,
        namespace: u64,
        key: &RecordKey,
        cells: &[(CellName, Cell)],
        injected: Option<Value>,
    ) -> (Vec<u64>, Option<(usize, Rejection)>, Option<Effect>) {
        let w = &self.in_force;
        if let Err(reason) = self.owned(namespace, actor) {
            return (vec![w.directory_lookup], Some((0, reason)), None);
        }
        let state = &self.namespaces[&namespace];
        if state.records.contains_key(key.as_bytes()) {
            return (
                vec![w.directory_lookup, w.point_read],
                Some((1, Rejection::AlreadyExists)),
                None,
            );
        }
        let mut all: Cells = cells.iter().cloned().collect();
        if let Some(value) = injected {
            all.insert(expires_at(), Cell::attribute(value));
        }
        let bytes: u64 = all.iter().map(|(name, cell)| cell_bytes(name, cell)).sum();
        let mut charges = vec![
            w.directory_lookup,
            w.point_read,
            w.record_create
                + w.cell_write * count(all.len())
                + w.cell_byte * bytes
                + w.row_byte * (count(key.as_bytes().len()) + bytes)
                + 3 * w.root_update,
        ];
        for (name, cell) in &all {
            for term in terms_of(cell) {
                charges.push(w.insert(!state.holds(name, &term, None)));
            }
        }
        let id = RecordId::from_ordinal(state.next_record).unwrap();
        (charges, None, Some(Effect::RecordCreated { id }))
    }

    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn patch_charges(
        &self,
        actor: OwnerId,
        namespace: u64,
        key: &RecordKey,
        set: &[(CellName, Cell)],
        remove: &[CellName],
        injected: Option<Value>,
    ) -> (Vec<u64>, Option<(usize, Rejection)>, Option<Effect>) {
        let w = &self.in_force;
        if let Err(reason) = self.owned(namespace, actor) {
            return (vec![w.directory_lookup], Some((0, reason)), None);
        }
        let state = &self.namespaces[&namespace];
        let Some(rec) = state.records.get(key.as_bytes()) else {
            return (
                vec![w.directory_lookup, w.point_read],
                Some((1, Rejection::NotFound)),
                None,
            );
        };
        let mut set: Vec<(CellName, Cell)> = set.to_vec();
        if let Some(value) = injected {
            set.push((expires_at(), Cell::attribute(value)));
        }
        let mut after = rec.cells.clone();
        for (name, cell) in &set {
            after.insert(name.clone(), cell.clone());
        }
        for name in remove {
            after.remove(name);
        }
        if after.len() > LIMITS.max_cells {
            return (
                vec![w.directory_lookup, w.point_read],
                Some((1, Rejection::RecordFull)),
                None,
            );
        }
        let set_bytes: u64 = set.iter().map(|(name, cell)| cell_bytes(name, cell)).sum();
        let row: u64 = count(key.as_bytes().len())
            + after
                .iter()
                .map(|(name, cell)| cell_bytes(name, cell))
                .sum::<u64>();
        let mut charges = vec![
            w.directory_lookup,
            w.point_read,
            w.record_patch
                + w.cell_write * count(set.len())
                + w.cell_byte * set_bytes
                + w.cell_remove * count(remove.len())
                + w.row_byte * row
                + 2 * w.root_update,
        ];
        let names: BTreeSet<&CellName> = set
            .iter()
            .map(|(name, _)| name)
            .chain(remove.iter())
            .collect();
        for name in names {
            let before = rec.cells.get(name).map_or_else(Vec::new, terms_of);
            let now = after.get(name).map_or_else(Vec::new, terms_of);
            for _ in before.iter().filter(|term| !now.contains(term)) {
                charges.push(w.remove());
            }
            for term in now.iter().filter(|term| !before.contains(term)) {
                charges.push(w.insert(!state.holds(name, term, Some(rec.id))));
            }
        }
        let id = RecordId::from_ordinal(rec.id).unwrap();
        (charges, None, Some(Effect::RecordPatched { id }))
    }

    /// Applies an operation the oracle predicted to apply.
    fn apply(&mut self, actor: OwnerId, op: &TxOp, injected: Option<Value>) {
        match op {
            TxOp::Transfer { .. } => {
                panic!("the generic model never applies native account transfers")
            }
            TxOp::Arkiv { .. } => panic!("the generic model never generates Arkiv operations"),
            TxOp::CreateNamespace { name } => {
                self.namespaces.insert(
                    self.next_namespace,
                    NsModel {
                        name: *name,
                        owner: actor,
                        revision: 0,
                        raw: BTreeMap::new(),
                        records: BTreeMap::new(),
                        next_record: 1,
                    },
                );
                self.next_namespace += 1;
            }
            TxOp::TransferNamespace {
                namespace,
                new_owner,
                ..
            } => {
                let state = self.namespaces.get_mut(&namespace.ordinal()).unwrap();
                state.owner = *new_owner;
                state.revision += 1;
            }
            TxOp::Put {
                namespace,
                key,
                value,
            } => {
                let state = self.namespaces.get_mut(&namespace.ordinal()).unwrap();
                state.raw.insert(key.clone(), value.clone());
            }
            TxOp::Delete { namespace, key } => {
                let state = self.namespaces.get_mut(&namespace.ordinal()).unwrap();
                state.raw.remove(key);
            }
            TxOp::CreateRecord {
                namespace,
                key,
                cells,
                ..
            } => {
                let state = self.namespaces.get_mut(&namespace.ordinal()).unwrap();
                let mut all: Cells = cells.iter().cloned().collect();
                if let Some(value) = injected {
                    all.insert(expires_at(), Cell::attribute(value));
                }
                state.records.insert(
                    key.as_bytes().to_vec(),
                    Rec {
                        id: state.next_record,
                        cells: all,
                    },
                );
                state.next_record += 1;
            }
            TxOp::PatchRecord {
                namespace,
                key,
                set,
                remove,
                ..
            } => {
                let state = self.namespaces.get_mut(&namespace.ordinal()).unwrap();
                let rec = state.records.get_mut(key.as_bytes()).unwrap();
                for (name, cell) in set {
                    rec.cells.insert(name.clone(), cell.clone());
                }
                if let Some(value) = injected {
                    rec.cells.insert(expires_at(), Cell::attribute(value));
                }
                for name in remove {
                    rec.cells.remove(name);
                }
            }
            TxOp::DeleteRecord { namespace, key } => {
                let state = self.namespaces.get_mut(&namespace.ordinal()).unwrap();
                state.records.remove(key.as_bytes());
            }
        }
    }

    fn tx(&mut self, envelope: &TxEnvelope, height: u64) -> TxOutcome {
        let checkpoint = self.clone();
        let mut ops = Vec::new();
        let mut spent = 0u64;
        let mut failed = None;
        for (index, op) in envelope.ops.iter().enumerate() {
            let outcome = self.op(envelope.actor, op, height, envelope.tx_budget - spent);
            spent += charged(&outcome);
            let fails = !matches!(outcome, OpOutcome::Applied { .. });
            ops.push(outcome);
            if fails {
                failed = Some(u32::try_from(index).unwrap());
                break;
            }
        }
        let status = match failed {
            None => TxStatus::Committed,
            Some(op_index) => {
                // Roll back the transaction's state; keep the pricing
                // committed by admin inputs (the checkpoint holds it too).
                *self = checkpoint;
                for outcome in &mut ops {
                    if let OpOutcome::Applied { effect, receipt } = outcome {
                        *outcome = OpOutcome::RolledBack {
                            effect: *effect,
                            receipt: receipt.clone(),
                        };
                    }
                }
                while ops.len() < envelope.ops.len() {
                    ops.push(OpOutcome::NotExecuted);
                }
                TxStatus::Failed { op_index }
            }
        };
        TxOutcome { status, ops, spent }
    }

    /// Predicts a whole block and advances the model past it.
    fn block(&mut self, height: u64, admin: &[Weights], txs: &[TxEnvelope]) -> BlockOutcomes {
        let admin = admin.iter().map(|weights| self.admin(weights)).collect();
        let mut remaining = PARAMS.block_budget;
        let expiry = self.sweep(height, &mut remaining);
        let mut outcomes = Vec::new();
        for envelope in txs {
            assert!(
                remaining >= envelope.tx_budget,
                "the reservation leaves every declared budget"
            );
            let outcome = self.tx(envelope, height);
            remaining -= outcome.spent;
            outcomes.push(outcome);
        }
        let spent = expiry
            .iter()
            .map(|deletion: &ExpiryDeletion| deletion.receipt.spent.units())
            .chain(outcomes.iter().map(|tx| tx.spent))
            .sum();
        self.in_force = self.pending;
        BlockOutcomes {
            admin,
            expiry,
            txs: outcomes,
            spent,
        }
    }
}

// ---------------------------------------------------------------------------
// Queries: a catalogue evaluated by full scan over the model.

fn catalogue(height: u64) -> Vec<Query> {
    let name = |text| CellName::new(text).unwrap();
    vec![
        Query::match_all(),
        Query::term(Term::eq(name("a"), Value::U64(1))),
        Query::term(Term::exists(name("a"), TypeTag::U64)),
        Query::term(Term::range(
            name("$expiresAt"),
            Bound::Included(Value::U64(height)),
            Bound::Excluded(Value::U64(height + 5)),
        )),
        Query::term(Term::starts_with(name("s"), "x")),
        Query::term(Term::not_eq(name("b"), Value::Bool(true))),
        Query::all(vec![Literal::not(Term::exists(name("f"), TypeTag::Str))]),
        Query {
            groups: vec![
                vec![Literal::of(Term::eq(name("a"), Value::U64(2)))],
                vec![
                    Literal::of(Term::exists(name("s"), TypeTag::Str)),
                    Literal::not(Term::eq(name("n"), Value::I64(0))),
                ],
            ],
        },
    ]
}

/// The query subset's semantics, written from the query contract: a term
/// matches an attribute (never a field) of its name and type.
fn term_matches(term: &Term, cells: &Cells) -> bool {
    let attribute = |name: &CellName| {
        cells
            .get(name)
            .filter(|cell| cell.kind == CellKind::Attribute)
            .map(|cell| &cell.value)
    };
    match term {
        Term::Eq { name, value } => attribute(name) == Some(value),
        Term::NotEq { name, value } => {
            attribute(name).is_some_and(|held| held.tag() == value.tag() && held != value)
        }
        Term::Range { name, lower, upper } => match attribute(name) {
            Some(Value::U64(held)) => {
                let above = match lower {
                    Bound::Included(Value::U64(bound)) => held >= bound,
                    Bound::Excluded(Value::U64(bound)) => held > bound,
                    _ => true,
                };
                let below = match upper {
                    Bound::Included(Value::U64(bound)) => held <= bound,
                    Bound::Excluded(Value::U64(bound)) => held < bound,
                    _ => true,
                };
                above && below
            }
            _ => false,
        },
        Term::StartsWith { name, prefix } => match attribute(name) {
            Some(Value::Str(text)) => text.starts_with(&**prefix),
            _ => false,
        },
        Term::Exists { name, tag } => attribute(name).is_some_and(|held| held.tag() == *tag),
    }
}

fn model_query(state: &NsModel, query: &Query) -> Vec<(u64, Vec<u8>)> {
    let mut rows: Vec<(u64, Vec<u8>)> = state
        .records
        .iter()
        .filter(|(_, rec)| {
            query.groups.iter().any(|group| {
                group
                    .iter()
                    .all(|literal| term_matches(&literal.term, &rec.cells) != literal.negated)
            })
        })
        .map(|(key, rec)| (rec.id, key.clone()))
        .collect();
    rows.sort();
    rows
}

// ---------------------------------------------------------------------------
// Comparison of a snapshot with the model.

/// Every key the generator uses; each is checked present or absent.
fn keys() -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = (0..6).map(|n| format!("k{n}").into_bytes()).collect();
    keys.push(vec![0xee; 32]);
    keys
}

fn raw_keys() -> Vec<Vec<u8>> {
    vec![b"r0".to_vec(), b"r1".to_vec(), vec![0x72; 20]]
}

fn verify(snapshot: &Snapshot, model: &Model, height: u64, context: &str) {
    let unlimited = Budget::unlimited();
    assert_eq!(
        snapshot.pricing(),
        &model.in_force.schedule(),
        "{context}: pricing in force"
    );
    // Namespaces that exist, and the next one that does not.
    for ordinal in 1..=model.next_namespace {
        let read = snapshot.try_descriptor(ns(ordinal), unlimited).unwrap();
        let Some(state) = model.namespaces.get(&ordinal) else {
            assert_eq!(
                read.map(|_| ()).map_err(|rejected| rejected.reason),
                Err(Rejection::UnknownNamespace),
                "{context}: namespace {ordinal} absent"
            );
            continue;
        };
        let descriptor = read.expect("namespace exists").value;
        assert_eq!(
            (descriptor.name, descriptor.owner, descriptor.revision),
            (state.name, state.owner, Revision(state.revision)),
            "{context}: descriptor {ordinal}"
        );
        let entries: Vec<(Vec<u8>, Vec<u8>)> = snapshot
            .try_scan_prefix(ns(ordinal), b"", unlimited)
            .unwrap()
            .unwrap()
            .value
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect();
        let expected: Vec<(Vec<u8>, Vec<u8>)> = state
            .raw
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        assert_eq!(entries, expected, "{context}: raw map {ordinal}");
        let checked: BTreeSet<Vec<u8>> = keys()
            .into_iter()
            .chain(state.records.keys().cloned())
            .collect();
        for key in checked {
            let row = snapshot
                .try_get_record(
                    ns(ordinal),
                    &RecordKey::new(&key).unwrap(),
                    &Projection::ALL,
                    unlimited,
                )
                .unwrap()
                .unwrap()
                .value;
            let expected = state.records.get(&key).map(|rec| {
                (
                    rec.id,
                    rec.cells
                        .iter()
                        .map(|(name, cell)| (name.clone(), cell.clone()))
                        .collect::<Vec<_>>(),
                )
            });
            assert_eq!(
                row.map(|row| (row.id.ordinal(), row.cells)),
                expected,
                "{context}: record {ordinal}/{key:x?}"
            );
        }
        for query in catalogue(height) {
            let expected = model_query(state, &query);
            let rows = support::query_all(
                snapshot,
                ordinal,
                &query,
                &Projection::KEY_ONLY,
                LIMITS.max_page,
            );
            let found: Vec<(u64, Vec<u8>)> = rows
                .into_iter()
                .map(|row| (row.id.ordinal(), row.key.unwrap().as_bytes().to_vec()))
                .collect();
            assert_eq!(found, expected, "{context}: {query:?} in {ordinal}");
            let counted = snapshot
                .try_count(ns(ordinal), &query, unlimited)
                .unwrap()
                .unwrap()
                .value;
            assert_eq!(counted, count(expected.len()), "{context}: count");
        }
    }
    // The due schedule, entry by entry, with keys built from the brief.
    let mut expected: BTreeMap<[u8; 32], Vec<u8>> = BTreeMap::new();
    for ((at, namespace), n) in &model.due {
        expected.insert(due_key(*at, *namespace), n.to_be_bytes().to_vec());
    }
    for (at, n) in &model.totals {
        expected.insert(total_key(*at), n.to_be_bytes().to_vec());
    }
    assert_eq!(
        host_entries(snapshot),
        expected.into_iter().collect::<Vec<_>>(),
        "{context}: due schedule"
    );
}

// ---------------------------------------------------------------------------
// The generator.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[usize::try_from(self.below(count(items.len()))).unwrap()]
    }
}

#[derive(Default)]
struct Coverage {
    kinds: BTreeMap<String, u64>,
}

impl Coverage {
    fn note(&mut self, kind: impl Into<String>) {
        *self.kinds.entry(kind.into()).or_insert(0) += 1;
    }

    fn outcomes(&mut self, outcomes: &BlockOutcomes, txs: &[TxEnvelope], exact: &BTreeSet<usize>) {
        for outcome in &outcomes.admin {
            self.note(format!("admin {outcome:?}"));
        }
        if !outcomes.expiry.is_empty() {
            self.note("expiry deletions");
        }
        if outcomes.expiry.len() == usize::try_from(PARAMS.expiry_per_block).unwrap() {
            self.note("expiry at capacity");
        }
        for (index, (tx, envelope)) in outcomes.txs.iter().zip(txs).enumerate() {
            if let TxStatus::Failed { op_index } = tx.status
                && op_index > 0
            {
                self.note("failed mid-transaction");
            }
            if exact.contains(&index) && tx.status == TxStatus::Committed {
                self.note("exact budget committed");
            }
            if tx.spent == envelope.tx_budget && tx.status == TxStatus::Committed {
                self.note("budget spent exactly");
            }
            if let [
                TxOp::PatchRecord {
                    set,
                    remove,
                    expiry: Some(_),
                    ..
                },
            ] = envelope.ops.as_slice()
                && set.is_empty()
                && remove.is_empty()
                && tx.status == TxStatus::Committed
            {
                self.note("expiry-only patch");
            }
            for op in &tx.ops {
                let kind = match op {
                    OpOutcome::AccountRejected { fault, .. } => format!("account {fault:?}"),
                    OpOutcome::Applied { effect, .. } => format!("applied {effect:?}")
                        .split(['(', ' ', '{'])
                        .take(2)
                        .collect::<Vec<_>>()
                        .join(" "),
                    OpOutcome::RolledBack { .. } => "rolled back".to_owned(),
                    OpOutcome::Rejected { reason, .. } => format!("rejected {reason:?}")
                        .split(['(', '{'])
                        .next()
                        .unwrap()
                        .trim()
                        .to_owned(),
                    OpOutcome::AdmissionRejected { .. } => "admission rejected".to_owned(),
                    OpOutcome::HostRejected { fault } => format!("host {fault:?}"),
                    OpOutcome::NotExecuted => "not executed".to_owned(),
                };
                self.note(kind);
            }
        }
    }
}

/// Cell names the generator uses.
const NAMES: [&str; 5] = ["a", "b", "s", "f", "n"];
/// String values of the `s` attribute.
const STRINGS: [&str; 4] = ["x", "xy", "yz", ""];
/// Names of namespaces created after the first block.
const EXTRA_NAMESPACES: [&str; 3] = ["extra", "more", "spare"];

struct Generator {
    rng: Rng,
    next_request: BTreeMap<OwnerId, u64>,
    next_schedule: u32,
}

impl Generator {
    fn request(&mut self, actor: OwnerId) -> [u8; 32] {
        let next = self.next_request.entry(actor).or_insert(1);
        *next += 1;
        request(*next)
    }

    fn envelope(&mut self, actor: OwnerId, budget: u64, ops: Vec<TxOp>) -> TxEnvelope {
        TxEnvelope {
            chain_id: CHAIN,
            actor,
            request_id: self.request(actor),
            tx_budget: budget,
            ops,
        }
    }

    fn value_cell(&mut self, name: &str) -> Cell {
        match name {
            "a" => Cell::attribute(Value::U64(self.rng.below(4))),
            "b" => Cell::attribute(Value::Bool(self.rng.chance(50))),
            "s" => {
                let text = *self.rng.pick(&STRINGS);
                Cell::attribute(Value::str(text))
            }
            "f" => {
                if self.rng.chance(50) {
                    Cell::field(Value::bytes(&[0xf0; 5]))
                } else {
                    Cell::attribute(Value::str("q"))
                }
            }
            _ => {
                if self.rng.chance(70) {
                    Cell::attribute(Value::I64(i64::try_from(self.rng.below(5)).unwrap() - 2))
                } else {
                    Cell::field(Value::U64(self.rng.below(3)))
                }
            }
        }
    }

    fn cells(&mut self, most: u64) -> Vec<(CellName, Cell)> {
        let names = NAMES;
        let wanted = self.rng.below(most + 1);
        let mut chosen = BTreeSet::new();
        while count(chosen.len()) < wanted {
            chosen.insert(*self.rng.pick(&names));
        }
        let mut cells: Vec<(CellName, Cell)> = chosen
            .into_iter()
            .map(|name| (CellName::new(name).unwrap(), self.value_cell(name)))
            .collect();
        // Any order is valid; shuffle by rotation.
        if !cells.is_empty() {
            let shift = usize::try_from(self.rng.below(count(cells.len()))).unwrap();
            cells.rotate_left(shift);
        }
        cells
    }

    fn expiry(&mut self, height: u64) -> Expiry {
        let max = PARAMS.max_lifetime;
        match self.rng.below(20) {
            0..=5 => until(height + 1),
            6..=10 => until(height + 1 + self.rng.below(3)),
            11 | 12 => Expiry {
                expires_at: 0,
                min_lifetime: 1 + self.rng.below(3),
            },
            13 => Expiry {
                expires_at: height + 2,
                min_lifetime: max,
            },
            14 | 15 => Expiry::PERMANENT,
            16 => until(height),
            17 => until(height + max + 1),
            _ => until(height + 4 + self.rng.below(6)),
        }
    }

    /// A record key of `namespace`: usually a live one (`live`) or a free
    /// one (not `live`) per `model`, sometimes any key of the pool.
    fn record_key(&mut self, model: &Model, namespace: u64, live: bool) -> RecordKey {
        let pool: Vec<Vec<u8>> = keys()
            .into_iter()
            .filter(|key| {
                model
                    .namespaces
                    .get(&namespace)
                    .is_some_and(|state| state.records.contains_key(key) == live)
            })
            .collect();
        let key = if !pool.is_empty() && self.rng.chance(80) {
            self.rng.pick(&pool).clone()
        } else {
            self.rng.pick(&keys()).clone()
        };
        RecordKey::new(&key).unwrap()
    }

    /// A namespace for `actor`: mostly one it owns (per `model`), sometimes
    /// someone else's, rarely one that does not exist.
    fn namespace(&mut self, model: &Model, actor: OwnerId) -> u64 {
        let owned: Vec<u64> = model
            .namespaces
            .iter()
            .filter(|(id, state)| **id != ENTITY_NAMESPACE && state.owner == actor)
            .map(|(id, _)| *id)
            .collect();
        let all: Vec<u64> = model
            .namespaces
            .keys()
            .copied()
            .filter(|id| *id != ENTITY_NAMESPACE)
            .collect();
        match self.rng.below(40) {
            0 => 99,
            1..=3 if !all.is_empty() => *self.rng.pick(&all),
            _ if !owned.is_empty() => *self.rng.pick(&owned),
            _ if !all.is_empty() => *self.rng.pick(&all),
            _ => 99,
        }
    }

    fn op(&mut self, model: &Model, actor: OwnerId, height: u64) -> TxOp {
        let ordinal = self.namespace(model, actor);
        let namespace = ns(ordinal);
        match self.rng.below(20) {
            0..=6 => {
                let expiry = self.rng.chance(60).then(|| self.expiry(height));
                let most = count(LIMITS.max_cells) - u64::from(expiry.is_some());
                TxOp::CreateRecord {
                    namespace,
                    key: self.record_key(model, ordinal, false),
                    cells: self.cells(most),
                    expiry,
                }
            }
            7..=11 => {
                let expiry = self.rng.chance(35).then(|| self.expiry(height));
                let set = self.cells(2);
                let mut remove = Vec::new();
                if self.rng.chance(40) {
                    let text = *self.rng.pick(&NAMES);
                    let name = CellName::new(text).unwrap();
                    if !set.iter().any(|(set_name, _)| *set_name == name) {
                        remove.push(name);
                    }
                }
                let mut set = set;
                if set.is_empty() && remove.is_empty() && expiry.is_none() {
                    set.push((CellName::new("a").unwrap(), self.value_cell("a")));
                }
                TxOp::PatchRecord {
                    namespace,
                    key: self.record_key(model, ordinal, true),
                    set,
                    remove,
                    expiry,
                }
            }
            12..=14 => TxOp::DeleteRecord {
                namespace,
                key: self.record_key(model, ordinal, true),
            },
            15 | 16 => TxOp::Put {
                namespace,
                key: self.rng.pick(&raw_keys()).clone(),
                value: vec![0x76; usize::try_from(self.rng.below(4)).unwrap()],
            },
            17 => TxOp::Delete {
                namespace,
                key: self.rng.pick(&raw_keys()).clone(),
            },
            18 => TxOp::TransferNamespace {
                namespace,
                expected_revision: Revision(
                    model
                        .namespaces
                        .get(&namespace.ordinal())
                        .map_or(0, |state| state.revision + self.rng.below(2)),
                ),
                new_owner: *self.rng.pick(&ACTORS),
            },
            _ => TxOp::CreateNamespace {
                name: {
                    let text = *self.rng.pick(&EXTRA_NAMESPACES);
                    NamespaceName::new(text).unwrap()
                },
            },
        }
    }

    /// Chooses a budget for `ops` run by `actor` on `model` (a copy of the
    /// block's state so far): ample, exact, or one short of the cost it
    /// would have with an ample budget.
    fn budget(&mut self, model: &Model, actor: OwnerId, ops: &[TxOp], height: u64) -> (u64, bool) {
        let mut probe = model.clone();
        let trial = TxEnvelope {
            chain_id: CHAIN,
            actor,
            request_id: [0; 32],
            tx_budget: AMPLE,
            ops: ops.to_vec(),
        };
        let cost = probe.tx(&trial, height).spent.max(1);
        match self.rng.below(10) {
            0 | 1 => (cost, true),
            2 | 3 if cost > 1 => (cost - 1, false),
            _ => (AMPLE, false),
        }
    }

    fn admin(&mut self, current: u32) -> (AdminInput, Weights) {
        let id = self.next_schedule;
        let weights = match self.rng.below(8) {
            0 => Weights::uniform(id, 2),
            1 => Weights::reads_x100(id),
            2 | 3 => Weights::reshaped(id),
            4 => Weights::too_dear(id),
            5 => Weights::overflowing(id),
            6 => Weights {
                id: current,
                ..FIXTURE
            },
            _ => Weights { id, ..FIXTURE },
        };
        if weights.id == id {
            self.next_schedule += 1;
        }
        let input = AdminInput {
            actor: ADMIN,
            request_id: self.request(ADMIN),
            action: arkiv_node::AdminAction::SetPricing(weights.schedule()),
        };
        (input, weights)
    }
}

const fn until(expires_at: u64) -> Expiry {
    Expiry {
        expires_at,
        min_lifetime: 0,
    }
}

/// Inputs for one block, generated against a working copy of the oracle
/// (the admin inputs, the sweep and each transaction are run on it as they
/// are chosen). Returns the inputs, the oracle weights of the admin inputs
/// and the positions of the exact-budget transactions.
fn generate(
    generator: &mut Generator,
    model: &Model,
    height: u64,
) -> (BlockInputs, Vec<Weights>, BTreeSet<usize>) {
    let mut work = model.clone();
    let mut inputs = BlockInputs::default();
    let mut weights = Vec::new();
    if height == 1 {
        for (actor, name) in [
            (ADMIN, "entities"),
            (ALICE, "alpha"),
            (BOB, "beta"),
            (CAROL, "gamma"),
        ] {
            let op = TxOp::CreateNamespace {
                name: NamespaceName::new(name).unwrap(),
            };
            inputs.txs.push(generator.envelope(actor, AMPLE, vec![op]));
        }
        return (inputs, weights, BTreeSet::new());
    }
    if height.is_multiple_of(11) {
        return (inputs, weights, BTreeSet::new());
    }
    let admin_count = match generator.rng.below(10) {
        0 => 2,
        1 | 2 => 1,
        _ => 0,
    };
    for _ in 0..admin_count {
        let (input, oracle) = generator.admin(work.pending.id);
        work.admin(&oracle);
        inputs.admin.push(input);
        weights.push(oracle);
    }
    let mut remaining = PARAMS.block_budget;
    work.sweep(height, &mut remaining);
    let mut scripted: Vec<(OwnerId, Vec<TxOp>)> = Vec::new();
    if height.is_multiple_of(7) {
        // A same-block transfer: the owner of a namespace hands it over,
        // then both the old and the new owner write to it.
        if let Some((&id, state)) = work
            .namespaces
            .iter()
            .filter(|(id, _)| **id != ENTITY_NAMESPACE)
            .nth(usize::try_from(generator.rng.below(count(work.namespaces.len() - 1))).unwrap())
        {
            let old = state.owner;
            let new = *ACTORS.iter().find(|actor| **actor != old).unwrap();
            let put = TxOp::Put {
                namespace: ns(id),
                key: b"r0".to_vec(),
                value: height.to_be_bytes().to_vec(),
            };
            scripted.push((
                old,
                vec![TxOp::TransferNamespace {
                    namespace: ns(id),
                    expected_revision: Revision(state.revision),
                    new_owner: new,
                }],
            ));
            scripted.push((old, vec![put.clone()]));
            scripted.push((new, vec![put]));
        }
    }
    if height.is_multiple_of(5) {
        // A transaction failing in the middle.
        let actor = *generator.rng.pick(&ACTORS);
        if let Some((&id, _)) = work
            .namespaces
            .iter()
            .find(|(_, state)| state.owner == actor)
        {
            scripted.push((
                actor,
                vec![
                    TxOp::Put {
                        namespace: ns(id),
                        key: b"r1".to_vec(),
                        value: b"mid".to_vec(),
                    },
                    TxOp::PatchRecord {
                        namespace: ns(id),
                        key: RecordKey::new(b"never").unwrap(),
                        set: vec![(CellName::new("a").unwrap(), Cell::attribute(Value::U64(1)))],
                        remove: Vec::new(),
                        expiry: None,
                    },
                    TxOp::Delete {
                        namespace: ns(id),
                        key: b"r0".to_vec(),
                    },
                ],
            ));
        }
    }
    if height.is_multiple_of(3) {
        // Four creations due at the next height: one over capacity.
        let actor = *generator.rng.pick(&ACTORS);
        if let Some((&id, _)) = work
            .namespaces
            .iter()
            .find(|(_, state)| state.owner == actor)
        {
            let tens = b'0' + u8::try_from(height % 10).unwrap();
            for index in 0..4u8 {
                scripted.push((
                    actor,
                    vec![TxOp::CreateRecord {
                        namespace: ns(id),
                        key: RecordKey::new(&[b'c', tens, b'0' + index]).unwrap(),
                        cells: Vec::new(),
                        expiry: Some(until(height + 1)),
                    }],
                ));
            }
            // A patch that keeps its record's height is admitted even when
            // that height is full.
            scripted.push((
                actor,
                vec![TxOp::PatchRecord {
                    namespace: ns(id),
                    key: RecordKey::new(&[b'c', tens, b'0']).unwrap(),
                    set: Vec::new(),
                    remove: Vec::new(),
                    expiry: Some(until(height + 1)),
                }],
            ));
        }
    }
    let random = generator.rng.below(u64::from(PARAMS.max_txs_per_block) + 1);
    let mut exact = BTreeSet::new();
    let mut ops_total = 0u64;
    let mut budget_total = 0u64;
    for index in 0..u64::from(PARAMS.max_txs_per_block) {
        let (actor, ops) = if let Some(next) = scripted.first() {
            let next = next.clone();
            scripted.remove(0);
            next
        } else if index < random {
            // Mostly short transactions, sometimes the longest; each
            // operation is generated against the state its predecessors
            // leave (as far as they apply).
            let actor = *generator.rng.pick(&ACTORS);
            let len = if generator.rng.chance(25) {
                u64::from(PARAMS.max_ops_per_tx)
            } else {
                1 + generator.rng.below(2)
            };
            let mut ahead = work.clone();
            let mut ops = Vec::new();
            for _ in 0..len {
                let op = generator.op(&ahead, actor, height);
                ahead.op(actor, &op, height, AMPLE);
                ops.push(op);
            }
            (actor, ops)
        } else {
            break;
        };
        let (budget, is_exact) = generator.budget(&work, actor, &ops, height);
        if ops_total + count(ops.len()) > u64::from(PARAMS.max_ops_per_block)
            || budget_total + budget > PARAMS.user_budget
        {
            break;
        }
        ops_total += count(ops.len());
        budget_total += budget;
        let envelope = generator.envelope(actor, budget, ops);
        if is_exact {
            exact.insert(inputs.txs.len());
        }
        work.tx(&envelope, height);
        inputs.txs.push(envelope);
    }
    (inputs, weights, exact)
}

fn genesis() -> Genesis {
    genesis_with(PARAMS, LIMITS, FIXTURE.schedule())
}

/// One seed: generate, predict, produce, compare; returns the chain, its
/// blocks and the model at every height.
fn run_seed(seed: u64, coverage: &mut Coverage) -> (MemoryChain, Vec<Block>, Vec<Model>) {
    let spec = ChainSpec::new(genesis()).expect("valid genesis");
    let mut chain = MemoryChain::new(spec);
    let mut generator = Generator {
        rng: Rng(seed | 1),
        next_request: BTreeMap::new(),
        next_schedule: 2,
    };
    let mut model = Model::new();
    let mut history = vec![model.clone()];
    let mut blocks = Vec::new();
    verify(
        &chain.snapshot(0).unwrap(),
        &model,
        0,
        &format!("seed {seed:#x} genesis"),
    );
    for height in 1..=BLOCKS {
        let (inputs, weights, exact) = generate(&mut generator, &model, height);
        let predicted = model.block(height, &weights, &inputs.txs);
        let timestamp = 1_000 + height * 1_000;
        let block = chain
            .produce(timestamp, inputs.clone())
            .unwrap_or_else(|error| panic!("seed {seed:#x} block {height}: {error}"));
        assert_eq!(
            block.outcomes, predicted,
            "seed {seed:#x} block {height}: outcomes"
        );
        coverage.outcomes(&block.outcomes, &inputs.txs, &exact);
        let context = format!("seed {seed:#x} block {height}");
        verify(&chain.snapshot(height).unwrap(), &model, height, &context);
        history.push(model.clone());
        blocks.push(block);
    }
    (chain, blocks, history)
}

#[test]
fn generated_blocks_match_the_oracle() {
    let mut coverage = Coverage::default();
    for seed in SEEDS {
        let (chain, blocks, history) = run_seed(seed, &mut coverage);
        // Replay on a fresh engine reproduces every block; every height of
        // both chains still matches the model's state at that height.
        let replayed = replay(&genesis(), &blocks).expect("replay reproduces the chain");
        for (height, model) in history.iter().enumerate() {
            let height = count(height);
            let context = format!("seed {seed:#x} history {height}");
            verify(&chain.snapshot(height).unwrap(), model, height, &context);
            verify(&replayed.snapshot(height).unwrap(), model, height, &context);
        }
    }
    let kinds = &coverage.kinds;
    for required in [
        "admin Applied",
        "admin Rejected(ReservationViolated)",
        "admin Rejected(ReservationOverflow)",
        "expiry deletions",
        "expiry at capacity",
        "failed mid-transaction",
        "exact budget committed",
        "expiry-only patch",
        "rolled back",
        "not executed",
        "rejected OutOfBudget",
        "rejected Unauthorized",
        "rejected UnknownNamespace",
        "rejected AlreadyExists",
        "rejected NotFound",
        "rejected StaleRevision",
        "host ExpiryCapacityFull",
        "host ExpiryInvalid(DeadOnArrival)",
        "host ExpiryInvalid(TooFar)",
        "host ExpiryShortened",
        "applied NamespaceCreated",
        "applied Transferred",
        "applied Put",
        "applied Deleted",
        "applied RecordCreated",
        "applied RecordPatched",
        "applied RecordDeleted",
    ] {
        assert!(
            kinds.contains_key(required),
            "coverage lacks {required}: {kinds:?}"
        );
    }
    assert!(
        kinds
            .keys()
            .any(|kind| kind.starts_with("admin Rejected(StaleSchedule")),
        "coverage lacks a stale schedule: {kinds:?}"
    );
    assert!(!kinds.contains_key("admission rejected"));
}

/// The oracle's reservation arithmetic agrees with the parameters: every
/// schedule the generator treats as valid fits, the others do not.
#[test]
fn oracle_reservation_matches_the_parameters() {
    for weights in [
        FIXTURE,
        Weights::uniform(2, 2),
        Weights::reads_x100(3),
        Weights::reshaped(4),
    ] {
        assert_eq!(weights.reservation(), Ok(()), "{weights:?}");
    }
    assert_eq!(
        Weights::too_dear(5).reservation(),
        Err(AdminRejection::ReservationViolated)
    );
    assert_eq!(
        Weights::overflowing(6).reservation(),
        Err(AdminRejection::ReservationOverflow)
    );
    // The dearest valid deletion is exactly the one the block budget
    // reserves for: one more unit breaks it.
    let tight = Weights {
        record_delete: 21,
        ..Weights::reads_x100(7)
    };
    assert_eq!(
        tight.reservation(),
        Err(AdminRejection::ReservationViolated)
    );
}

#[path = "support/block_roots.rs"]
mod root_model;

#[test]
fn continuous_demo_200_blocks_match_independent_roots_outcomes_and_pages() {
    use arkiv_db::proof::{EqQuery, EqRequest, ProofLimits, SnapshotRef, VerifiedEq, verify_eq};
    use arkiv_node::demo::{
        auth::{SignedHeader, TrustConfig},
        runtime::{Node, Role},
        wire, workload,
    };
    let genesis = workload::genesis(CHAIN, 1000);
    let key = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
    let trust = TrustConfig {
        genesis_hash: genesis.hash(),
        public_key: key.verifying_key().to_bytes(),
        genesis,
    };
    let mut producer = Node::producer(trust.clone(), key.clone(), 202, Some(7)).unwrap();
    let signed = SignedHeader::sign(trust.genesis_hash, trust.genesis.header(), &key);
    let mut fulls = [
        Node::follower(
            Role::Full,
            trust.clone(),
            signed.clone(),
            202,
            "unused".into(),
        )
        .unwrap(),
        Node::follower(
            Role::Full,
            trust.clone(),
            signed.clone(),
            202,
            "unused".into(),
        )
        .unwrap(),
    ];
    let mut lights = [
        Node::follower(
            Role::Light,
            trust.clone(),
            signed.clone(),
            202,
            "unused".into(),
        )
        .unwrap(),
        Node::follower(Role::Light, trust.clone(), signed, 202, "unused".into()).unwrap(),
    ];
    assert!(lights.iter().all(|n| n.chain.is_none()));
    let mut model = Model::new();
    let mut history = Vec::new();
    assert_eq!(trust.genesis.header().state_root, root_model::root(&model));
    for height in 1..=200 {
        let inputs = workload::inputs(CHAIN, height, 7);
        let predicted = model.block(height, &[], &inputs.txs);
        let body = producer.produce().unwrap();
        let block: Block = wire::unpack(&body.block, wire::RESPONSE_CAP).unwrap();
        assert_eq!(
            block.outcomes, predicted,
            "independent outcomes at {height}"
        );
        assert_eq!(
            block.header.state_root,
            root_model::root(&model),
            "independent root at {height}"
        );
        for full in &mut fulls {
            full.apply_body(body.clone()).unwrap();
            assert_eq!(full.chain.as_ref().unwrap().head(), &block.header);
        }
        for light in &mut lights {
            light
                .headers
                .accept(wire::unpack(&body.signed_header, 1024).unwrap())
                .unwrap();
        }
        history.push((block.header, model.clone()));
    }
    for (header, model) in history
        .iter()
        .filter(|(h, _)| h.height <= 20 || h.height % 17 == 0)
    {
        for full in &fulls {
            for light in &lights {
                for limit in [1, 3, 64] {
                    let snap = full
                        .chain
                        .as_ref()
                        .unwrap()
                        .snapshot(header.height)
                        .unwrap();
                    let accepted = light.headers.at(header.height).unwrap().header;
                    let attribute = CellName::new("group").unwrap();
                    let mut request = EqRequest {
                        snapshot: SnapshotRef {
                            genesis_hash: trust.genesis_hash,
                            height: header.height,
                            block_hash: accepted.hash(),
                            state_root: accepted.state_root,
                        },
                        query: EqQuery {
                            namespace: ns(2),
                            attribute: attribute.clone(),
                            value: Value::U64(1),
                        },
                        limit,
                        continuation: None,
                    };
                    let mut expected: Vec<_> = model.namespaces[&2]
                        .records
                        .iter()
                        .filter(|(_, r)| {
                            r.cells.get(&attribute) == Some(&Cell::attribute(Value::U64(1)))
                        })
                        .map(|(k, r)| {
                            (
                                r.id,
                                k.clone(),
                                r.cells
                                    .iter()
                                    .map(|(n, c)| (n.clone(), c.clone()))
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect();
                    expected.sort_by_key(|r| r.0);
                    let mut previous = None;
                    let mut actual = Vec::new();
                    loop {
                        let generated = snap
                            .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
                            .unwrap();
                        let VerifiedEq::Page(page) = verify_eq(
                            &request,
                            previous.as_ref(),
                            &generated.bytes,
                            ProofLimits::default(),
                            trust.genesis.limits,
                        )
                        .unwrap() else {
                            panic!("existing namespace")
                        };
                        actual.extend(page.rows.into_iter().map(|r| {
                            (r.id.ordinal(), r.key.unwrap().as_bytes().to_vec(), r.cells)
                        }));
                        previous = page.next;
                        if previous.is_none() {
                            break;
                        }
                        request.continuation = Some(previous.as_ref().unwrap().cursor().clone());
                    }
                    assert_eq!(
                        actual, expected,
                        "historical Eq at {} page size {limit}",
                        header.height
                    );
                }
            }
        }
    }
}
