//! Deterministic operation metering: classes, weights, budgets, receipts and
//! optional debug ledgers.
//!
//! Every data-plane operation counts abstract logical units before it does
//! the corresponding work. Counting is integer-only with checked arithmetic,
//! independent of elapsed time, cache state, allocation behaviour or the
//! physical layout of storage. The counting rules are versioned by
//! [`MODEL_VERSION`]; the unit prices come from a [`PriceSchedule`] whose
//! identity is recorded in every receipt.
//!
//! Only write receipts are converted to gas by Arkiv. Read receipts exist for
//! local RPC budgets or API billing and never enter consensus.

use std::fmt;

use crate::{namespace::Revision, query::QueryFault, record::ArgumentFault};

/// Version of the counting rules (which classes are charged, and when).
///
/// Changing any charge point or class definition requires a new version.
/// The rules cover raw byte operations, typed records, indexes and queries,
/// and prepay authenticated-state maintenance: every write adds `RootUpdate`
/// per logical map-entry write to the group it already charges, typed record
/// writes add `RowByte` for the stored row they rewrite, and
/// `ProofNode`/`ProofByte` are reserved for point proofs (not charged yet).
/// The value is an opaque identifier of this rule set.
pub const MODEL_VERSION: u32 = 3;

/// A number of abstract cost units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cost(u64);

impl Cost {
    /// Zero units.
    pub const ZERO: Self = Self(0);

    /// Wraps a unit count.
    pub const fn new(units: u64) -> Self {
        Self(units)
    }

    /// Returns the unit count.
    pub const fn units(self) -> u64 {
        self.0
    }

    /// Checked addition; `None` on overflow.
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(units) => Some(Self(units)),
            None => None,
        }
    }

    /// Saturating subtraction.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl fmt::Display for Cost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} units", self.0)
    }
}

