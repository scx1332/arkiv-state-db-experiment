//! A fault-injecting [`VersionedStore`] wrapper.

use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard, PoisonError},
};

use crate::versioned::{
    CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, SpaceId,
    StorageError, StorageErrorKind, StoreStats, Table, VersionedStore,
};

/// A commit fault, consumed by the next [`VersionedStore::commit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFault {
    /// Refuse before anything is applied: `NotCommitted(kind)`, as when the
    /// medium fails while the transaction is staged. In latching mode it
    /// latches like any other failure of that kind.
    NotCommitted(StorageErrorKind),
    /// Apply the batch through the inner store, then report
    /// `Indeterminate(kind)`: the commit did happen, as when a medium fails
    /// after the commit record is durable. Latches whatever the kind.
    IndeterminateApplied(StorageErrorKind),
    /// Report `Indeterminate(kind)` without applying anything: the commit did
    /// not happen, as when a medium fails before the commit record is
    /// durable. Latches whatever the kind.
    IndeterminateNotApplied(StorageErrorKind),
    /// Apply the batch, then lose the acknowledgement: the caller sees
    /// `Indeterminate(Io)`, but the store itself is healthy and never latches.
    AckLost,
}

/// Where a full medium stops a commit ([`FaultyStore::set_disk_full`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskFull {
    /// While the transaction is staged, before it starts to commit:
    /// `NotCommitted(Full)`. A `RedbStore` whose file cannot grow reports
    /// this, because redb extends the file before its `commit()`.
    BeforeCommit,
    /// Inside the commit, with nothing applied: `Indeterminate(Full)`, as
    /// when a file system that allocates on write reports `ENOSPC` from a
    /// write or sync inside redb's `commit()`.
    DuringCommit,
}

/// Which reads a [`ReadFault`] fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadScope {
    /// Object reads in every space.
    AllObjects,
    /// Object reads in one space only.
    Space(SpaceId),
    /// Record reads.
    Records,
    /// Head reads.
    Head,
    /// Every read: objects, records and head.
    Everything,
}

impl ReadScope {
    fn covers(self, target: ReadTarget) -> bool {
        match (self, target) {
            (ReadScope::Everything, _) => true,
            (ReadScope::AllObjects, ReadTarget::Object(_)) => true,
            (ReadScope::Space(space), ReadTarget::Object(read)) => space == read,
            (ReadScope::Records, ReadTarget::Record) => true,
            (ReadScope::Head, ReadTarget::Head) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadTarget {
    Object(SpaceId),
    Record,
    Head,
}

/// A read fault: fails matching reads with `kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadFault {
    /// The reads it applies to.
    pub scope: ReadScope,
    /// The error kind returned.
    pub kind: StorageErrorKind,
    /// How many more reads fail; `None` until
    /// [`FaultyStore::clear_read_faults`].
    pub remaining: Option<u64>,
}

/// What a [`FaultyStore`] has injected so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultCounters {
    /// Reads failed by a [`ReadFault`].
    pub reads_failed: u64,
    /// Commits answered by an injected fault (including disk full).
    pub commits_faulted: u64,
    /// Of those, commits the inner store actually applied.
    pub commits_applied: u64,
    /// Recoveries failed on purpose.
    pub recoveries_failed: u64,
    /// Calls answered with a latched failure.
    pub latched_calls: u64,
}

#[derive(Debug, Default)]
struct FaultState {
    latching: bool,
    latch_corrupt: bool,
    latched: Option<StorageError>,
    commit_faults: VecDeque<CommitFault>,
    read_faults: Vec<ReadFault>,
    disk_full: Option<DiskFull>,
    recover_failures: u32,
    recover_kind: Option<StorageErrorKind>,
    counters: FaultCounters,
}

impl FaultState {
    /// Latches a failure of a read or of a staged commit if latching is on
    /// and its kind latches (`Io`, `Full`, and `Corrupt` when enabled);
    /// returns it.
    fn fail(&mut self, error: StorageError) -> StorageError {
        let latches = match error.kind {
            StorageErrorKind::Io | StorageErrorKind::Full => true,
            StorageErrorKind::Corrupt => self.latch_corrupt,
            _ => false,
        };
        if latches {
            self.latch_first(&error);
        }
        error
    }

    /// Latches the failure of an indeterminate commit, whatever its kind, if
    /// latching is on; returns it.
    fn fail_indeterminate(&mut self, error: StorageError) -> StorageError {
        self.latch_first(&error);
        error
    }

    fn latch_first(&mut self, error: &StorageError) {
        if self.latching && self.latched.is_none() {
            self.latched = Some(error.clone());
        }
    }

