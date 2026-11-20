//! A fault-injecting [`VersionedStore`] wrapper.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use crate::versioned::{
    CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, Relabel, SpaceId,
    StorageError, StorageErrorKind, StoreStats, Table, VersionedStore, copy_bounded,
};

/// A fault consumed by the next write: [`VersionedStore::commit`],
/// [`stage_objects`](VersionedStore::stage_objects) or
/// [`relabel`](VersionedStore::relabel).
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

/// Which reads [`FaultyStore::fail_reads`] fails.
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
}

impl ReadScope {
    fn covers(self, target: ReadTarget) -> bool {
        match (self, target) {
            (ReadScope::Space(space), ReadTarget::Object(read)) => space == read,
            (ReadScope::AllObjects, ReadTarget::Object(_))
            | (ReadScope::Records, ReadTarget::Record)
            | (ReadScope::Head, ReadTarget::Head) => true,
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

/// Lets `skip` matching reads through, then fails matching reads with
/// `kind`: `remaining` more of them, or all of them until cleared if `None`.
#[derive(Clone, Copy, Debug)]
struct ReadFault {
    scope: ReadScope,
    kind: StorageErrorKind,
    skip: u64,
    remaining: Option<u64>,
}

/// Called before every object read ([`FaultyStore::set_object_read_hook`]).
pub type ObjectReadHook = Arc<dyn Fn(SpaceId, &ObjectId) + Send + Sync>;

#[derive(Clone, Default)]
struct Hook(Option<ObjectReadHook>);

impl fmt::Debug for Hook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "Hook(set)"
        } else {
            "Hook(unset)"
        })
    }
}

/// What a [`FaultyStore`] has injected and which calls it has seen so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultCounters {
    /// Reads failed by an injected read fault.
    pub reads_failed: u64,
    /// Writes the inner store applied although a fault was reported.
    pub commits_applied: u64,
    /// Recoveries failed on purpose.
    pub recoveries_failed: u64,
    /// Calls answered with a latched failure.
    pub latched_calls: u64,
    /// `stage_objects` calls, whatever their outcome.
    pub stage_calls: u64,
    /// `recover` calls, whatever their outcome.
    pub recover_calls: u64,
}

#[derive(Debug, Default)]
struct FaultState {
    latching: bool,
    latch_corrupt: bool,
    latched: Option<StorageError>,
    commit_faults: VecDeque<CommitFault>,
    write_skips: u64,
    read_faults: Vec<ReadFault>,
    object_overrides: BTreeMap<(SpaceId, ObjectId), Option<Vec<u8>>>,
    record_overrides: BTreeMap<(Table, Vec<u8>), Option<Vec<u8>>>,
    head_override: Option<Option<Head>>,
    object_read_hook: Hook,
    forbid_unbounded_reads: bool,
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
/// Faults are armed with the `fail_*`, `push_*` and `set_*` methods and read
/// answers are replaced with the `override_*` methods; with none armed every
/// call is forwarded unchanged, so a wrapped store passes the behavioural
/// [contract](crate::testing::contract).
///
/// **Latching mode** ([`FaultyStore::latching`]) follows `RedbStore`'s rule
/// (the `error` module of crate `arkiv-db-redb`), so a caller tested against
/// it meets the same health changes as against the disk store: an injected
/// `Io` or `Full` failure of a read or of a staged write latches, an
/// injected `Indeterminate` outcome latches whatever its kind, a failed
/// recovery latches its own error in place of any earlier one, and a lost
/// acknowledgement never latches.
/// [`set_latch_corrupt`](FaultyStore::set_latch_corrupt) also latches
/// `Corrupt`, which the contract allows and redb does not do.
///
/// Once latched, every call, reads included, returns the latched error
/// (writes as `NotCommitted`) and [`health`](VersionedStore::health) is
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

    /// Queues a fault for a later write; faults are consumed in order, one
    /// per `commit`, `stage_objects` or `relabel`.
    pub fn push_commit_fault(&self, fault: CommitFault) {
        self.lock().commit_faults.push_back(fault);
    }