/// A logical operation class with its own unit price.
///
/// Counting rules (model version 3), per operation. `c` = cells, `b` = name
/// plus encoded value bytes of the cells written, `a` = attribute cells,
/// `n` = the stored row after the write (key bytes plus name and encoded
/// value bytes of every cell it holds), `p` = projected bytes (key bytes if
/// projected plus name and encoded value bytes of every projected cell).
///
/// | Operation | Charges, in order, each before the work it covers |
/// | --- | --- |
/// | create namespace | `DirectoryLookup` 1, then one group: `NamespaceCreate` 1 + `RootUpdate` 1 |
/// | transfer namespace | `DirectoryLookup` 1, then one group: `NamespaceTransfer` 1 + `RootUpdate` 1 |
/// | put | `DirectoryLookup` 1, then one group: `PutRecord` 1 + `WriteKeyByte` per key byte + `WriteValueByte` per value byte + `RootUpdate` 2 |
/// | delete | `DirectoryLookup` 1, then one group: `DeleteRecord` 1 + `WriteKeyByte` per key byte + `RootUpdate` 2 (also when the key is missing) |
/// | get | `DirectoryLookup` 1, then `PointRead` 1, then (if present) `ReadByte` per value byte |
/// | scan | `DirectoryLookup` 1, then `ScanSeek` 1, then per entry one group: `ScanEntry` 1 + `ReadByte` per key and value byte |
/// | descriptor | `DirectoryLookup` 1 |
/// | create record | `DirectoryLookup` 1, `PointRead` 1 (key binding), then one group: `RecordCreate` 1 + `CellWrite` c + `CellByte` b + `RowByte` n + `RootUpdate` 3, then per attribute cell in name order, for its value term and then its existence term: one group `IndexInsert` 1 + `IndexTermCreate` 1 if the term is new + `RootUpdate` 2 |
/// | patch record | `DirectoryLookup` 1, `PointRead` 1, then one group: `RecordPatch` 1 + `CellWrite` per set cell + `CellByte` b + `CellRemove` per removed name + `RowByte` n + `RootUpdate` 2, then per changed name in order: one group `IndexRemove` 1 + `RootUpdate` 2 per term no longer held, then per term newly held one group `IndexInsert` 1 + `IndexTermCreate` 1 if new + `RootUpdate` 2 |
/// | delete record | `DirectoryLookup` 1, `PointRead` 1, then one group: `RecordDelete` 1 + `CellRemove` c + `RootUpdate` 3, then one group `IndexRemove` 1 + `RootUpdate` 2 per term held (value then existence, per attribute cell in name order) |
/// | get record | `DirectoryLookup` 1, `PointRead` 1, then (if present) one group: `ResultRow` 1 + `ReadByte` p |
/// | query | `DirectoryLookup` 1, then per group `QueryGroup` 1 and its enumeration (`TermSeek`, `TermVisit`, `PostingEntry`, `RowVisit`, `RecordProbe` as documented in the plan), then per emitted row one group: `ResultRow` 1 + `ReadByte` p |
///
/// A "group" is charged atomically: either every class in it fits the budget
/// and is charged, or nothing in the group is charged and the operation stops.
/// Within a group the classes are listed (and appear in a ledger) in the
/// order above.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperationClass {
    /// Resolving a namespace ID through the shared namespace directory.
    DirectoryLookup,
    /// Creating a namespace descriptor and allocating its ID.
    NamespaceCreate,
    /// Replacing a namespace descriptor's owner and revision.
    NamespaceTransfer,
    /// Inserting or replacing one record.
    PutRecord,
    /// Removing one record (charged whether or not the key existed).
    DeleteRecord,
    /// Each key byte submitted with a put or delete.
    WriteKeyByte,
    /// Each value byte submitted with a put.
    WriteValueByte,
    /// One point lookup in a namespace's records.
    PointRead,
    /// Positioning a scan at its first candidate entry.
    ScanSeek,
    /// Each entry a scan visits and returns.
    ScanEntry,
    /// Each key or value byte a read returns.
    ReadByte,
    /// Creating one typed record.
    RecordCreate,
    /// Patching one typed record.
    RecordPatch,
    /// Deleting one typed record.
    RecordDelete,
    /// Each cell set by a create or patch.
    CellWrite,
    /// Each byte of name plus encoded value of a cell being set.
    CellByte,
    /// Each name in a patch's remove list, or each cell of a deleted record.
    CellRemove,
    /// Each posting membership added (value term or existence term).
    IndexInsert,
    /// Each term key created by an insert, in addition to the membership.
    IndexTermCreate,
    /// Each posting membership removed.
    IndexRemove,
    /// Each AND-group a query evaluates.
    QueryGroup,
    /// Positioning the index for a group's first literal.
    TermSeek,
    /// Each distinct term key visited inside a range or prefix run.
    TermVisit,
    /// Each posting entry read.
    PostingEntry,
    /// Each live row enumerated for a match-all group or a negated literal.
    RowVisit,
    /// Each evaluation of a literal against one candidate record's cells.
    RecordProbe,
    /// Each row emitted by a record read or a query page.
    ResultRow,
    /// Each logical map-entry write that authenticated state must rehash:
    /// a raw entry, a key binding, a row, a posting entry, a term leaf or the
    /// namespace's catalog leaf. Counts logical entries, never physical
    /// nodes, so it does not depend on the shape of any structure.
    RootUpdate,
    /// Each byte of the stored row a typed record write leaves behind: key
    /// bytes plus name and encoded value bytes of every cell of the row after
    /// the write. Prepays re-encoding and rewriting the whole row leaf.
    RowByte,
    /// Each hash-referenced node a point proof emits. Reserved; no operation
    /// charges it yet.
    ProofNode,
    /// Each byte of the nodes a point proof emits. Reserved; no operation
    /// charges it yet.
    ProofByte,
}

