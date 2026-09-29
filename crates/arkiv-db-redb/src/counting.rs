//! An instrumented redb storage backend.

use std::{
    io,
    ops::Bound,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::metrics::{OperationTiming, OperationTimingSnapshot};
use arkiv_db_storage::versioned::BackendStats;
use redb::{BackendError, StorageBackend};

/// Counters shared by every [`CountingBackend`] handle of one store, and a
/// kill switch.
///
/// A store keeps one probe for its whole life, so counts continue across
/// [`recover`](arkiv_db_storage::versioned::VersionedStore::recover), which
/// opens a new backend handle.
#[derive(Debug, Default)]
pub struct IoProbe {
    reads: AtomicU64,
    read_bytes: AtomicU64,
    writes: AtomicU64,
    write_bytes: AtomicU64,
    syncs: AtomicU64,
    set_lens: AtomicU64,
    lock_calls: AtomicU64,
    killed: AtomicBool,
    timings: [OperationTiming; 4],
}

impl IoProbe {
    /// Creates zeroed counters.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The counters so far. Reads and writes count successful calls and
    /// their bytes; syncs and length changes count successful calls.
    pub fn stats(&self) -> BackendStats {
        BackendStats {
            reads: self.reads.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
            set_lens: self.set_lens.load(Ordering::Relaxed),
        }
    }

    /// Timings of all attempted backend operations, including failed calls.
    pub fn operation_timings(&self) -> [OperationTimingSnapshot; 4] {
        std::array::from_fn(|i| {
            self.timings[i].snapshot(["read", "write", "sync_data", "set_len"][i])
        })
    }

    /// Lock, unlock and lock-query calls forwarded so far.
    pub fn lock_calls(&self) -> u64 {
        self.lock_calls.load(Ordering::Relaxed)
    }

    /// Simulates the death of the process that owns the file: from now on
    /// every backend call fails, except [`StorageBackend::close`] and
    /// unlocking, which still release the file locks as the kernel does for a
    /// dead process.
    ///
    /// Everything written before the kill has reached the operating system
    /// and stays visible to the next open, exactly as after `SIGKILL`; nothing
    /// redb would write on a clean close reaches the file. This is not a
    /// power-loss model (unsynced data survives); see
    /// [`CrashDisk`](crate::testing::CrashDisk) for that.
    pub fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
    }

    /// Whether [`kill`](Self::kill) was called.
    pub fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }
}

/// A [`StorageBackend`] wrapper that counts I/O and forwards **every**
/// method, including the lock methods, to the wrapped backend.
///
/// Forwarding the locks matters: redb takes its single-writer file lock
/// through them, and a wrapper that fell back to the default (unsupported)
/// lock methods would silently let a second writer open the same file.
#[derive(Debug)]
pub struct CountingBackend<B> {
    inner: B,
    probe: Arc<IoProbe>,
}

impl<B: StorageBackend> CountingBackend<B> {
    /// Wraps `inner` with fresh counters.
    pub fn new(inner: B) -> Self {
        Self::with_probe(inner, IoProbe::new())
    }

    /// Wraps `inner`, counting into an existing probe.
    pub fn with_probe(inner: B, probe: Arc<IoProbe>) -> Self {
        Self { inner, probe }
    }

    /// The shared counters.
    pub fn probe(&self) -> &Arc<IoProbe> {
        &self.probe
    }

    fn alive(&self) -> io::Result<()> {
        if self.probe.is_killed() {
            Err(io::Error::other("backend killed: simulated process death"))
        } else {
            Ok(())
        }
    }

    fn alive_for_lock(&self) -> Result<(), BackendError> {
        self.probe.lock_calls.fetch_add(1, Ordering::Relaxed);
        self.alive().map_err(BackendError::Io)
    }
}

impl<B: StorageBackend> StorageBackend for CountingBackend<B> {
    fn len(&self) -> io::Result<u64> {
        self.alive()?;
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let mut timing = self.probe.timings[0].start();
        self.alive()?;
        self.inner.read(offset, out)?;
        self.probe.reads.fetch_add(1, Ordering::Relaxed);
        self.probe
            .read_bytes
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        timing.success();
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut timing = self.probe.timings[3].start();
        self.alive()?;
        self.inner.set_len(len)?;
        self.probe.set_lens.fetch_add(1, Ordering::Relaxed);
        timing.success();
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        let mut timing = self.probe.timings[2].start();
        self.alive()?;
        self.inner.sync_data()?;
        self.probe.syncs.fetch_add(1, Ordering::Relaxed);
        timing.success();
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut timing = self.probe.timings[1].start();
        self.alive()?;
        self.inner.write(offset, data)?;
        self.probe.writes.fetch_add(1, Ordering::Relaxed);
        self.probe
            .write_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        timing.success();
        Ok(())
    }

    fn close(&self) -> io::Result<()> {
        // Always forwarded: it releases the file locks.
        self.inner.close()
    }

    fn try_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.alive_for_lock()?;
        self.inner.try_lock_range(start, end)
    }

    fn try_lock_shared_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> Result<bool, BackendError> {
        self.alive_for_lock()?;
        self.inner.try_lock_shared_range(start, end)
    }

    fn lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.alive_for_lock()?;
        self.inner.lock_range(start, end)
    }

    fn lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.alive_for_lock()?;
        self.inner.lock_shared_range(start, end)
    }

    fn unlock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        // Forwarded even after a kill: releasing a lock is what the kernel
        // does for a dead process.
        self.probe.lock_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.unlock_range(start, end)
    }

    fn query_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.alive_for_lock()?;
        self.inner.query_lock_range(start, end)
    }
}