    /// Lets the next `count` writes through before the queued faults apply,
    /// so a queued fault hits write number `count + 1`.
    pub fn skip_writes(&self, count: u64) {
        self.lock().write_skips = count;
    }

    /// Fails matching reads with `kind`: the next `count` of them, or all of
    /// them until [`clear_read_faults`](Self::clear_read_faults) if `count`
    /// is `None`.
    pub fn fail_reads(&self, scope: ReadScope, kind: StorageErrorKind, count: Option<u64>) {
        self.lock().read_faults.push(ReadFault {
            scope,
            kind,
            skip: 0,
            remaining: count,
        });
    }

    /// Lets the next `skip` matching reads through, then fails every
    /// matching read with `kind` until
    /// [`clear_read_faults`](Self::clear_read_faults).
    pub fn fail_reads_after(&self, scope: ReadScope, skip: u64, kind: StorageErrorKind) {
        self.lock().read_faults.push(ReadFault {
            scope,
            kind,
            skip,
            remaining: None,
        });
    }

    /// Removes every read fault.
    pub fn clear_read_faults(&self) {
        self.lock().read_faults.clear();
    }

    /// Answers reads of one object with `bytes` instead of the stored
    /// value; `None` answers "absent". A bounded read still checks the
    /// length of the replacement.
    pub fn override_object(&self, space: SpaceId, id: ObjectId, bytes: Option<Vec<u8>>) {
        self.lock().object_overrides.insert((space, id), bytes);
    }

    /// The record counterpart of [`override_object`](Self::override_object).
    pub fn override_record(&self, table: Table, key: &[u8], value: Option<Vec<u8>>) {
        self.lock()
            .record_overrides
            .insert((table, key.to_vec()), value);
    }

    /// Answers [`head`](VersionedStore::head) and a successful
    /// [`recover`](VersionedStore::recover) with `head`.
    pub fn override_head(&self, head: Option<Head>) {
        self.lock().head_override = Some(head);
    }

    /// Removes every read override.
    pub fn clear_overrides(&self) {
        let mut state = self.lock();
        state.object_overrides.clear();
        state.record_overrides.clear();
        state.head_override = None;
    }

    /// Calls `hook` at the start of every object read, before any fault or
    /// override and outside the wrapper's lock, so it may block or sleep.
    pub fn set_object_read_hook(&self, hook: Option<ObjectReadHook>) {
        self.lock().object_read_hook = Hook(hook);
    }

    /// Makes the unbounded `read_object` and `read_record` panic, for
    /// callers that must only read with a limit.
    pub fn forbid_unbounded_reads(&self) {
        self.lock().forbid_unbounded_reads = true;
    }

    /// Sets the disk-full condition (`None` clears it): while set, every
    /// write fails with `Full` where `full` says, applying nothing.
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

    /// What has been injected and seen so far.
    pub fn injected(&self) -> FaultCounters {
        self.lock().counters
    }

    fn unbounded_reads_allowed(&self) -> bool {
        !self.lock().forbid_unbounded_reads
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
        if fault.skip > 0 {
            fault.skip -= 1;
            return Ok(());
        }
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

    /// Runs one write of the inner store behind the latch, the disk-full
    /// condition and the commit-fault queue.
    fn write(&self, apply: impl FnOnce(&S) -> CommitOutcome) -> CommitOutcome {
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
                None if state.write_skips > 0 => {
                    state.write_skips -= 1;
                    None
                }
                None => state.commit_faults.pop_front(),
            }
        };
        let Some(fault) = fault else {
            return apply(&self.inner);
        };
        let injected = |kind: StorageErrorKind, what: &str| {
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
            CommitFault::IndeterminateApplied(kind) => match apply(&self.inner) {
                CommitOutcome::Committed => {
                    let mut state = self.lock();
                    state.counters.commits_applied += 1;
                    let error = injected(kind, "indeterminate, applied");
                    CommitOutcome::Indeterminate(state.fail_indeterminate(error))
                }
                other => other,
            },
            CommitFault::AckLost => match apply(&self.inner) {
                CommitOutcome::Committed => {
                    self.lock().counters.commits_applied += 1;
                    CommitOutcome::Indeterminate(injected(
                        StorageErrorKind::Io,
                        "acknowledgement lost",
                    ))
                }
                other => other,
            },
        }
    }
}