impl OperationClass {
    /// Every class, in ledger index order.
    pub const ALL: [Self; 31] = [
        Self::DirectoryLookup,
        Self::NamespaceCreate,
        Self::NamespaceTransfer,
        Self::PutRecord,
        Self::DeleteRecord,
        Self::WriteKeyByte,
        Self::WriteValueByte,
        Self::PointRead,
        Self::ScanSeek,
        Self::ScanEntry,
        Self::ReadByte,
        Self::RecordCreate,
        Self::RecordPatch,
        Self::RecordDelete,
        Self::CellWrite,
        Self::CellByte,
        Self::CellRemove,
        Self::IndexInsert,
        Self::IndexTermCreate,
        Self::IndexRemove,
        Self::QueryGroup,
        Self::TermSeek,
        Self::TermVisit,
        Self::PostingEntry,
        Self::RowVisit,
        Self::RecordProbe,
        Self::ResultRow,
        Self::RootUpdate,
        Self::RowByte,
        Self::ProofNode,
        Self::ProofByte,
    ];

    const fn index(self) -> usize {
        self as usize
    }
}

/// Unit prices for every operation class, identified by a schedule ID.
///
/// A branch pins the schedule committed in its base snapshot, so a later
/// pricing change never reprices work already executed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceSchedule {
    id: u32,
    weights: [u64; OperationClass::ALL.len()],
}

impl PriceSchedule {
    /// Test fixture weights. **Not calibrated**: these numbers exist so that
    /// receipts are checkable by hand, and must not be read as production
    /// prices.
    pub const FIXTURE: Self = Self {
        id: 1,
        weights: [
            1,  // DirectoryLookup
            50, // NamespaceCreate
            20, // NamespaceTransfer
            10, // PutRecord
            5,  // DeleteRecord
            2,  // WriteKeyByte
            1,  // WriteValueByte
            3,  // PointRead
            4,  // ScanSeek
            2,  // ScanEntry
            1,  // ReadByte
            20, // RecordCreate
            10, // RecordPatch
            20, // RecordDelete
            4,  // CellWrite
            1,  // CellByte
            4,  // CellRemove
            6,  // IndexInsert
            30, // IndexTermCreate
            6,  // IndexRemove
            5,  // QueryGroup
            4,  // TermSeek
            3,  // TermVisit
            1,  // PostingEntry
            1,  // RowVisit
            2,  // RecordProbe
            3,  // ResultRow
            2,  // RootUpdate
            1,  // RowByte
            2,  // ProofNode
            1,  // ProofByte
        ],
    };

    /// Builds a schedule from one weight per class, in
    /// [`OperationClass::ALL`] order (the ledger index order).
    pub const fn new(id: u32, weights: [u64; OperationClass::ALL.len()]) -> Self {
        Self { id, weights }
    }

    /// Builds a schedule with every class priced at `weight`.
    pub const fn uniform(id: u32, weight: u64) -> Self {
        Self {
            id,
            weights: [weight; OperationClass::ALL.len()],
        }
    }

    /// Returns a copy with one class repriced.
    pub const fn with_price(mut self, class: OperationClass, weight: u64) -> Self {
        self.weights[class.index()] = weight;
        self
    }

    /// Returns a copy under a different schedule ID.
    pub const fn with_id(mut self, id: u32) -> Self {
        self.id = id;
        self
    }

    /// Schedule identity recorded in receipts.
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// Unit price of `class`.
    pub const fn price(&self, class: OperationClass) -> u64 {
        self.weights[class.index()]
    }
}

/// A spending limit for one operation, plus whether to record a debug ledger
/// and whether a host [`Interrupt`](crate::Interrupt) may stop it.
///
/// Budgets are optional for reads (a local RPC decides), but counting is not:
/// every operation produces a receipt whether or not it had a limit. Tracing
/// never changes charges; it only adds the ledger to the receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    limit: Option<Cost>,
    trace: bool,
    interruptible: bool,
}