    fn latched(&mut self) -> Option<StorageError> {
        let error = self.latched.clone()?;
        self.counters.latched_calls += 1;
        Some(error)
    }
}

/// A [`VersionedStore`] wrapper that injects failures.
///
/// Without armed faults every call is forwarded unchanged, so a wrapped store
/// passes the whole behavioural [contract](crate::testing::contract).
///
/// Faults:
///
/// - read faults ([`ReadFault`]) for object reads (all spaces or one
///   space), record reads and head reads, for a number of calls or until
///   cleared;
/// - commit faults ([`CommitFault`]), consumed one per commit:
///   `NotCommitted` before apply, `Indeterminate` after apply (the commit did
///   happen) and without apply, and a lost acknowledgement;
/// - disk full ([`FaultyStore::set_disk_full`]): while set, every commit
///   fails with `Full` and applies nothing, either before the commit starts
///   (`NotCommitted`) or inside it (`Indeterminate`), see [`DiskFull`];
/// - failed recoveries ([`FaultyStore::fail_next_recoveries`]).
///
/// **Latching mode** ([`FaultyStore::latching`]) follows `RedbStore`'s rule
/// (crate `arkiv-db-redb`), so a caller tested against it meets the same
/// health changes as against the disk store:
///
/// - an injected `Io` or `Full` failure of a read or of a staged commit
///   (`NotCommitted`, disk full before the commit) latches; `Corrupt` and
///   every other kind do not, unless
///   [`set_latch_corrupt`](FaultyStore::set_latch_corrupt) turns `Corrupt`
///   latching on (the contract allows a store to latch it; redb does not);
/// - an injected `Indeterminate` outcome latches whatever its kind, as any
///   error of redb's `commit()` does;
/// - an injected recovery failure latches the recovery's error in place of
///   any earlier one, as a `RedbStore` whose reopen failed has no handle;
/// - a lost acknowledgement never latches.
///
/// Once latched, every call — reads included — returns the latched error
/// (commits as `NotCommitted`) and [`health`](VersionedStore::health) is
/// `Failed`, until [`recover`](VersionedStore::recover) succeeds. Without
/// latching, faults are one-shot and health is the inner store's.
#[derive(Debug)]
pub struct FaultyStore<S> {
    inner: S,
    state: Mutex<FaultState>,
}

impl<S: VersionedStore> FaultyStore<S> {
    /// Wraps `inner` with no faults armed and latching off.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            state: Mutex::new(FaultState::default()),
        }
    }

    /// Wraps `inner` with latching on.
    pub fn latching(inner: S) -> Self {
        let store = Self::new(inner);
        store.set_latching(true);
        store
    }

    fn lock(&self) -> MutexGuard<'_, FaultState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The wrapped store.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Turns latching on or off (an existing latch stays until `recover`).
    pub fn set_latching(&self, latching: bool) {
        self.lock().latching = latching;
    }

    /// In latching mode, also latches injected `Corrupt` failures of reads
    /// and staged commits (off by default, like `RedbStore`).
    pub fn set_latch_corrupt(&self, latch: bool) {
        self.lock().latch_corrupt = latch;
    }

    /// Queues a fault for a later commit; faults are consumed in order, one
    /// per commit.
    pub fn push_commit_fault(&self, fault: CommitFault) {
        self.lock().commit_faults.push_back(fault);
    }

    /// Fails matching reads with `kind`: the next `count` of them, or all of
    /// them until [`clear_read_faults`](Self::clear_read_faults) if `count`
    /// is `None`.
    pub fn fail_reads(&self, scope: ReadScope, kind: StorageErrorKind, count: Option<u64>) {
        self.lock().read_faults.push(ReadFault {
            scope,
            kind,
            remaining: count,
        });
    }

    /// Removes every read fault.
    pub fn clear_read_faults(&self) {
        self.lock().read_faults.clear();
    }

    /// Sets the disk-full condition (`None` clears it): while set, every
    /// commit fails with `Full` where `full` says, applying nothing.
    pub fn set_disk_full(&self, full: Option<DiskFull>) {
        self.lock().disk_full = full;
    }

    /// Fails the next `count` recoveries with `kind`. In latching mode each
    /// failure latches its error in place of any earlier one.
    pub fn fail_next_recoveries(&self, count: u32, kind: StorageErrorKind) {
        let mut state = self.lock();
        state.recover_failures = count;
        state.recover_kind = Some(kind);
    }

    /// Latches `error` directly, as if an injected fault had produced it.
    pub fn latch(&self, error: StorageError) {
        self.lock().latched = Some(error);
    }

    /// Removes every armed fault (the latch, if any, stays).
    pub fn clear_faults(&self) {
        let mut state = self.lock();
        state.commit_faults.clear();
        state.read_faults.clear();
        state.disk_full = None;
        state.recover_failures = 0;
    }

    /// What has been injected so far.
    pub fn injected(&self) -> FaultCounters {
        self.lock().counters
    }

    fn gate(&self, target: ReadTarget) -> Result<(), StorageError> {
        let mut state = self.lock();
        if let Some(error) = state.latched() {
            return Err(error);
        }
        let Some(index) = state
            .read_faults
            .iter()
            .position(|fault| fault.scope.covers(target) && fault.remaining != Some(0))
        else {
            return Ok(());
        };
        let fault = &mut state.read_faults[index];
        let kind = fault.kind;
        if let Some(remaining) = fault.remaining.as_mut() {
            *remaining -= 1;
            if *remaining == 0 {
                state.read_faults.remove(index);
            }
        }
        state.counters.reads_failed += 1;
        let error = StorageError::new(kind, format!("injected {target:?} read fault"));
        Err(state.fail(error))
    }
}