impl<S: VersionedStore> ObjectReader for FaultyStore<S> {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        assert!(self.unbounded_reads_allowed(), "unbounded object read");
        self.read_object_bounded(space, id, usize::MAX)
    }

    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let hook = self.lock().object_read_hook.clone();
        if let Some(hook) = hook.0 {
            hook(space, id);
        }
        self.gate(ReadTarget::Object(space))?;
        if let Some(bytes) = self.lock().object_overrides.get(&(space, *id)) {
            return replacement(bytes.as_deref(), max_bytes);
        }
        self.inner.read_object_bounded(space, id, max_bytes)
    }
}

/// A read override's answer, under the bounded read's length check.
fn replacement(bytes: Option<&[u8]>, max_bytes: usize) -> Result<Option<Vec<u8>>, StorageError> {
    bytes
        .map(|bytes| copy_bounded(bytes, max_bytes))
        .transpose()
}

impl<S: VersionedStore> VersionedStore for FaultyStore<S> {
    fn database_id(&self) -> DatabaseId {
        self.inner.database_id()
    }

    fn head(&self) -> Result<Option<Head>, StorageError> {
        self.gate(ReadTarget::Head)?;
        let head = self.inner.head()?;
        Ok(self.lock().head_override.unwrap_or(head))
    }

    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        assert!(self.unbounded_reads_allowed(), "unbounded record read");
        self.read_record_bounded(table, key, usize::MAX)
    }

    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.gate(ReadTarget::Record)?;
        if let Some(value) = self.lock().record_overrides.get(&(table, key.to_vec())) {
            return replacement(value.as_deref(), max_bytes);
        }
        self.inner.read_record_bounded(table, key, max_bytes)
    }

    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        self.write(|inner| inner.commit(batch))
    }

    fn stage_objects(&self, objects: &[(SpaceId, ObjectId, Vec<u8>)]) -> CommitOutcome {
        self.lock().counters.stage_calls += 1;
        self.write(|inner| inner.stage_objects(objects))
    }

    fn relabel(&self, relabel: &Relabel) -> CommitOutcome {
        self.write(|inner| inner.relabel(relabel))
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
            state.counters.recover_calls += 1;
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
        let mut state = self.lock();
        state.latched = None;
        Ok(state.head_override.unwrap_or(head))
    }

    fn stats(&self) -> StoreStats {
        self.inner.stats()
    }

    fn gc_state(&self) -> Result<Option<crate::gc::GcState>, StorageError> {
        self.inner.gc_state()
    }

    /// Faults apply as they do to [`commit`](VersionedStore::commit).
    fn commit_gc(
        &self,
        batch: &CommitBatch,
        gc: &crate::gc::GcCommit,
        graph: &dyn crate::gc::ObjectGraph,
    ) -> CommitOutcome {
        self.write(|inner| inner.commit_gc(batch, gc, graph))
    }

    fn gc_report(&self) -> Option<crate::gc::GcReport> {
        self.inner.gc_report()
    }

    fn gc_enable(
        &self,
        roots: &[(SpaceId, ObjectId)],
        graph: &dyn crate::gc::ObjectGraph,
        spill: Option<&std::path::Path>,
    ) -> Result<crate::gc::GcState, StorageError> {
        self.inner.gc_enable(roots, graph, spill)
    }

    fn gc_dump(&self) -> Result<Option<crate::versioned::GcDump>, StorageError> {
        self.inner.gc_dump()
    }
}