impl Budget {
    /// A budget of exactly `units`.
    pub const fn limit(units: u64) -> Self {
        Self {
            limit: Some(Cost(units)),
            trace: false,
            interruptible: false,
        }
    }

    /// No limit: the operation runs to completion and is still counted.
    pub const fn unlimited() -> Self {
        Self {
            limit: None,
            trace: false,
            interruptible: false,
        }
    }

    /// Requests a debug ledger in the receipt.
    pub const fn traced(mut self) -> Self {
        self.trace = true;
        self
    }

    /// Lets the [`Interrupt`](crate::Interrupt) of an enclosing scope stop
    /// the operation at its next charge (see [`crate::interrupt`]). Charges
    /// are unchanged unless the flag is raised. Only for reads a host
    /// abandons; block execution must never opt in.
    pub const fn interruptible(mut self) -> Self {
        self.interruptible = true;
        self
    }

    /// The configured limit, if any.
    pub const fn limit_units(&self) -> Option<Cost> {
        self.limit
    }

    /// Whether a ledger was requested.
    pub const fn is_traced(&self) -> bool {
        self.trace
    }

    /// Whether a host interrupt may stop the operation.
    pub const fn is_interruptible(&self) -> bool {
        self.interruptible
    }
}

/// Whether a receipt describes gas-relevant write work or local read work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReceiptKind {
    /// Consensus-path work; Arkiv converts these receipts to gas.
    Write,
    /// Read/scan work; never enters consensus.
    Read,
}

/// Why an operation was rejected. The receipt still records what was charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The namespace does not exist in the selected state.
    UnknownNamespace,
    /// The actor is not the namespace's current owner.
    Unauthorized,
    /// The transfer named a revision other than the current one.
    StaleRevision {
        /// The revision the request expected.
        expected: Revision,
        /// The revision the namespace actually has.
        current: Revision,
    },
    /// The operation's budget is larger than the block's remaining budget.
    ///
    /// This is an admission decision made before any charge: the operation
    /// belongs in a later block. No work is done and nothing is recorded.
    ExceedsBlockBudget {
        /// Block budget still available at admission time.
        remaining: Cost,
    },
    /// The next charge would exceed the operation's effective budget.
    OutOfBudget,
    /// A charge or running total does not fit in 64 bits.
    CostOverflow,
    /// The namespace directory has reached its configured capacity.
    DirectoryFull,
    /// A namespace ordinal, record ordinal or revision counter cannot be
    /// incremented.
    CounterExhausted,
    /// A record with that key is already live in the namespace.
    AlreadyExists,
    /// No live record has that key.
    NotFound,
    /// The patched record would hold more than `Limits::max_cells` cells.
    /// Discovered after the key lookup charge, so it is recorded like any
    /// other post-charge rejection.
    RecordFull,
    /// The write's arguments violate an instance limit or are malformed.
    ///
    /// Decided at admission before any charge: spent 0, nothing recorded.
    InvalidArgument(ArgumentFault),
    /// The query violates an instance limit or names an unsupported
    /// predicate. Decided at admission before any charge.
    InvalidQuery(QueryFault),
    /// The cursor was issued for another snapshot, branch, engine,
    /// namespace, query or projection. Decided at admission before any charge.
    CursorMismatch,
}

impl fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownNamespace => write!(formatter, "unknown namespace"),
            Self::Unauthorized => write!(formatter, "actor is not the namespace owner"),
            Self::StaleRevision { expected, current } => write!(
                formatter,
                "stale namespace revision: expected {}, current {}",
                expected.0, current.0
            ),
            Self::ExceedsBlockBudget { remaining } => write!(
                formatter,
                "operation budget exceeds the block's remaining {remaining}"
            ),
            Self::OutOfBudget => write!(formatter, "operation budget exhausted"),
            Self::CostOverflow => write!(formatter, "cost arithmetic overflow"),
            Self::DirectoryFull => write!(formatter, "namespace directory is full"),
            Self::CounterExhausted => write!(formatter, "counter exhausted"),
            Self::AlreadyExists => write!(formatter, "record key already exists"),
            Self::NotFound => write!(formatter, "record not found"),
            Self::RecordFull => write!(formatter, "record would exceed the cell limit"),
            Self::InvalidArgument(fault) => write!(formatter, "invalid argument: {fault}"),
            Self::InvalidQuery(fault) => write!(formatter, "invalid query: {fault}"),
            Self::CursorMismatch => write!(formatter, "cursor does not match the request"),
        }
    }
}