impl<S: VersionedStore> ObjectReader for FaultyStore<S> {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.gate(ReadTarget::Object(space))?;
        self.inner.read_object(space, id)
    }

    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.gate(ReadTarget::Object(space))?;
        self.inner.read_object_bounded(space, id, max_bytes)
    }
}

impl<S: VersionedStore> VersionedStore for FaultyStore<S> {
    fn database_id(&self) -> DatabaseId {
        self.inner.database_id()
    }

    fn head(&self) -> Result<Option<Head>, StorageError> {
        self.gate(ReadTarget::Head)?;
        self.inner.head()
    }

    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.gate(ReadTarget::Record)?;
        self.inner.read_record(table, key)
    }

    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.gate(ReadTarget::Record)?;
        self.inner.read_record_bounded(table, key, max_bytes)
    }

    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        let fault = {
            let mut state = self.lock();
            if let Some(error) = state.latched() {
                return CommitOutcome::NotCommitted(error);
            }
            match state.disk_full {
                Some(DiskFull::BeforeCommit) => {
                    Some(CommitFault::NotCommitted(StorageErrorKind::Full))
                }
                Some(DiskFull::DuringCommit) => {
                    Some(CommitFault::IndeterminateNotApplied(StorageErrorKind::Full))
                }
                None => state.commit_faults.pop_front(),
            }
        };
        let Some(fault) = fault else {
            return self.inner.commit(batch);
        };
        let injected = |kind: StorageErrorKind, what: &str| {
            let mut state = self.lock();
            state.counters.commits_faulted += 1;
            StorageError::new(kind, format!("injected commit fault: {what}"))
        };
        match fault {
            CommitFault::NotCommitted(kind) => {
                let error = injected(kind, "not committed");
                CommitOutcome::NotCommitted(self.lock().fail(error))
            }
            CommitFault::IndeterminateNotApplied(kind) => {
                let error = injected(kind, "indeterminate, not applied");
                CommitOutcome::Indeterminate(self.lock().fail_indeterminate(error))
            }
            CommitFault::IndeterminateApplied(kind) => match self.inner.commit(batch) {
                CommitOutcome::Committed => {
                    self.lock().counters.commits_applied += 1;
                    let error = injected(kind, "indeterminate, applied");
                    CommitOutcome::Indeterminate(self.lock().fail_indeterminate(error))
                }
                other => other,
            },
            CommitFault::AckLost => match self.inner.commit(batch) {
                CommitOutcome::Committed => {
                    let mut state = self.lock();
                    state.counters.commits_faulted += 1;
                    state.counters.commits_applied += 1;
                    CommitOutcome::Indeterminate(StorageError::new(
                        StorageErrorKind::Io,
                        "injected commit fault: acknowledgement lost",
                    ))
                }
                other => other,
            },
        }
    }

    fn health(&self) -> Health {
        if let Some(error) = self.lock().latched.clone() {
            return Health::Failed(error);
        }
        self.inner.health()
    }

    fn recover(&self) -> Result<Option<Head>, StorageError> {
        {
            let mut state = self.lock();
            if state.recover_failures > 0 {
                state.recover_failures -= 1;
                state.counters.recoveries_failed += 1;
                let kind = state.recover_kind.unwrap_or(StorageErrorKind::Io);
                let error = StorageError::new(kind, "injected recovery failure");
                if state.latching {
                    state.latched = Some(error.clone());
                }
                return Err(error);
            }
        }
        let head = self.inner.recover()?;
        self.lock().latched = None;
        Ok(head)
    }

    fn stats(&self) -> StoreStats {
        self.inner.stats()
    }
}
