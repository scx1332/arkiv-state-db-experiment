//! Independent model test: generated block histories against an oracle.
//!
//! The oracle predicts every verdict, receipt, effect, namespace ordinal and
//! read result from its own published log plus the open candidate's pending
//! entries, and only then looks at what the engine did. Costs are computed
//! from the CHARGE table below (transcribed from the decisions file), never
//! from the engine's pricing API.

use std::{collections::BTreeMap, fmt, ops::Range};

use arkiv_db::{
    BlockMap, BlockNumber, Branch, Budget, Checkpoint, Cost, Effect, Engine, EngineError, Entry,
    HostError, NamespaceDescriptor, NamespaceId, OperationClass, Outcome, OwnerId, PhysicalTrace,
    PriceSchedule, ReadResult, Receipt, ReceiptKind, Rejection, Revision, SealedCandidate,
    Snapshot, SnapshotId, WriteOp, WriteResult,
};

const KEYS: &[&[u8]] = &[
    b"",
    b"\0",
    b"\0\0",
    b"\0\xff",
    b"\x01",
    b"a",
    b"a\0",
    b"a/",
    b"a/\0",
    b"a/b",
    b"a\xff",
    b"\x7f",
    b"\x80",
    b"\xfe",
    b"\xff",
    b"\xff\0",
    b"\xff\xff",
    b"\xff\xff\0",
];

const SEEDS: [u32; 4] = [1, 0x9e37_79b9, 0xdead_beef, 0xa5a5_a5a5];
const BLOCKS: u64 = 192;
/// Block sizes cycle through these from block 2 on; block 1 is the two seeds.
const SIZES: [usize; 6] = [0, 1, 2, 8, 32, 64];
const ADMIN_EVERY: u64 = 17;
const LOSER_EVERY: u64 = 11;
const TRANSACTION_OPS: usize = 5;
/// An ordinal no run allocates: creations are capped far below it.
const SENTINEL: u64 = 1000;

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);
const CAROL: OwnerId = OwnerId::new([0xca; 20]);
const OWNERS: [OwnerId; 3] = [ALICE, BOB, CAROL];

fn owner_name(owner: OwnerId) -> &'static str {
    if owner == ALICE {
        "alice"
    } else if owner == BOB {
        "bob"
    } else {
        "carol"
    }
}

fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_bytes(ordinal.to_be_bytes()).expect("ordinals start at 1")
}

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("lengths fit u64")
}

fn count(items: usize) -> u64 {
    u64::try_from(items).expect("counts fit u64")
}

fn total(charges: &[u64]) -> u64 {
    charges.iter().sum()
}

// ---------------------------------------------------------------------------
// CHARGE table: unit prices and charge points, transcribed from the decisions
// file. Plain u64 arithmetic; the engine's pricing API is never consulted.
// ---------------------------------------------------------------------------

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
    root_update: u64,
}

/// `PriceSchedule::FIXTURE` as listed in the decisions file (id 1).
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
    root_update: 2,
};

impl Weights {
    const fn uniform(id: u32, weight: u64) -> Self {
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
            root_update: weight,
        }
    }

    // Each element below is one atomic charge point, in charge order
    // (model version 3).

    /// CreateNamespace: DirectoryLookup 1; group {NamespaceCreate 1, RootUpdate 1}.
    fn create_charges(&self) -> Vec<u64> {
        vec![
            self.directory_lookup,
            self.namespace_create + self.root_update,
        ]
    }

    /// TransferNamespace: DirectoryLookup 1; [checks]; group
    /// {NamespaceTransfer 1, RootUpdate 1}.
    fn transfer_charges(&self) -> Vec<u64> {
        vec![
            self.directory_lookup,
            self.namespace_transfer + self.root_update,
        ]
    }

    /// Put: DirectoryLookup 1; [checks]; group {PutRecord 1, WriteKeyByte k,
    /// WriteValueByte v, RootUpdate 2}.
    fn put_charges(&self, key: u64, value: u64) -> Vec<u64> {
        vec![
            self.directory_lookup,
            self.put_record
                + self.write_key_byte * key
                + self.write_value_byte * value
                + self.root_update * 2,
        ]
    }

    /// Delete: DirectoryLookup 1; [checks]; group {DeleteRecord 1,
    /// WriteKeyByte k, RootUpdate 2}, whether or not the key exists.
    fn delete_charges(&self, key: u64) -> Vec<u64> {
        vec![
            self.directory_lookup,
            self.delete_record + self.write_key_byte * key + self.root_update * 2,
        ]
    }

    /// get: DirectoryLookup 1; [unknown]; PointRead 1; if present ReadByte v.
    fn get_charges(&self, value: Option<u64>) -> Vec<u64> {
        let mut charges = vec![self.directory_lookup, self.point_read];
        if let Some(value) = value {
            charges.push(self.read_byte * value);
        }
        charges
    }

    /// scan: DirectoryLookup 1; [unknown]; ScanSeek 1; per entry group
    /// {ScanEntry 1, ReadByte k+v}.
    fn scan_charges(&self, entries: &[Entry]) -> Vec<u64> {
        let mut charges = vec![self.directory_lookup, self.scan_seek];
        for entry in entries {
            charges.push(self.scan_entry + self.read_byte * (len(&entry.key) + len(&entry.value)));
        }
        charges
    }

    /// descriptor: DirectoryLookup 1; [unknown].
    fn descriptor_charges(&self) -> Vec<u64> {
        vec![self.directory_lookup]
    }
}

fn charges_for(weights: &Weights, op: &WriteOp) -> Vec<u64> {
    match op {
        WriteOp::CreateNamespace { .. } => weights.create_charges(),
        WriteOp::TransferNamespace { .. } => weights.transfer_charges(),
        WriteOp::Put { key, value, .. } => weights.put_charges(len(key), len(value)),
        WriteOp::Delete { key, .. } => weights.delete_charges(len(key)),
        WriteOp::CreateRecord { .. }
        | WriteOp::PatchRecord { .. }
        | WriteOp::DeleteRecord { .. } => {
            unreachable!("the milestone-1 oracle issues no typed record operations")
        }
    }
}

/// The admin schedule committed by the `n`-th admin candidate. Both halves
/// are written from the same literals; the oracle never reads weights back.
fn admin_schedule(n: u32, id: u32) -> (PriceSchedule, Weights) {
    match n % 4 {
        0 => (PriceSchedule::uniform(id, 3), Weights::uniform(id, 3)),
        1 => (
            PriceSchedule::FIXTURE
                .with_id(id)
                .with_price(OperationClass::DirectoryLookup, 2)
                .with_price(OperationClass::PutRecord, 7),
            Weights {
                id,
                directory_lookup: 2,
                put_record: 7,
                ..FIXTURE
            },
        ),
        2 => (PriceSchedule::uniform(id, 1), Weights::uniform(id, 1)),
        _ => (
            PriceSchedule::FIXTURE
                .with_id(id)
                .with_price(OperationClass::ReadByte, 0)
                .with_price(OperationClass::WriteValueByte, 0)
                .with_price(OperationClass::ScanEntry, 6),
            Weights {
                id,
                read_byte: 0,
                write_value_byte: 0,
                scan_entry: 6,
                ..FIXTURE
            },
        ),
    }
}

/// Walks the charge points. A charge that would push the running total past
/// `limit` stops the operation with OutOfBudget and the total unchanged; the
/// semantic check (unknown namespace, authorization, revision) sits after the
/// first charge, so it costs exactly the DirectoryLookup weight.
fn verdict(charges: &[u64], semantic: Option<Rejection>, limit: u64) -> (Outcome, u64) {
    let mut spent = 0u64;
    for (index, &charge) in charges.iter().enumerate() {
        if spent + charge > limit {
            return (Outcome::Rejected(Rejection::OutOfBudget), spent);
        }
        spent += charge;
        if index == 0
            && let Some(reason) = semantic
        {
            return (Outcome::Rejected(reason), spent);
        }
    }
    (Outcome::Applied, spent)
}