impl std::error::Error for Rejection {}

/// Result of an operation as recorded in its receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The operation's logical changes were applied (or, for reads, returned).
    Applied,
    /// The operation stopped; its logical changes were not applied.
    Rejected(Rejection),
}

/// One aggregated ledger line: how many units of a class were charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    /// The charged class.
    pub class: OperationClass,
    /// Total count charged for the class within the operation.
    pub count: u64,
    /// Unit price under the receipt's schedule.
    pub unit_price: u64,
    /// `count * unit_price`.
    pub subtotal: Cost,
}

/// Physical node visits observed while executing an operation.
///
/// This is **not** canonical and is deliberately kept out of [`Receipt`]: it
/// measures the in-memory tree layout so that isolation experiments can
/// separate shared directory work from namespace-local work. It may change
/// with the data structure and never affects charges.
///
/// Directory nodes are counted on the lookup path and again on the update
/// path of a write (a persistent map copies that path). Record nodes are
/// counted on lookups, update paths and scan traversals of the selected
/// namespace's own maps only: the raw byte record map, the key-binding map
/// and the row map. Index nodes are the selected namespace's term map and
/// posting map nodes touched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalTrace {
    /// Nodes of the shared namespace directory touched.
    pub directory_nodes: u64,
    /// Nodes of the selected namespace's record, key and row maps touched.
    pub record_nodes: u64,
    /// Nodes of the selected namespace's term and posting maps touched.
    pub index_nodes: u64,
}

/// Debug ledger attached to a receipt on request. Fully canonical.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    /// Charged classes in first-charge order, aggregated per class. Classes
    /// charged with a count of zero do not appear.
    pub entries: Vec<LedgerEntry>,
}

/// The cost record of one operation, issued whether or not it succeeded.
///
/// Receipts are never withdrawn: rolling back a transaction or discarding a
/// candidate does not erase the work that was charged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Write (gas-relevant) or read (local) work.
    pub kind: ReceiptKind,
    /// Counting-rule version.
    pub model_version: u32,
    /// Pricing schedule the charges used.
    pub schedule_id: u32,
    /// Units charged, including charges made before a rejection.
    pub spent: Cost,
    /// Whether the operation applied.
    pub outcome: Outcome,
    /// Present only when the budget requested tracing.
    pub ledger: Option<Ledger>,
}

impl Receipt {
    /// Whether the operation applied.
    pub const fn is_applied(&self) -> bool {
        matches!(self.outcome, Outcome::Applied)
    }
}