fn receipt(kind: ReceiptKind, schedule_id: u32, spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind,
        model_version: 3,
        schedule_id,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

/// Directory nodes a read may visit, whatever the namespace count. The
/// directory is a Merkle Patricia trie keyed by 8-byte big-endian ids, so a
/// key has 16 nibbles; every branch on a lookup path consumes one nibble,
/// every extension at least one and is followed by a branch, and the path
/// ends at most at one leaf: at most `2·8 + 1` = 17 nodes.
const DIRECTORY_READ_BOUND: u64 = 2 * 8 + 1;

/// Directory nodes a write may visit: the lookup path and then the insert
/// path of the updated catalog leaf, each at most 17 nodes (a namespace
/// creation walks only the insert path).
const DIRECTORY_WRITE_BOUND: u64 = 2 * DIRECTORY_READ_BOUND;

// ---------------------------------------------------------------------------
// Oracle: published log, projected views and the open candidate.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mutation {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

impl Mutation {
    fn key(&self) -> &[u8] {
        match self {
            Self::Put(key, _) | Self::Delete(key) => key,
        }
    }

    fn value(&self) -> Option<&[u8]> {
        match self {
            Self::Put(_, value) => Some(value),
            Self::Delete(_) => None,
        }
    }
}

/// What every read must return at one state.
#[derive(Clone, Debug, Default)]
struct View {
    descriptors: BTreeMap<NamespaceId, (OwnerId, Revision)>,
    records: BTreeMap<(NamespaceId, Vec<u8>), Vec<u8>>,
}

impl View {
    fn apply(&mut self, namespace: NamespaceId, mutation: &Mutation) {
        match mutation {
            Mutation::Put(key, value) => {
                self.records.insert((namespace, key.clone()), value.clone());
            }
            Mutation::Delete(key) => {
                self.records.remove(&(namespace, key.clone()));
            }
        }
    }

    fn knows(&self, namespace: NamespaceId) -> bool {
        self.descriptors.contains_key(&namespace)
    }

    fn descriptor(&self, namespace: NamespaceId) -> Option<NamespaceDescriptor> {
        self.descriptors
            .get(&namespace)
            .map(|&(owner, revision)| NamespaceDescriptor {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
                id: namespace,
                owner,
                revision,
            })
    }

    fn value(&self, namespace: NamespaceId, key: &[u8]) -> Option<&Vec<u8>> {
        self.records.get(&(namespace, key.to_vec()))
    }

    fn entries(&self, namespace: NamespaceId, prefix: &[u8]) -> Vec<Entry> {
        self.records
            .iter()
            .filter(|((owner, key), _)| *owner == namespace && key.starts_with(prefix))
            .map(|((_, key), value)| Entry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect()
    }

    fn record_count(&self, namespace: NamespaceId) -> Option<usize> {
        self.knows(namespace).then(|| {
            self.records
                .keys()
                .filter(|(owner, _)| *owner == namespace)
                .count()
        })
    }

    fn namespaces_with_sentinel(&self) -> Vec<NamespaceId> {
        self.descriptors
            .keys()
            .copied()
            .chain([ns(SENTINEL)])
            .collect()
    }
}

/// Published history, tagged with the block each entry became visible in.
#[derive(Default)]
struct Log {
    mutations: Vec<(u64, NamespaceId, Mutation)>,
    descriptors: Vec<(u64, NamespaceId, OwnerId, Revision)>,
    creates: Vec<(u64, NamespaceId, OwnerId)>,
    /// The schedule in force for blocks from this one on.
    schedules: Vec<(u64, Weights)>,
}

impl Log {
    fn view_at(&self, block: u64) -> View {
        let mut view = View::default();
        for &(at, namespace, owner, revision) in &self.descriptors {
            if at <= block {
                view.descriptors.insert(namespace, (owner, revision));
            }
        }
        for (at, namespace, mutation) in &self.mutations {
            if *at <= block {
                view.apply(*namespace, mutation);
            }
        }
        view
    }

    fn schedule_at(&self, block: u64) -> Weights {
        self.schedules
            .iter()
            .rev()
            .find(|(at, _)| *at <= block)
            .map(|(_, weights)| *weights)
            .expect("the genesis schedule is always recorded")
    }
}

struct Oracle {
    log: Log,
    /// The log projected to the head, recomputed at every commit.
    head: View,
}

impl Oracle {
    fn new() -> Self {
        let mut log = Log::default();
        log.schedules.push((0, FIXTURE));
        Self {
            log,
            head: View::default(),
        }
    }

    /// Publishes the candidate's pending entries under `block` and returns the
    /// namespaces whose records it mutated.
    fn commit(&mut self, block: u64, pending: Pending) -> Vec<NamespaceId> {
        let mut touched: Vec<NamespaceId> = pending.mutations.iter().map(|(n, _)| *n).collect();
        touched.sort();
        touched.dedup();
        for (namespace, creator) in pending.creates {
            self.log.creates.push((block, namespace, creator));
        }
        for (namespace, owner, revision) in pending.descriptors {
            self.log
                .descriptors
                .push((block, namespace, owner, revision));
        }
        for (namespace, mutation) in pending.mutations {
            self.log.mutations.push((block, namespace, mutation));
        }
        self.head = self.log.view_at(u64::MAX);
        touched
    }
}

#[derive(Clone, Copy)]
struct Mark {
    mutations: usize,
    descriptors: usize,
    creates: usize,
    receipts: usize,
}

/// The open candidate as the oracle sees it.
struct Pending {
    weights: Weights,
    mutations: Vec<(NamespaceId, Mutation)>,
    descriptors: Vec<(NamespaceId, OwnerId, Revision)>,
    creates: Vec<(NamespaceId, OwnerId)>,
    /// Owner and revision of every namespace visible to the candidate.
    current: BTreeMap<NamespaceId, (OwnerId, Revision)>,
    next_namespace: u64,
    block_remaining: u64,
    marks: Vec<Mark>,
    /// Receipts the branch must have recorded, in order.
    receipts: Vec<Receipt>,
}

impl Pending {
    fn open(oracle: &Oracle, weights: Weights, block_budget: Option<u64>) -> Self {
        Self {
            weights,
            mutations: Vec::new(),
            descriptors: Vec::new(),
            creates: Vec::new(),
            current: oracle.head.descriptors.clone(),
            next_namespace: 1 + count(oracle.log.creates.len()),
            block_remaining: block_budget.unwrap_or(u64::MAX),
            marks: Vec::new(),
            receipts: Vec::new(),
        }
    }

    fn known(&self) -> Vec<NamespaceId> {
        self.current.keys().copied().collect()
    }

    fn spent(&self) -> u64 {
        self.receipts
            .iter()
            .map(|receipt| receipt.spent.units())
            .sum()
    }

    fn checkpoint(&mut self) {
        self.marks.push(Mark {
            mutations: self.mutations.len(),
            descriptors: self.descriptors.len(),
            creates: self.creates.len(),
            receipts: self.receipts.len(),
        });
    }

    /// Truncates the pending lists to the mark and recomputes the owner map
    /// and the allocator from what is left. Returns the discarded receipt
    /// range and the number of creations discarded.
    fn rollback(&mut self, oracle: &Oracle, index: usize) -> (Range<usize>, usize) {
        let mark = self.marks[index];
        let discarded_creates = self.creates.len() - mark.creates;
        self.mutations.truncate(mark.mutations);
        self.descriptors.truncate(mark.descriptors);
        self.creates.truncate(mark.creates);
        self.marks.truncate(index + 1);
        self.current = oracle.head.descriptors.clone();
        for &(namespace, owner, revision) in &self.descriptors {
            self.current.insert(namespace, (owner, revision));
        }
        self.next_namespace = 1 + count(oracle.log.creates.len() + self.creates.len());
        (mark.receipts..self.receipts.len(), discarded_creates)
    }

    fn view(&self, head: &View) -> View {
        let mut view = head.clone();
        for &(namespace, owner, revision) in &self.descriptors {
            view.descriptors.insert(namespace, (owner, revision));
        }
        for (namespace, mutation) in &self.mutations {
            view.apply(*namespace, mutation);
        }
        view
    }

    fn value(&self, head: &View, namespace: NamespaceId, key: &[u8]) -> Option<Vec<u8>> {
        for (candidate, mutation) in self.mutations.iter().rev() {
            if *candidate == namespace && mutation.key() == key {
                return mutation.value().map(<[u8]>::to_vec);
            }
        }
        head.value(namespace, key).cloned()
    }

    fn authorize(&self, namespace: NamespaceId, actor: OwnerId) -> Option<Rejection> {
        match self.current.get(&namespace) {
            None => Some(Rejection::UnknownNamespace),
            Some((owner, _)) if *owner != actor => Some(Rejection::Unauthorized),
            Some(_) => None,
        }
    }

    fn apply(&mut self, actor: OwnerId, op: &WriteOp) {
        match op {
            WriteOp::CreateNamespace { .. } => {
                let id = ns(self.next_namespace);
                self.creates.push((id, actor));
                self.descriptors.push((id, actor, Revision::INITIAL));
                self.current.insert(id, (actor, Revision::INITIAL));
                self.next_namespace += 1;
            }
            WriteOp::TransferNamespace {
                namespace,
                new_owner,
                ..
            } => {
                let revision = Revision(self.current[namespace].1.0 + 1);
                self.descriptors.push((*namespace, *new_owner, revision));
                self.current.insert(*namespace, (*new_owner, revision));
            }
            WriteOp::Put {
                namespace,
                key,
                value,
            } => self
                .mutations
                .push((*namespace, Mutation::Put(key.clone(), value.clone()))),
            WriteOp::Delete { namespace, key } => self
                .mutations
                .push((*namespace, Mutation::Delete(key.clone()))),
            WriteOp::CreateRecord { .. }
            | WriteOp::PatchRecord { .. }
            | WriteOp::DeleteRecord { .. } => {
                unreachable!("the milestone-1 oracle issues no typed record operations")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expected {
    /// Admission failure: nothing charged, nothing recorded.
    NotAdmitted { remaining: u64 },
    /// A recorded receipt; `effect` is present exactly when the op applied.
    Recorded {
        outcome: Outcome,
        spent: u64,
        effect: Option<Effect>,
    },
}

fn expect(
    pending: &Pending,
    head: &View,
    actor: OwnerId,
    op: &WriteOp,
    budget: Budget,
) -> Expected {
    let limit = budget.limit_units().map(Cost::units);
    if let Some(limit) = limit
        && limit > pending.block_remaining
    {
        return Expected::NotAdmitted {
            remaining: pending.block_remaining,
        };
    }
    let limit = limit.unwrap_or(pending.block_remaining);
    let (semantic, effect) = match op {
        WriteOp::CreateNamespace { .. } => {
            (None, Effect::NamespaceCreated(ns(pending.next_namespace)))
        }
        WriteOp::TransferNamespace {
            namespace,
            expected_revision,
            ..
        } => {
            let (semantic, current) = match pending.current.get(namespace) {
                None => (Some(Rejection::UnknownNamespace), Revision::INITIAL),
                Some((owner, current)) if *owner != actor => {
                    (Some(Rejection::Unauthorized), *current)
                }
                Some((_, current)) if current != expected_revision => (
                    Some(Rejection::StaleRevision {
                        expected: *expected_revision,
                        current: *current,
                    }),
                    *current,
                ),
                Some((_, current)) => (None, *current),
            };
            (
                semantic,
                Effect::Transferred {
                    revision: Revision(current.0 + 1),
                },
            )
        }
        WriteOp::Put { namespace, key, .. } => (
            pending.authorize(*namespace, actor),
            Effect::Put {
                replaced: pending.value(head, *namespace, key).is_some(),
            },
        ),
        WriteOp::Delete { namespace, key } => (
            pending.authorize(*namespace, actor),
            Effect::Deleted {
                existed: pending.value(head, *namespace, key).is_some(),
            },
        ),
        WriteOp::CreateRecord { .. }
        | WriteOp::PatchRecord { .. }
        | WriteOp::DeleteRecord { .. } => {
            unreachable!("the milestone-1 oracle issues no typed record operations")
        }
    };
    let (outcome, spent) = verdict(&charges_for(&pending.weights, op), semantic, limit);
    Expected::Recorded {
        outcome,
        spent,
        effect: (outcome == Outcome::Applied).then_some(effect),
    }
}

// ---------------------------------------------------------------------------
// Comparable shapes, transcript and coverage.
// ---------------------------------------------------------------------------

type Flat<T> = Result<(T, Receipt), (Rejection, Receipt)>;

fn flat_write(result: &WriteResult) -> (Flat<Effect>, PhysicalTrace) {
    match result {
        Ok(executed) => (
            Ok((executed.effect, executed.receipt.clone())),
            executed.physical,
        ),
        Err(rejected) => (
            Err((rejected.reason, rejected.receipt.clone())),
            rejected.physical,
        ),
    }
}

fn flat_read<T: Clone>(result: &ReadResult<T>) -> (Flat<T>, PhysicalTrace) {
    match result {
        Ok(metered) => (
            Ok((metered.value.clone(), metered.receipt.clone())),
            metered.physical,
        ),
        Err(rejected) => (
            Err((rejected.reason, rejected.receipt.clone())),
            rejected.physical,
        ),
    }
}

fn expected_flat(expected: &Expected, schedule_id: u32) -> Flat<Effect> {
    match expected {
        Expected::NotAdmitted { remaining } => {
            let reason = Rejection::ExceedsBlockBudget {
                remaining: Cost::new(*remaining),
            };
            Err((
                reason,
                receipt(
                    ReceiptKind::Write,
                    schedule_id,
                    0,
                    Outcome::Rejected(reason),
                ),
            ))
        }
        Expected::Recorded {
            outcome: Outcome::Applied,
            spent,
            effect: Some(effect),
        } => Ok((
            *effect,
            receipt(ReceiptKind::Write, schedule_id, *spent, Outcome::Applied),
        )),
        Expected::Recorded {
            outcome: Outcome::Rejected(reason),
            spent,
            effect: None,
        } => Err((
            *reason,
            receipt(
                ReceiptKind::Write,
                schedule_id,
                *spent,
                Outcome::Rejected(*reason),
            ),
        )),
        other => unreachable!("inconsistent expectation {other:?}"),
    }
}

/// Expected read result for `value` under `charges`, with the semantic check
/// after the first charge and `limit` as the operation budget.
fn expected_read<T>(
    value: T,
    charges: &[u64],
    semantic: Option<Rejection>,
    limit: u64,
    schedule_id: u32,
) -> Flat<T> {
    let (outcome, spent) = verdict(charges, semantic, limit);
    let receipt = receipt(ReceiptKind::Read, schedule_id, spent, outcome);
    match outcome {
        Outcome::Applied => Ok((value, receipt)),
        Outcome::Rejected(reason) => Err((reason, receipt)),
    }
}

fn verdict_name(flat: &Flat<Effect>) -> &'static str {
    match flat {
        Ok(_) => "Applied",
        Err((Rejection::UnknownNamespace, _)) => "UnknownNamespace",
        Err((Rejection::Unauthorized, _)) => "Unauthorized",
        Err((Rejection::StaleRevision { .. }, _)) => "StaleRevision",
        Err((Rejection::ExceedsBlockBudget { .. }, _)) => "ExceedsBlockBudget",
        Err((Rejection::OutOfBudget, _)) => "OutOfBudget",
        Err((Rejection::CostOverflow, _)) => "CostOverflow",
        Err((Rejection::DirectoryFull, _)) => "DirectoryFull",
        Err((Rejection::CounterExhausted, _)) => "CounterExhausted",
        Err((Rejection::AlreadyExists, _)) => "AlreadyExists",
        Err((Rejection::NotFound, _)) => "NotFound",
        Err((Rejection::RecordFull, _)) => "RecordFull",
        Err((Rejection::InvalidArgument(_), _)) => "InvalidArgument",
        Err((Rejection::InvalidQuery(_), _)) => "InvalidQuery",
        Err((Rejection::CursorMismatch, _)) => "CursorMismatch",
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Published(SnapshotId),
    Write(Flat<Effect>),
    Rollback(Range<usize>),
    StaleHead(EngineError),
    Read(Receipt),
}

#[derive(Debug, Default)]
struct Transcript {
    events: Vec<Event>,
    /// Non-canonical traces, compared separately from the events.
    physical: Vec<PhysicalTrace>,
}

#[derive(Debug, Default)]
struct Coverage {
    verdicts: BTreeMap<&'static str, usize>,
    rollbacks: usize,
    discarded_creates: usize,
    deleted_later: usize,
}

struct Ctx {
    seed: u32,
    block: u64,
    phase: &'static str,
}

impl fmt::Display for Ctx {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "seed={:#010x} block={} phase={}",
            self.seed, self.block, self.phase
        )
    }
}

struct Retained {
    block: u64,
    id: SnapshotId,
    handle: Snapshot,
    namespaces: Vec<NamespaceId>,
    receipts: Vec<Receipt>,
}

// ---------------------------------------------------------------------------
// Metered reads on branches and snapshots, checked against a view.
// ---------------------------------------------------------------------------

trait Reader {
    fn get_metered(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> ReadResult<Option<Vec<u8>>>;
    fn scan_metered(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> ReadResult<Vec<Entry>>;
    fn descriptor_metered(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> ReadResult<NamespaceDescriptor>;
}

impl Reader for Branch {
    fn get_metered(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> ReadResult<Option<Vec<u8>>> {
        Branch::get(self, namespace, key, budget)
    }

    fn scan_metered(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> ReadResult<Vec<Entry>> {
        Branch::scan_prefix(self, namespace, prefix, budget)
    }

    fn descriptor_metered(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> ReadResult<NamespaceDescriptor> {
        Branch::descriptor(self, namespace, budget)
    }
}

impl Reader for Snapshot {
    fn get_metered(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> ReadResult<Option<Vec<u8>>> {
        Snapshot::get(self, namespace, key, budget)
    }

    fn scan_metered(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> ReadResult<Vec<Entry>> {
        Snapshot::scan_prefix(self, namespace, prefix, budget)
    }

    fn descriptor_metered(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> ReadResult<NamespaceDescriptor> {
        Snapshot::descriptor(self, namespace, budget)
    }
}

fn check_descriptor<R: Reader>(
    reader: &R,
    view: &View,
    weights: &Weights,
    namespace: NamespaceId,
    ctx: &Ctx,
) -> Receipt {
    let unknown = (!view.knows(namespace)).then_some(Rejection::UnknownNamespace);
    let want = expected_read(
        view.descriptor(namespace),
        &weights.descriptor_charges(),
        unknown,
        u64::MAX,
        weights.id,
    );
    let (got, physical) = flat_read(&reader.descriptor_metered(namespace, Budget::unlimited()));
    let got = got.map(|(value, receipt)| (Some(value), receipt));
    assert_eq!(got, want, "{ctx} descriptor ns={namespace}");
    assert!(
        physical.directory_nodes <= DIRECTORY_READ_BOUND,
        "{ctx} descriptor ns={namespace}: {physical:?} exceeds the directory path bound"
    );
    let (_, receipt) = want.unwrap_or_else(|(_, receipt)| (None, receipt));
    receipt
}

/// Checks every key, every prefix and the descriptor of each namespace and
/// returns the read receipts in order.
fn check_reads<R: Reader>(
    reader: &R,
    view: &View,
    weights: &Weights,
    namespaces: &[NamespaceId],
    prefixes: &[Vec<u8>],
    ctx: &Ctx,
) -> Vec<Receipt> {
    let mut receipts = Vec::new();
    let bound = DIRECTORY_READ_BOUND;
    for &namespace in namespaces {
        receipts.push(check_descriptor(reader, view, weights, namespace, ctx));
        let unknown = (!view.knows(namespace)).then_some(Rejection::UnknownNamespace);

        for key in KEYS.iter().copied().chain([b"absent".as_slice()]) {
            let value = view.value(namespace, key).cloned();
            let charges = weights.get_charges(value.as_deref().map(len));
            let want = expected_read(value, &charges, unknown, u64::MAX, weights.id);
            let (got, physical) =
                flat_read(&reader.get_metered(namespace, key, Budget::unlimited()));
            assert_eq!(got, want, "{ctx} get ns={namespace} key={key:?}");
            assert!(
                physical.directory_nodes <= bound,
                "{ctx} get ns={namespace} key={key:?}: {physical:?} exceeds the path bound"
            );
            receipts.push(receipt_of(&got));
        }

        for prefix in prefixes {
            let entries = view.entries(namespace, prefix);
            let charges = weights.scan_charges(&entries);
            let want = expected_read(entries, &charges, unknown, u64::MAX, weights.id);
            let (got, physical) =
                flat_read(&reader.scan_metered(namespace, prefix, Budget::unlimited()));
            assert_eq!(got, want, "{ctx} scan ns={namespace} prefix={prefix:?}");
            assert!(
                physical.directory_nodes <= bound,
                "{ctx} scan ns={namespace} prefix={prefix:?}: {physical:?} exceeds the path bound"
            );
            receipts.push(receipt_of(&got));
        }

        // Limited whole-namespace scans: the exact total applies; one unit
        // less stops at the last charge point that still fits, with no
        // partial result.
        let entries = view.entries(namespace, b"");
        let charges = weights.scan_charges(&entries);
        let exact = total(&charges);
        for limit in [exact, exact.saturating_sub(1)] {
            let want = expected_read(entries.clone(), &charges, unknown, limit, weights.id);
            let (got, _) = flat_read(&reader.scan_metered(namespace, b"", Budget::limit(limit)));
            assert_eq!(
                got, want,
                "{ctx} limited scan ns={namespace} limit={limit} exact={exact}"
            );
            receipts.push(receipt_of(&got));
        }
    }
    receipts
}

fn receipt_of<T>(flat: &Flat<T>) -> Receipt {
    match flat {
        Ok((_, receipt)) | Err((_, receipt)) => receipt.clone(),
    }
}

fn check_descriptors<R: Reader>(reader: &R, view: &View, weights: &Weights, ctx: &Ctx) {
    for namespace in view.namespaces_with_sentinel() {
        check_descriptor(reader, view, weights, namespace, ctx);
    }
}

fn prefix_cases() -> Vec<Vec<u8>> {
    let mut prefixes = vec![b"absent".to_vec(), b"\xff\xff\xff".to_vec()];
    for key in KEYS {
        for length in 0..=key.len() {
            prefixes.push(key[..length].to_vec());
        }
        let mut longer = key.to_vec();
        longer.extend_from_slice(b"\x55\x55");
        prefixes.push(longer);
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

/// The prefixes checked after every single operation (the full set runs at
/// block boundaries and in the history pass).
fn short_prefixes() -> Vec<Vec<u8>> {
    [b"".as_slice(), b"a", b"a/", b"\0", b"\xff", b"absent"]
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect()
}

// ---------------------------------------------------------------------------
// Workload generation.
// ---------------------------------------------------------------------------

struct Generator(u32);

impl Generator {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn index(&mut self, length: usize) -> usize {
        self.next() as usize % length
    }

    fn value(&mut self) -> Vec<u8> {
        let lengths = [0, 1, 2, 7, 32, 257];
        let length = lengths[self.index(lengths.len())];
        (0..length).map(|_| self.next() as u8).collect()
    }

    fn owner(&mut self) -> OwnerId {
        OWNERS[self.index(OWNERS.len())]
    }
}

fn block_size(block: u64) -> usize {
    if block == 1 {
        2
    } else {
        SIZES[usize::try_from((block - 2) % 6).expect("small")]
    }
}

/// Block write budgets: unlimited every fifth block, tight every third,
/// otherwise roomy. Tight blocks run dry and exercise admission.
fn block_budget(block: u64, size: usize) -> Option<u64> {
    let size = count(size);
    if block.is_multiple_of(5) {
        None
    } else if block % 3 == 1 {
        Some(size * 30 + 100)
    } else {
        Some(size * 60 + 120)
    }
}

/// Namespaces are created over time: two in block 1, one more every 24 blocks.
fn creation_cap(block: u64) -> usize {
    2 + usize::try_from(block / 24).expect("small")
}

fn put(namespace: NamespaceId, key: &[u8], value: Vec<u8>) -> WriteOp {
    WriteOp::Put {
        namespace,
        key: key.to_vec(),
        value,
    }
}

fn delete(namespace: NamespaceId, key: &[u8]) -> WriteOp {
    WriteOp::Delete {
        namespace,
        key: key.to_vec(),
    }
}

fn plan_op(
    generator: &mut Generator,
    block: u64,
    index: usize,
    focus: &[u8],
    other: &[u8],
    pending: &Pending,
    doomed: bool,
) -> (OwnerId, WriteOp, Budget) {
    let weights = pending.weights;
    if block == 1 {
        // Block 1 seeds the history: one namespace for alice, one for bob.
        let cost = total(&weights.create_charges());
        return (
            OWNERS[index],
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(cost),
        );
    }
    let known = pending.known();
    let namespace = if generator.index(16) == 0 {
        // Never allocated at this point: at or past the allocator, which
        // also names the ordinal a just-rolled-back creation released.
        ns(pending.next_namespace + count(generator.index(2)))
    } else {
        known[generator.index(known.len())]
    };
    let current = pending.current.get(&namespace).copied();
    let actor = match current {
        Some((owner, _)) if generator.index(5) != 0 => owner,
        _ => generator.owner(),
    };
    let op = match index % 8 {
        // Force replace, delete and recreate of the focus keys within a block.
        0 | 3 | 4 => put(namespace, focus, generator.value()),
        1 => put(namespace, other, generator.value()),
        2 => delete(namespace, focus),
        5 => delete(namespace, other),
        6 => {
            let revision = current.map_or(Revision::INITIAL, |(_, revision)| revision);
            let expected_revision = if generator.index(4) == 0 {
                Revision(revision.0 + 1)
            } else {
                revision
            };
            WriteOp::TransferNamespace {
                namespace,
                expected_revision,
                new_owner: generator.owner(),
            }
        }
        // Creations inside a doomed transaction never persist, so they are
        // free of the cap.
        _ if doomed || known.len() < creation_cap(block) => WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
        _ => {
            let key = KEYS[generator.index(KEYS.len())];
            if generator.index(3) == 0 {
                delete(namespace, key)
            } else {
                put(namespace, key, generator.value())
            }
        }
    };
    let actor = if matches!(op, WriteOp::CreateNamespace { .. }) {
        generator.owner()
    } else {
        actor
    };
    let cost = total(&charges_for(&weights, &op));
    let budget = match generator.index(8) {
        0..=3 => Budget::limit(cost),
        4 => Budget::limit(cost.saturating_sub(1)),
        5 => Budget::unlimited(),
        6 => Budget::limit(0),
        _ => Budget::limit(cost + 5),
    };
    (actor, op, budget)
}

fn op_namespace(op: &WriteOp) -> Option<NamespaceId> {
    match op {
        WriteOp::CreateNamespace { .. } => None,
        WriteOp::TransferNamespace { namespace, .. }
        | WriteOp::Put { namespace, .. }
        | WriteOp::Delete { namespace, .. }
        | WriteOp::CreateRecord { namespace, .. }
        | WriteOp::PatchRecord { namespace, .. }
        | WriteOp::DeleteRecord { namespace, .. } => Some(*namespace),
    }
}

// ---------------------------------------------------------------------------
// Checked execution.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_checked(
    branch: &mut Branch,
    pending: &mut Pending,
    oracle: &Oracle,
    actor: OwnerId,
    op: WriteOp,
    budget: Budget,
    ctx: &Ctx,
    transcript: &mut Transcript,
    coverage: &mut Coverage,
) {
    let expected = expect(pending, &oracle.head, actor, &op, budget);
    let want = expected_flat(&expected, pending.weights.id);
    let result = branch.execute(actor, op.clone(), budget);
    let (got, physical) = flat_write(&result);
    assert_eq!(
        got,
        want,
        "{ctx} actor={} op={op:?} budget={budget:?} block_remaining={}",
        owner_name(actor),
        pending.block_remaining
    );
    if let Expected::Recorded { outcome, spent, .. } = expected {
        pending.block_remaining -= spent;
        pending.receipts.push(receipt(
            ReceiptKind::Write,
            pending.weights.id,
            spent,
            outcome,
        ));
        if outcome == Outcome::Applied {
            pending.apply(actor, &op);
        }
    }
    assert_eq!(
        branch.spent(),
        Cost::new(pending.spent()),
        "{ctx} after op={op:?}"
    );
    assert_eq!(
        branch.remaining(),
        Cost::new(pending.block_remaining),
        "{ctx} after op={op:?}"
    );
    assert_eq!(
        branch.receipts().len(),
        pending.receipts.len(),
        "{ctx} after op={op:?}: admission rejections are never recorded"
    );
    // Physical rules stated in the decisions file.
    if matches!(op, WriteOp::TransferNamespace { .. }) {
        assert_eq!(
            physical.record_nodes, 0,
            "{ctx} op={op:?}: a transfer visits no record nodes"
        );
    }
    assert!(
        physical.directory_nodes <= DIRECTORY_WRITE_BOUND,
        "{ctx} op={op:?}: {physical:?} exceeds the directory write bound {DIRECTORY_WRITE_BOUND}"
    );
    *coverage.verdicts.entry(verdict_name(&got)).or_default() += 1;
    transcript.events.push(Event::Write(got));
    transcript.physical.push(physical);
}

#[allow(clippy::too_many_arguments)]
fn rollback_checked(
    branch: &mut Branch,
    pending: &mut Pending,
    oracle: &Oracle,
    checkpoints: &[Checkpoint],
    index: usize,
    ctx: &Ctx,
    transcript: &mut Transcript,
    coverage: &mut Coverage,
) {
    let (discarded, creates) = pending.rollback(oracle, index);
    let got = branch.rollback_to(checkpoints[index]);
    assert_eq!(
        got,
        Ok(discarded.clone()),
        "{ctx} rollback to checkpoint {index}: discarded receipt range"
    );
    assert_eq!(
        branch.receipts(),
        pending.receipts.as_slice(),
        "{ctx} rollback to checkpoint {index}: receipts are never withdrawn"
    );
    assert_eq!(
        branch.spent(),
        Cost::new(pending.spent()),
        "{ctx} rollback to checkpoint {index}"
    );
    assert_eq!(
        branch.remaining(),
        Cost::new(pending.block_remaining),
        "{ctx} rollback to checkpoint {index}"
    );
    assert_eq!(
        branch.namespace_count(),
        pending.current.len(),
        "{ctx} rollback to checkpoint {index}: allocator and catalog restored"
    );
    coverage.rollbacks += 1;
    coverage.discarded_creates += creates;
    transcript.events.push(Event::Rollback(discarded));
}

// ---------------------------------------------------------------------------
// Admin candidates, stale losers and the per-seed run.
// ---------------------------------------------------------------------------

/// Publishes a pricing change as its own snapshot before the block's data
/// candidate. The block maps to the data snapshot only.
fn publish_admin(
    engine: &mut Engine,
    oracle: &mut Oracle,
    block: u64,
    next_snapshot: &mut u64,
    transcript: &mut Transcript,
    prefixes: &[Vec<u8>],
    seed: u32,
) {
    let ctx = Ctx {
        seed,
        block,
        phase: "admin",
    };
    let n = u32::try_from(block / ADMIN_EVERY).expect("small");
    // Distinct, increasing ids: 2, 3, ... after the fixture's 1.
    let id = n + 1;
    let current = oracle.log.schedule_at(block).id;
    let (schedule, weights) = admin_schedule(n, id);
    let mut admin = engine.begin(engine.head(), None).unwrap();
    assert_eq!(admin.pricing().id(), current, "{ctx}: pinned to the base");
    assert_eq!(
        admin.set_pricing(PriceSchedule::uniform(current, 9)),
        Err(EngineError::StaleSchedule { current }),
        "{ctx}: a schedule id must exceed the committed one"
    );
    // A pricing change inside a rolled-back span does not publish: the
    // second set_pricing succeeds only if the rollback restored the base id.
    let checkpoint = admin.checkpoint();
    admin
        .set_pricing(schedule.clone().with_id(id + 100))
        .unwrap();
    assert_eq!(admin.rollback_to(checkpoint), Ok(0..0), "{ctx}");
    admin
        .set_pricing(schedule.clone())
        .unwrap_or_else(|error| panic!("{ctx}: rolled-back pricing leaked: {error}"));
    assert_eq!(
        admin.pricing().id(),
        current,
        "{ctx}: pinned pricing never changes"
    );
    let sealed = admin.seal();
    assert!(sealed.receipts().is_empty(), "{ctx}");
    assert_eq!(sealed.spent(), Cost::ZERO, "{ctx}");
    let published = engine.publish(&sealed).unwrap();
    assert_eq!(
        published.sequence(),
        *next_snapshot,
        "{ctx}: snapshot ids are sequential"
    );
    *next_snapshot += 1;
    transcript.events.push(Event::Published(published));
    let head = engine.head_snapshot();
    assert_eq!(head.id(), published, "{ctx}");
    assert_eq!(
        head.pricing(),
        &schedule,
        "{ctx}: the admin snapshot commits the schedule"
    );
    oracle.log.schedules.push((block, weights));
    // The admin snapshot carries the previous block's data, priced with the
    // new schedule.
    let namespaces = oracle.head.namespaces_with_sentinel();
    check_reads(&head, &oracle.head, &weights, &namespaces, prefixes, &ctx);
}

/// Builds a competing candidate on the current head that touches two
/// existing namespaces, transfers one and creates a third; it is sealed here
/// and loses once the real block publishes.
#[allow(clippy::too_many_arguments)]
fn build_loser(
    engine: &Engine,
    oracle: &Oracle,
    generator: &mut Generator,
    weights: Weights,
    seed: u32,
    block: u64,
    thorough: bool,
    short_prefixes: &[Vec<u8>],
    transcript: &mut Transcript,
    coverage: &mut Coverage,
) -> SealedCandidate {
    let ctx = Ctx {
        seed,
        block,
        phase: "loser",
    };
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let mut pending = Pending::open(oracle, weights, None);
    let known = pending.known();
    let first = known[0];
    let second = known[known.len() - 1];
    assert_ne!(first, second, "{ctx}: the loser needs two namespaces");
    let (first_owner, _) = pending.current[&first];
    let (second_owner, second_revision) = pending.current[&second];
    let created = ns(pending.next_namespace);
    let new_owner = generator.owner();
    let key = KEYS[generator.index(KEYS.len())].to_vec();
    let value = generator.value();
    let steps = [
        (
            CAROL,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(total(&weights.create_charges())),
        ),
        (
            first_owner,
            put(first, &key, value.clone()),
            Budget::unlimited(),
        ),
        (
            second_owner,
            WriteOp::TransferNamespace {
                namespace: second,
                expected_revision: second_revision,
                new_owner,
            },
            Budget::limit(total(&weights.transfer_charges())),
        ),
        (
            CAROL,
            put(created, &key, value.clone()),
            Budget::unlimited(),
        ),
        (first_owner, delete(first, &key), Budget::unlimited()),
        // The revision advanced above, so this one is stale.
        (
            new_owner,
            WriteOp::TransferNamespace {
                namespace: second,
                expected_revision: second_revision,
                new_owner: second_owner,
            },
            Budget::unlimited(),
        ),
        // Unauthorized unless the transfer happened to keep the owner.
        (second_owner, put(second, &key, value), Budget::unlimited()),
    ];
    for (actor, op, budget) in steps {
        execute_checked(
            &mut branch,
            &mut pending,
            oracle,
            actor,
            op,
            budget,
            &ctx,
            transcript,
            coverage,
        );
    }
    assert_eq!(
        pending.creates,
        vec![(created, CAROL)],
        "{ctx}: the loser's creation takes the next published ordinal"
    );
    if thorough {
        let view = pending.view(&oracle.head);
        check_reads(
            &branch,
            &view,
            &weights,
            &[first, second, created, ns(SENTINEL)],
            short_prefixes,
            &ctx,
        );
    }
    branch.seal()
}

struct Run {
    transcript: Transcript,
    coverage: Coverage,
}

/// Runs one seed on a fresh engine. `thorough` adds the per-operation branch
/// checks and the final history pass; the transcript is the same either way.
fn run_seed(seed: u32, thorough: bool) -> Run {
    let prefixes = prefix_cases();
    let short_prefixes = short_prefixes();
    let mut generator = Generator(seed);
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut blocks = BlockMap::new();
    let mut oracle = Oracle::new();
    let mut transcript = Transcript::default();
    let mut coverage = Coverage::default();
    let mut retained = Vec::new();
    let mut next_snapshot = 1u64;

    assert_eq!(engine.head(), SnapshotId::GENESIS);
    blocks.record(BlockNumber(0), engine.head()).unwrap();
    {
        let ctx = Ctx {
            seed,
            block: 0,
            phase: "genesis",
        };
        let genesis = blocks.at(&engine, BlockNumber(0)).unwrap();
        let namespaces = oracle.head.namespaces_with_sentinel();
        let receipts = check_reads(
            &genesis,
            &oracle.head,
            &FIXTURE,
            &namespaces,
            &prefixes,
            &ctx,
        );
        retained.push(Retained {
            block: 0,
            id: genesis.id(),
            handle: genesis,
            namespaces,
            receipts,
        });
    }

    for block in 1..=BLOCKS {
        if block % ADMIN_EVERY == 0 {
            publish_admin(
                &mut engine,
                &mut oracle,
                block,
                &mut next_snapshot,
                &mut transcript,
                &prefixes,
                seed,
            );
        }
        let weights = oracle.log.schedule_at(block);
        let size = block_size(block);
        let budget = block_budget(block, size);
        let base = engine.head();
        let mut branch = engine.begin(base, budget.map(Cost::new)).unwrap();
        let ctx = Ctx {
            seed,
            block,
            phase: "execute",
        };
        assert_eq!(branch.base(), base, "{ctx}");
        assert_eq!(
            branch.pricing().id(),
            weights.id,
            "{ctx}: writes are priced with the schedule committed at the base"
        );
        assert_eq!(
            branch.block_limit(),
            Cost::new(budget.unwrap_or(u64::MAX)),
            "{ctx}"
        );
        let mut pending = Pending::open(&oracle, weights, budget);
        let key_index = generator.index(KEYS.len());
        let focus = KEYS[key_index];
        let other = KEYS[(key_index + 1) % KEYS.len()];
        let transactional = block > 1;
        let mut checkpoints: Vec<Checkpoint> = Vec::new();
        let mut doomed = false;

        for index in 0..size {
            if transactional && index % TRANSACTION_OPS == 0 {
                checkpoints.push(branch.checkpoint());
                pending.checkpoint();
                doomed = generator.index(4) == 0;
            }
            let (actor, op, op_budget) =
                plan_op(&mut generator, block, index, focus, other, &pending, doomed);
            let target = op_namespace(&op).unwrap_or(ns(pending.next_namespace));
            execute_checked(
                &mut branch,
                &mut pending,
                &oracle,
                actor,
                op,
                op_budget,
                &ctx,
                &mut transcript,
                &mut coverage,
            );
            if thorough {
                let view = pending.view(&oracle.head);
                check_reads(&branch, &view, &weights, &[target], &short_prefixes, &ctx);
                check_descriptors(&branch, &view, &weights, &ctx);
                assert_eq!(branch.namespace_count(), view.descriptors.len(), "{ctx}");
            }
            let closes = index % TRANSACTION_OPS == TRANSACTION_OPS - 1 || index + 1 == size;
            if transactional && doomed && closes {
                let ctx = Ctx {
                    seed,
                    block,
                    phase: "rollback",
                };
                let last = checkpoints.len() - 1;
                rollback_checked(
                    &mut branch,
                    &mut pending,
                    &oracle,
                    &checkpoints,
                    last,
                    &ctx,
                    &mut transcript,
                    &mut coverage,
                );
                doomed = false;
                if thorough {
                    let view = pending.view(&oracle.head);
                    let all = view.namespaces_with_sentinel();
                    check_reads(&branch, &view, &weights, &all, &short_prefixes, &ctx);
                }
            }
        }

        // Occasionally unwind several transactions at once: checkpoints taken
        // after the target become invalid, the target itself stays usable.
        if transactional && checkpoints.len() >= 2 && generator.index(6) == 0 {
            let ctx = Ctx {
                seed,
                block,
                phase: "unwind",
            };
            let target = generator.index(checkpoints.len() - 1);
            let stale = checkpoints[checkpoints.len() - 1];
            rollback_checked(
                &mut branch,
                &mut pending,
                &oracle,
                &checkpoints,
                target,
                &ctx,
                &mut transcript,
                &mut coverage,
            );
            assert_eq!(
                branch.rollback_to(stale),
                Err(EngineError::InvalidCheckpoint),
                "{ctx}: a checkpoint taken after the rollback target is invalid"
            );
            // The target stays valid. The range is "receipts since the
            // checkpoint" (none of their effects survive), so repeating the
            // rollback reports the same range and changes nothing.
            let since = pending.marks[target].receipts..pending.receipts.len();
            assert_eq!(
                branch.rollback_to(checkpoints[target]),
                Ok(since),
                "{ctx}: the target stays valid after the rollback"
            );
            assert_eq!(branch.receipts(), pending.receipts.as_slice(), "{ctx}");
            assert_eq!(branch.namespace_count(), pending.current.len(), "{ctx}");
            checkpoints.truncate(target + 1);
            if thorough {
                let view = pending.view(&oracle.head);
                let all = view.namespaces_with_sentinel();
                check_reads(&branch, &view, &weights, &all, &short_prefixes, &ctx);
            }
        }

        // Seal: receipts and totals are the oracle's, whatever was rolled back.
        let ctx = Ctx {
            seed,
            block,
            phase: "seal",
        };
        assert_eq!(branch.receipts(), pending.receipts.as_slice(), "{ctx}");
        assert_eq!(branch.spent(), Cost::new(pending.spent()), "{ctx}");
        assert_eq!(branch.namespace_count(), pending.current.len(), "{ctx}");
        let sealed = branch.seal();
        assert_eq!(sealed.base(), base, "{ctx}");
        assert_eq!(sealed.receipts(), pending.receipts.as_slice(), "{ctx}");
        assert_eq!(sealed.spent(), Cost::new(pending.spent()), "{ctx}");

        // Staged: the head still shows only the published log.
        let ctx = Ctx {
            seed,
            block,
            phase: "staged",
        };
        let head = engine.head_snapshot();
        assert_eq!(head.id(), base, "{ctx}");
        let known = oracle.head.namespaces_with_sentinel();
        check_reads(&head, &oracle.head, &weights, &known, &prefixes, &ctx);

        let loser = (block % LOSER_EVERY == 0).then(|| {
            build_loser(
                &engine,
                &oracle,
                &mut generator,
                weights,
                seed,
                block,
                thorough,
                &short_prefixes,
                &mut transcript,
                &mut coverage,
            )
        });

        let ctx = Ctx {
            seed,
            block,
            phase: "publish",
        };
        let id = engine.publish(&sealed).unwrap();
        assert_eq!(
            id.sequence(),
            next_snapshot,
            "{ctx}: snapshot ids are sequential"
        );
        next_snapshot += 1;
        transcript.events.push(Event::Published(id));
        blocks.record(BlockNumber(block), id).unwrap();
        assert_eq!(
            blocks.record(BlockNumber(block), id),
            Err(HostError::NonConsecutiveBlock {
                block: BlockNumber(block),
                expected: BlockNumber(block + 1),
            }),
            "{ctx}: a block is recorded once"
        );
        assert_eq!(blocks.resolve(BlockNumber(block)), Some(id), "{ctx}");
        assert_eq!(blocks.latest(), Some((BlockNumber(block), id)), "{ctx}");
        assert_eq!(count(blocks.len()), block + 1, "{ctx}");
        let touched = oracle.commit(block, pending);

        if let Some(loser) = loser {
            let ctx = Ctx {
                seed,
                block,
                phase: "loser",
            };
            let error = engine
                .publish(&loser)
                .expect_err("a candidate whose base is no longer head cannot publish");
            assert_eq!(
                error,
                EngineError::StaleHead {
                    expected: base,
                    head: id
                },
                "{ctx}"
            );
            transcript.events.push(Event::StaleHead(error));
            assert_eq!(engine.head(), id, "{ctx}: a stale publish changes nothing");
        }
        assert_eq!(
            engine.begin(base, None).err(),
            Some(EngineError::StaleHead {
                expected: base,
                head: id
            }),
            "{ctx}: the old head is no longer a valid base"
        );
        assert_eq!(
            engine.snapshot_count(),
            usize::try_from(next_snapshot).expect("small"),
            "{ctx}"
        );

        // Applied: the new head is exactly the log.
        let ctx = Ctx {
            seed,
            block,
            phase: "applied",
        };
        let snapshot = blocks.at(&engine, BlockNumber(block)).unwrap();
        assert_eq!(snapshot.id(), id, "{ctx}");
        assert_eq!(snapshot.base(), Some(base), "{ctx}");
        assert_eq!(snapshot.pricing().id(), weights.id, "{ctx}");
        let namespaces = oracle.head.namespaces_with_sentinel();
        assert_eq!(snapshot.namespace_count(), namespaces.len() - 1, "{ctx}");
        for &namespace in &namespaces {
            assert_eq!(
                snapshot.record_count(namespace),
                oracle.head.record_count(namespace),
                "{ctx} ns={namespace}"
            );
        }
        let receipts = check_reads(
            &snapshot,
            &oracle.head,
            &weights,
            &namespaces,
            &prefixes,
            &ctx,
        );
        // Namespaces the block did not write share their record map with the
        // previous block; transfers and rolled-back writes copy nothing.
        let previous: &Retained = retained.last().expect("genesis is retained");
        for &namespace in &previous.namespaces {
            if namespace != ns(SENTINEL) && !touched.contains(&namespace) {
                assert!(
                    previous.handle.records_shared_with(&snapshot, namespace),
                    "{ctx} ns={namespace}: untouched records were copied"
                );
            }
        }
        transcript
            .events
            .extend(receipts.iter().cloned().map(Event::Read));
        retained.push(Retained {
            block,
            id,
            handle: snapshot,
            namespaces,
            receipts,
        });
    }

    if thorough {
        verify_history(
            &engine,
            &blocks,
            &oracle,
            &retained,
            &prefixes,
            seed,
            &mut coverage,
        );
    }
    Run {
        transcript,
        coverage,
    }
}

/// After all blocks: every published block, every namespace ever created,
/// every key and prefix, from the block map, from `engine.snapshot` and from
/// the handle retained at the boundary, must repeat the boundary's answers
/// and receipts.
fn verify_history(
    engine: &Engine,
    blocks: &BlockMap,
    oracle: &Oracle,
    retained: &[Retained],
    prefixes: &[Vec<u8>],
    seed: u32,
    coverage: &mut Coverage,
) {
    let all: Vec<NamespaceId> = oracle.log.creates.iter().map(|(_, id, _)| *id).collect();
    assert_eq!(
        all,
        (1..=count(all.len())).map(ns).collect::<Vec<_>>(),
        "seed={seed:#010x}: published ordinals are 1, 2, ... in publication order"
    );
    for entry in retained {
        let block = entry.block;
        let view = oracle.log.view_at(block);
        let weights = oracle.log.schedule_at(block);
        let ctx = Ctx {
            seed,
            block,
            phase: "history",
        };
        let snapshot = blocks.at(engine, BlockNumber(block)).unwrap();
        assert_eq!(snapshot.id(), entry.id, "{ctx}");
        assert_eq!(
            snapshot.pricing().id(),
            weights.id,
            "{ctx}: reads use the snapshot's schedule"
        );
        assert_eq!(snapshot.namespace_count(), view.descriptors.len(), "{ctx}");
        // Namespaces created after this block do not exist here, whatever
        // they hold at head.
        let later: Vec<NamespaceId> = all.iter().copied().filter(|id| !view.knows(*id)).collect();
        check_reads(&snapshot, &view, &weights, &later, prefixes, &ctx);
        let pinned = engine.snapshot(entry.id).unwrap();
        for (phase, handle) in [
            ("history/blocks.at", &snapshot),
            ("history/engine.snapshot", &pinned),
            ("history/retained", &entry.handle),
        ] {
            let ctx = Ctx { seed, block, phase };
            let receipts = check_reads(handle, &view, &weights, &entry.namespaces, prefixes, &ctx);
            assert_eq!(
                receipts.len(),
                entry.receipts.len(),
                "{ctx}: the boundary read the same namespaces"
            );
            for (index, (again, then)) in receipts.iter().zip(&entry.receipts).enumerate() {
                assert_eq!(
                    again, then,
                    "{ctx}: read receipt {index} differs from the boundary"
                );
            }
        }
        coverage.deleted_later += view
            .records
            .keys()
            .filter(|key| !oracle.head.records.contains_key(*key))
            .count();
    }
    assert_eq!(
        engine
            .snapshot(SnapshotId::GENESIS)
            .unwrap()
            .namespace_count(),
        0,
        "seed={seed:#010x}: genesis stays empty"
    );
    let unknown = ns(SENTINEL);
    assert!(
        matches!(
            blocks.at(engine, BlockNumber(BLOCKS + 1)),
            Err(HostError::UnknownBlock(BlockNumber(b))) if b == BLOCKS + 1
        ),
        "seed={seed:#010x}: unrecorded blocks do not resolve"
    );
    assert_eq!(
        engine.head_snapshot().record_count(unknown),
        None,
        "seed={seed:#010x}: the sentinel namespace never exists"
    );
}

fn full_run(seed: u32) {
    let run = run_seed(seed, true);
    let coverage = &run.coverage;
    for name in [
        "Applied",
        "UnknownNamespace",
        "Unauthorized",
        "StaleRevision",
        "ExceedsBlockBudget",
        "OutOfBudget",
    ] {
        assert!(
            coverage.verdicts.get(name).copied().unwrap_or(0) > 0,
            "seed={seed:#010x}: the workload never produced {name}: {coverage:?}"
        );
    }
    assert!(coverage.rollbacks > 0, "seed={seed:#010x}: {coverage:?}");
    assert!(
        coverage.discarded_creates > 0,
        "seed={seed:#010x}: no creation was ever rolled back: {coverage:?}"
    );
    assert!(
        coverage.deleted_later > 0,
        "seed={seed:#010x}: no historical read saw a key deleted later: {coverage:?}"
    );
}

fn assert_replay(seed: u32) {
    let first = run_seed(seed, false);
    let second = run_seed(seed, false);
    let (a, b) = (&first.transcript, &second.transcript);
    for (index, (x, y)) in a.events.iter().zip(&b.events).enumerate() {
        assert_eq!(
            x, y,
            "seed={seed:#010x}: transcript event {index} differs on replay"
        );
    }
    assert_eq!(
        a.events.len(),
        b.events.len(),
        "seed={seed:#010x}: transcript length"
    );
    for (index, (x, y)) in a.physical.iter().zip(&b.physical).enumerate() {
        assert_eq!(
            x, y,
            "seed={seed:#010x}: physical trace {index} differs on replay"
        );
    }
    assert_eq!(
        a.physical.len(),
        b.physical.len(),
        "seed={seed:#010x}: trace count"
    );
    assert!(
        a.events
            .iter()
            .any(|event| matches!(event, Event::StaleHead(_))),
        "seed={seed:#010x}: the transcript records the losers"
    );
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn charge_table_reproduces_the_decisions_file_totals() {
    let w = FIXTURE;
    // Totals quoted in the decisions file under FIXTURE (model version 3:
    // RootUpdate 2 per logical entry write).
    assert_eq!(total(&w.create_charges()), 53);
    assert_eq!(total(&w.transfer_charges()), 23);
    assert_eq!(total(&w.put_charges(3, 7)), 1 + 10 + 2 * 3 + 7 + 2 * 2);
    assert_eq!(total(&w.delete_charges(3)), 1 + 5 + 2 * 3 + 2 * 2);
    assert_eq!(total(&w.get_charges(Some(9))), 1 + 3 + 9);
    assert_eq!(total(&w.get_charges(None)), 4);
    let entries = vec![
        Entry {
            key: b"ab".to_vec(),
            value: b"xyz".to_vec(),
        },
        Entry {
            key: b"".to_vec(),
            value: b"".to_vec(),
        },
    ];
    assert_eq!(total(&w.scan_charges(&entries)), 5 + (2 + 5) + 2);
    assert_eq!(total(&w.descriptor_charges()), 1);
    // Verdict rules: exact budget applies; one less stops before the group;
    // semantic rejections cost the lookup; zero budget rejects at no cost.
    assert_eq!(
        verdict(&w.put_charges(3, 7), None, 28),
        (Outcome::Applied, 28)
    );
    assert_eq!(
        verdict(&w.put_charges(3, 7), None, 27),
        (Outcome::Rejected(Rejection::OutOfBudget), 1)
    );
    assert_eq!(
        verdict(&w.put_charges(3, 7), Some(Rejection::Unauthorized), 27),
        (Outcome::Rejected(Rejection::Unauthorized), 1)
    );
    assert_eq!(
        verdict(&w.put_charges(3, 7), Some(Rejection::Unauthorized), 0),
        (Outcome::Rejected(Rejection::OutOfBudget), 0)
    );
    assert_eq!(
        verdict(&w.scan_charges(&entries), None, 13),
        (Outcome::Rejected(Rejection::OutOfBudget), 12)
    );
}

macro_rules! seed_tests {
    ($($history:ident, $replay:ident => $seed:expr;)*) => {
        $(
            #[test]
            fn $history() {
                full_run($seed);
            }

            #[test]
            fn $replay() {
                assert_replay($seed);
            }
        )*
    };
}

seed_tests! {
    history_matches_oracle_seed_00000001, replay_is_deterministic_seed_00000001 => SEEDS[0];
    history_matches_oracle_seed_9e3779b9, replay_is_deterministic_seed_9e3779b9 => SEEDS[1];
    history_matches_oracle_seed_deadbeef, replay_is_deterministic_seed_deadbeef => SEEDS[2];
    history_matches_oracle_seed_a5a5a5a5, replay_is_deterministic_seed_a5a5a5a5 => SEEDS[3];
}