/// The charging state machine for one operation.
pub(crate) struct Meter<'s> {
    schedule: &'s PriceSchedule,
    limit: Option<Cost>,
    spent: Cost,
    ledger: Option<Vec<LedgerEntry>>,
    physical: PhysicalTrace,
    /// The enclosing scope's flag, for an interruptible budget only.
    interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl<'s> Meter<'s> {
    /// Starts a meter whose effective limit is the smaller of the operation
    /// budget and, for writes, the block's remaining budget.
    pub(crate) fn new(
        schedule: &'s PriceSchedule,
        budget: Budget,
        block_remaining: Option<Cost>,
    ) -> Self {
        let limit = match (budget.limit, block_remaining) {
            (Some(op), Some(block)) => Some(op.min(block)),
            (Some(op), None) => Some(op),
            (None, block) => block,
        };
        Self {
            schedule,
            limit,
            spent: Cost::ZERO,
            ledger: budget.trace.then(Vec::new),
            physical: PhysicalTrace::default(),
            interrupt: if budget.interruptible {
                crate::interrupt::active()
            } else {
                None
            },
        }
    }

    /// Charges `count` units of `class` before the covered work runs.
    pub(crate) fn charge(&mut self, class: OperationClass, count: u64) -> Result<(), Rejection> {
        self.charge_group(&[(class, count)])
    }

    /// Checks conservative capacity before materializing proof work. No
    /// reservation is charged: the caller charges the actual work next.
    pub(crate) fn check_capacity(&self, items: &[(OperationClass, u64)]) -> Result<(), Rejection> {
        let mut next = self.spent;
        for &(class, count) in items {
            let subtotal = count
                .checked_mul(self.schedule.price(class))
                .map(Cost)
                .ok_or(Rejection::CostOverflow)?;
            next = next.checked_add(subtotal).ok_or(Rejection::CostOverflow)?;
        }
        if self.limit.is_some_and(|limit| next > limit) {
            return Err(Rejection::OutOfBudget);
        }
        Ok(())
    }

    /// Charges several classes atomically: all of them or none of them.
    pub(crate) fn charge_group(
        &mut self,
        items: &[(OperationClass, u64)],
    ) -> Result<(), Rejection> {
        // An abandoned read stops before the work this group covers; the
        // host discards its receipt (see `crate::interrupt`).
        if self
            .interrupt
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
        {
            return Err(Rejection::OutOfBudget);
        }
        let mut total = Cost::ZERO;
        let mut subtotals = [Cost::ZERO; OperationClass::ALL.len()];
        for &(class, count) in items {
            let subtotal = count
                .checked_mul(self.schedule.price(class))
                .map(Cost)
                .ok_or(Rejection::CostOverflow)?;
            subtotals[class.index()] = subtotal;
            total = total.checked_add(subtotal).ok_or(Rejection::CostOverflow)?;
        }
        let next = self
            .spent
            .checked_add(total)
            .ok_or(Rejection::CostOverflow)?;
        if let Some(limit) = self.limit
            && next > limit
        {
            return Err(Rejection::OutOfBudget);
        }
        self.spent = next;
        if let Some(entries) = &mut self.ledger {
            for &(class, count) in items {
                if count == 0 {
                    continue;
                }
                let subtotal = subtotals[class.index()];
                match entries.iter_mut().find(|entry| entry.class == class) {
                    Some(entry) => {
                        entry.count += count;
                        entry.subtotal = Cost(entry.subtotal.0 + subtotal.0);
                    }
                    None => entries.push(LedgerEntry {
                        class,
                        count,
                        unit_price: self.schedule.price(class),
                        subtotal,
                    }),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn note_directory_nodes(&mut self, nodes: u64) {
        self.physical.directory_nodes += nodes;
    }

    pub(crate) fn note_record_nodes(&mut self, nodes: u64) {
        self.physical.record_nodes += nodes;
    }

    pub(crate) fn note_index_nodes(&mut self, nodes: u64) {
        self.physical.index_nodes += nodes;
    }

    pub(crate) const fn spent(&self) -> Cost {
        self.spent
    }

    /// Closes the meter into a canonical receipt plus the physical trace.
    pub(crate) fn finish(self, kind: ReceiptKind, outcome: Outcome) -> (Receipt, PhysicalTrace) {
        let receipt = Receipt {
            kind,
            model_version: MODEL_VERSION,
            schedule_id: self.schedule.id,
            spent: self.spent,
            outcome,
            ledger: self.ledger.map(|entries| Ledger { entries }),
        };
        (receipt, self.physical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_three_appends_four_classes_with_their_fixture_weights() {
        assert_eq!(MODEL_VERSION, 3);
        assert_eq!(OperationClass::ALL.len(), 31);
        assert_eq!(
            OperationClass::ALL[27..],
            [
                OperationClass::RootUpdate,
                OperationClass::RowByte,
                OperationClass::ProofNode,
                OperationClass::ProofByte,
            ]
        );
        for (index, class) in OperationClass::ALL.into_iter().enumerate() {
            assert_eq!(class.index(), index, "{class:?}");
        }
        let fixture = PriceSchedule::FIXTURE;
        assert_eq!(fixture.price(OperationClass::RootUpdate), 2);
        assert_eq!(fixture.price(OperationClass::RowByte), 1);
        assert_eq!(fixture.price(OperationClass::ProofNode), 2);
        assert_eq!(fixture.price(OperationClass::ProofByte), 1);
        // The version-2 weights are unchanged, e.g. the first and the last.
        assert_eq!(fixture.price(OperationClass::DirectoryLookup), 1);
        assert_eq!(fixture.price(OperationClass::ResultRow), 3);
    }

    #[test]
    fn groups_charge_atomically_and_respect_the_effective_limit() {
        let schedule = PriceSchedule::FIXTURE;
        let mut meter = Meter::new(&schedule, Budget::limit(20).traced(), Some(Cost::new(12)));
        assert_eq!(meter.charge(OperationClass::DirectoryLookup, 1), Ok(()));
        // 1 + (10 + 2*1 + 1*0) = 13 exceeds the block remainder of 12.
        assert_eq!(
            meter.charge_group(&[
                (OperationClass::PutRecord, 1),
                (OperationClass::WriteKeyByte, 1),
                (OperationClass::WriteValueByte, 0),
            ]),
            Err(Rejection::OutOfBudget)
        );
        assert_eq!(meter.spent(), Cost::new(1));
        let (receipt, _) = meter.finish(
            ReceiptKind::Write,
            Outcome::Rejected(Rejection::OutOfBudget),
        );
        let ledger = receipt.ledger.unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].class, OperationClass::DirectoryLookup);
    }

    #[test]
    fn overflow_is_reported_before_any_charge_in_the_group() {
        let schedule = PriceSchedule::uniform(9, u64::MAX);
        let mut meter = Meter::new(&schedule, Budget::unlimited(), None);
        assert_eq!(meter.charge(OperationClass::PointRead, 1), Ok(()));
        assert_eq!(
            meter.charge(OperationClass::ReadByte, 1),
            Err(Rejection::CostOverflow)
        );
        assert_eq!(
            meter.charge_group(&[(OperationClass::ScanEntry, 2)]),
            Err(Rejection::CostOverflow)
        );
        assert_eq!(meter.spent(), Cost::new(u64::MAX));
    }

    #[test]
    fn ledger_aggregates_by_class_in_first_charge_order() {
        let schedule = PriceSchedule::FIXTURE;
        let mut meter = Meter::new(&schedule, Budget::unlimited().traced(), None);
        meter.charge(OperationClass::ScanSeek, 1).unwrap();
        meter
            .charge_group(&[
                (OperationClass::ScanEntry, 1),
                (OperationClass::ReadByte, 3),
            ])
            .unwrap();
        meter
            .charge_group(&[
                (OperationClass::ScanEntry, 1),
                (OperationClass::ReadByte, 5),
            ])
            .unwrap();
        meter
            .charge_group(&[(OperationClass::WriteKeyByte, 0)])
            .unwrap();
        let (receipt, physical) = meter.finish(ReceiptKind::Read, Outcome::Applied);
        assert_eq!(physical, PhysicalTrace::default());
        assert_eq!(receipt.spent, Cost::new(4 + 2 + 3 + 2 + 5));
        let entries = receipt.ledger.unwrap().entries;
        let summary: Vec<_> = entries
            .iter()
            .map(|entry| (entry.class, entry.count, entry.subtotal.units()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (OperationClass::ScanSeek, 1, 4),
                (OperationClass::ScanEntry, 2, 4),
                (OperationClass::ReadByte, 8, 8),
            ]
        );
    }
}
