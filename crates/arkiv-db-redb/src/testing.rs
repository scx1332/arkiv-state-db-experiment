//! A simulated disk for crash, fault and disk-full tests of redb stores.
//!
//! [`CrashDisk`] is an in-memory device with an explicit durability model.
//! Every handle ([`CrashBackend`]) implements redb's `StorageBackend` over
//! the same device, so a store can be dropped and reopened on it
//! ([`RedbStore::with_backend`](crate::RedbStore::with_backend)).
//!
//! # Durability model
//!
//! The device keeps a **durable image** (what survives power loss) and an
//! ordered **log of unsynced operations** — every `write` and every
//! `set_len` since the last successful `sync_data`. Reads see the durable
//! image with the whole log applied, as a process sees its own writes
//! through the page cache. A successful sync applies the log to the durable
//! image and clears it.
//!
//! [`CrashDisk::crash`] models a power cut. It first **cuts off** the device:
//! every later call from any handle fails and a sync persists nothing. Only
//! then does it drop the live store, so redb's clean-close path (which would
//! write its allocator state and a clean shutdown flag) cannot reach the
//! device. The crash image is the durable image plus a subset of the logged
//! operations chosen by a [`CrashMode`]: none, all, or a seeded subset
//! applied **in log order**, where each write may be torn at 512-byte-sector
//! or byte granularity (a random subset of its units persists) and each
//! length change may or may not persist. Bytes outside every persisted unit
//! keep their previous value. The model assumes what redb assumes of real
//! devices: completed syncs are durable, and nothing in the log is ordered
//! except by syncs.
//!
//! The device can also fail on purpose: a crash point after a number of
//! mutating calls ([`CrashDisk::crash_after`]), injected read, write, sync or
//! length-change errors ([`CrashDisk::fail`]), a failed sync that loses the
//! unsynced operations while reads keep returning them
//! ([`CrashDisk::fail_sync_losing_unsynced`]), two space quotas that make
//! calls fail with `StorageFull`, and a gate that holds readers inside a read
//! ([`CrashDisk::hold_reads`]).
//!
//! # Space models
//!
//! - **Length quota** ([`CrashDisk::set_quota`]): a write or length change
//!   that would end beyond the quota fails. redb grows its file with
//!   `set_len` before it writes the new pages, so under this model running
//!   out of space always fails that length change, which redb makes before
//!   its `commit()`. Real file systems do not behave like this: extending a
//!   file with `ftruncate` on ext4 creates a sparse file and allocates
//!   nothing.
//! - **Allocation quota** ([`CrashDisk::set_allocation_quota`]): length
//!   changes are free (sparse), and a write fails if it would allocate
//!   [`BLOCK`]-byte blocks beyond the quota — how a file system with sparse
//!   files and allocation on write reports `ENOSPC`. A write that lands
//!   inside redb's `commit()` can then fail, so the commit is
//!   `Indeterminate`.

use std::{
    collections::BTreeSet,
    fmt, io,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use redb::StorageBackend;

/// Sector size used by [`Granularity::Sector`].
pub const SECTOR: usize = 512;

/// Allocation unit of [`CrashDisk::set_allocation_quota`].
pub const BLOCK: u64 = 4096;

/// An unsynced operation in a [`CrashDisk`]'s log.
#[derive(Clone, PartialEq, Eq)]
pub enum LoggedOp {
    /// `write(offset, data)`.
    Write {
        /// Byte offset.
        offset: u64,
        /// The bytes written.
        data: Vec<u8>,
    },
    /// `set_len(len)`.
    SetLen(u64),
}

impl fmt::Debug for LoggedOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoggedOp::Write { offset, data } => {
                write!(f, "Write {{ offset: {offset}, len: {} }}", data.len())
            }
            LoggedOp::SetLen(len) => write!(f, "SetLen({len})"),
        }
    }
}

/// The unit at which a torn write persists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Granularity {
    /// 512-byte sectors aligned to absolute offsets.
    Sector,
    /// Single bytes.
    Byte,
}

/// Which unsynced operations survive a crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashMode {
    /// None: only synced data survives.
    DurableOnly,
    /// All of them, complete, in order (what a process kill leaves behind).
    AllUnsynced,
    /// A seeded subset, in log order: each length change persists or not;
    /// each write is dropped, persists whole, or persists torn at the given
    /// granularity (each unit independently).
    Torn {
        /// Seed of the subset.
        seed: u64,
        /// Unit of tearing.
        granularity: Granularity,
    },
}

/// The kind of backend call an injected fault targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskOp {
    /// `read`.
    Read,
    /// `write`.
    Write,
    /// `sync_data`.
    Sync,
    /// `set_len`.
    SetLen,
}

/// Calls a [`CrashDisk`] has served, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskCounters {
    /// Successful reads.
    pub reads: u64,
    /// Successful writes.
    pub writes: u64,
    /// Successful syncs.
    pub syncs: u64,
    /// Successful length changes.
    pub set_lens: u64,
    /// Mutating calls attempted (writes, syncs, length changes), including
    /// failed ones: the numbering that [`CrashDisk::crash_after`] uses.
    pub mutating_calls: u64,
}

#[derive(Debug)]
struct Fault {
    op: DiskOp,
    skip: u64,
    kind: io::ErrorKind,
    sticky: bool,
    /// A failed sync discards the unsynced log (reads still see it).
    lose_unsynced: bool,
}

#[derive(Debug, Default)]
struct DiskState {
    durable: Vec<u8>,
    volatile: Vec<u8>,
    log: Vec<LoggedOp>,
    cut: bool,
    quota: Option<u64>,
    allocation_quota: Option<u64>,
    /// Blocks of the visible image that hold data (writes allocate them,
    /// shrinking frees them, growing the length does not allocate).
    allocated: BTreeSet<u64>,
    crash_in: Option<u64>,
    faults: Vec<Fault>,
    counters: DiskCounters,
    trace: Vec<DiskOp>,
    hold_reads: bool,
    blocked_readers: usize,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<DiskState>,
    readers: Condvar,
}

/// An in-memory device with a power-loss model (see the [module](self)
/// documentation).
///
/// Cloning yields another reference to the same device.
#[derive(Clone, Debug, Default)]
pub struct CrashDisk {
    shared: Arc<Shared>,
}

impl CrashDisk {
    /// An empty device.
    pub fn new() -> Self {
        Self::default()
    }

    /// A device whose durable (and visible) contents are `image`, as found
    /// after a crash.
    pub fn from_image(image: Vec<u8>) -> Self {
        let disk = Self::new();
        {
            let mut state = disk.lock();
            // Every block of the image counts as allocated.
            state.allocated = (0..(image.len() as u64).div_ceil(BLOCK)).collect();
            state.volatile = image.clone();
            state.durable = image;
        }
        disk
    }

    fn lock(&self) -> MutexGuard<'_, DiskState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A new backend handle on this device.
    pub fn backend(&self) -> CrashBackend {
        CrashBackend { disk: self.clone() }
    }

    /// The image a power cut right now would leave with no unsynced
    /// operation persisting.
    pub fn durable_image(&self) -> Vec<u8> {
        self.lock().durable.clone()
    }

    /// The contents reads currently see.
    pub fn visible_image(&self) -> Vec<u8> {
        self.lock().volatile.clone()
    }

    /// The unsynced operations, in order.
    pub fn unsynced(&self) -> Vec<LoggedOp> {
        self.lock().log.clone()
    }

    /// Calls served so far.
    pub fn counters(&self) -> DiskCounters {
        self.lock().counters
    }

    /// The kinds of all mutating calls attempted so far, in order (index `i`
    /// is the call [`crash_after`](Self::crash_after) numbers `i` when armed
    /// before the first of them).
    pub fn mutating_trace(&self) -> Vec<DiskOp> {
        self.lock().trace.clone()
    }

    /// Limits the device's length to `quota` bytes (`None`: unlimited). A
    /// write or length change that would end beyond it fails with
    /// `ErrorKind::StorageFull` and changes nothing (the length model of the
    /// [module documentation](self#space-models)).
    pub fn set_quota(&self, quota: Option<u64>) {
        self.lock().quota = quota;
    }

    /// Limits the device to `quota` bytes of allocated [`BLOCK`]s (`None`:
    /// unlimited). Length changes allocate nothing; a write that would
    /// allocate blocks beyond the quota fails with `ErrorKind::StorageFull`
    /// and changes nothing (the allocation model of the
    /// [module documentation](self#space-models)).
    pub fn set_allocation_quota(&self, quota: Option<u64>) {
        self.lock().allocation_quota = quota;
    }

    /// Bytes of allocated blocks in the visible image.
    pub fn allocated_bytes(&self) -> u64 {
        self.lock().allocated.len() as u64 * BLOCK
    }

    /// Arms a crash point: the next `calls` mutating calls (writes, syncs,
    /// length changes) succeed, and the one after that cuts the device off
    /// (see [`cut_off`](Self::cut_off)) and fails.
    pub fn crash_after(&self, calls: u64) {
        self.lock().crash_in = Some(calls);
    }

    /// Disarms a crash point that has not fired.
    pub fn disarm_crash(&self) {
        self.lock().crash_in = None;
    }

    /// Fails the call of kind `op` after letting `skip` such calls succeed,
    /// with `kind`. One-shot.
    pub fn fail(&self, op: DiskOp, skip: u64, kind: io::ErrorKind) {
        self.lock().faults.push(Fault {
            op,
            skip,
            kind,
            sticky: false,
            lose_unsynced: false,
        });
    }

    /// Fails the sync after letting `skip` syncs succeed, and loses the
    /// unsynced operations: they never become durable (no later sync
    /// persists them, a crash image never holds them), while reads keep
    /// returning them. One-shot.
    ///
    /// This models Linux after an `fsync` error: pages whose write-back
    /// failed can be marked clean, so the page cache serves the new bytes
    /// but the medium does not hold them.
    pub fn fail_sync_losing_unsynced(&self, skip: u64) {
        self.lock().faults.push(Fault {
            op: DiskOp::Sync,
            skip,
            kind: io::ErrorKind::Other,
            sticky: false,
            lose_unsynced: true,
        });
    }

    /// Fails every call of kind `op` with `kind` until
    /// [`clear_faults`](Self::clear_faults).
    pub fn fail_all(&self, op: DiskOp, kind: io::ErrorKind) {
        self.lock().faults.push(Fault {
            op,
            skip: 0,
            kind,
            sticky: true,
            lose_unsynced: false,
        });
    }

    /// Removes every injected fault.
    pub fn clear_faults(&self) {
        self.lock().faults.clear();
    }

    /// Makes every later read wait inside the backend until
    /// [`release_reads`](Self::release_reads).
    pub fn hold_reads(&self) {
        self.lock().hold_reads = true;
    }

    /// Lets held and future reads proceed.
    pub fn release_reads(&self) {
        self.lock().hold_reads = false;
        self.shared.readers.notify_all();
    }

    /// Waits until at least `count` readers are held, or `timeout` passes.
    /// Returns whether they are.
    pub fn wait_for_held_readers(&self, count: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while state.blocked_readers < count {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .shared
                .readers
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Cuts the device off: every later call from any handle fails, and no
    /// sync persists anything.
    pub fn cut_off(&self) {
        self.lock().cut = true;
    }

    /// Whether the device has been cut off.
    pub fn is_cut_off(&self) -> bool {
        self.lock().cut
    }

    /// Simulates a power cut under `live` (the store or anything else that
    /// holds handles on this device): cuts the device off, drops `live`, and
    /// returns the crash image `mode` selects.
    pub fn crash<T>(&self, live: T, mode: CrashMode) -> Vec<u8> {
        self.cut_off();
        drop(live);
        self.crash_image(mode)
    }

    /// The image a crash would leave now under `mode` (without cutting the
    /// device off).
    pub fn crash_image(&self, mode: CrashMode) -> Vec<u8> {
        let state = self.lock();
        let mut image = state.durable.clone();
        match mode {
            CrashMode::DurableOnly => {}
            CrashMode::AllUnsynced => {
                for op in &state.log {
                    apply(&mut image, op);
                }
            }
            CrashMode::Torn { seed, granularity } => {
                let mut rng = SplitMix::new(seed);
                for op in &state.log {
                    match op {
                        LoggedOp::SetLen(_) => {
                            if rng.coin() {
                                apply(&mut image, op);
                            }
                        }
                        LoggedOp::Write { offset, data } => match rng.below(3) {
                            0 => {}
                            1 => apply(&mut image, op),
                            _ => tear(&mut image, *offset, data, granularity, &mut rng),
                        },
                    }
                }
            }
        }
        image
    }
}

fn apply(image: &mut Vec<u8>, op: &LoggedOp) {
    match op {
        LoggedOp::SetLen(len) => image.resize(*len as usize, 0),
        LoggedOp::Write { offset, data } => {
            let start = *offset as usize;
            let end = start + data.len();
            if image.len() < end {
                image.resize(end, 0);
            }
            image[start..end].copy_from_slice(data);
        }
    }
}

fn tear(
    image: &mut Vec<u8>,
    offset: u64,
    data: &[u8],
    granularity: Granularity,
    rng: &mut SplitMix,
) {
    let start = offset as usize;
    let end = start + data.len();
    let unit = match granularity {
        Granularity::Sector => SECTOR,
        Granularity::Byte => 1,
    };
    let mut position = start;
    while position < end {
        // Units are aligned to absolute offsets, like device sectors.
        let unit_end = ((position / unit) + 1) * unit;
        let piece_end = unit_end.min(end);
        if rng.coin() {
            if image.len() < piece_end {
                image.resize(piece_end, 0);
            }
            image[position..piece_end].copy_from_slice(&data[position - start..piece_end - start]);
        }
        position = piece_end;
    }
}

/// A small deterministic generator for crash subsets (SplitMix64).
#[derive(Clone, Debug)]
pub struct SplitMix(u64);

impl SplitMix {
    /// A generator from `seed`.
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound > 0`).
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// A fair coin.
    pub fn coin(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

/// A `StorageBackend` handle on a [`CrashDisk`].
///
/// Lock methods are left at redb's defaults (unsupported), so redb opens the
/// device without file locks; tests control which handles are live.
#[derive(Clone, Debug)]
pub struct CrashBackend {
    disk: CrashDisk,
}

impl CrashBackend {
    /// The device this handle writes to.
    pub fn disk(&self) -> &CrashDisk {
        &self.disk
    }
}

fn cut_error() -> io::Error {
    io::Error::other("device cut off (simulated power loss)")
}

fn injected(kind: io::ErrorKind, op: DiskOp) -> io::Error {
    io::Error::new(kind, format!("injected {op:?} fault"))
}

impl DiskState {
    /// Checks the cut-off flag, the crash point (for mutating calls) and the
    /// injected faults, in that order.
    fn admit(&mut self, op: DiskOp) -> io::Result<()> {
        if self.cut {
            return Err(cut_error());
        }
        if op != DiskOp::Read {
            self.counters.mutating_calls += 1;
            self.trace.push(op);
            if let Some(remaining) = self.crash_in.as_mut() {
                if *remaining == 0 {
                    self.crash_in = None;
                    self.cut = true;
                    return Err(cut_error());
                }
                *remaining -= 1;
            }
        }
        let mut fired = None;
        for (index, fault) in self.faults.iter_mut().enumerate() {
            if fault.op != op {
                continue;
            }
            if fault.skip > 0 {
                fault.skip -= 1;
                continue;
            }
            fired = Some((index, fault.kind, fault.sticky, fault.lose_unsynced));
            break;
        }
        if let Some((index, kind, sticky, lose_unsynced)) = fired {
            if !sticky {
                self.faults.remove(index);
            }
            if lose_unsynced {
                self.log.clear();
            }
            return Err(injected(kind, op));
        }
        Ok(())
    }

    fn within_quota(&self, end: u64) -> io::Result<()> {
        match self.quota {
            Some(quota) if end > quota => Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!("simulated disk full: {end} bytes exceed the quota of {quota}"),
            )),
            _ => Ok(()),
        }
    }

    /// The blocks a write of `len` bytes at `offset` covers (none if empty).
    fn blocks(offset: u64, len: usize) -> std::ops::Range<u64> {
        let first = offset / BLOCK;
        if len == 0 {
            return first..first;
        }
        first..(offset + len as u64 - 1) / BLOCK + 1
    }

    fn within_allocation_quota(&self, offset: u64, len: usize) -> io::Result<()> {
        let Some(quota) = self.allocation_quota else {
            return Ok(());
        };
        let new = Self::blocks(offset, len)
            .filter(|block| !self.allocated.contains(block))
            .count() as u64;
        let needed = (self.allocated.len() as u64 + new) * BLOCK;
        if new > 0 && needed > quota {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "simulated disk full: {needed} allocated bytes exceed the quota of {quota}"
                ),
            ));
        }
        Ok(())
    }
}

impl StorageBackend for CrashBackend {
    fn len(&self) -> io::Result<u64> {
        let state = self.disk.lock();
        if state.cut {
            return Err(cut_error());
        }
        Ok(state.volatile.len() as u64)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let mut state = self.disk.lock();
        if state.hold_reads {
            state.blocked_readers += 1;
            self.disk.shared.readers.notify_all();
            while state.hold_reads {
                state = self
                    .disk
                    .shared
                    .readers
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state.blocked_readers -= 1;
        }
        state.admit(DiskOp::Read)?;
        let start = offset as usize;
        let end = start + out.len();
        if end > state.volatile.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read of {}..{end} beyond the device length {}",
                    start,
                    state.volatile.len()
                ),
            ));
        }
        out.copy_from_slice(&state.volatile[start..end]);
        state.counters.reads += 1;
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut state = self.disk.lock();
        state.admit(DiskOp::SetLen)?;
        state.within_quota(len)?;
        let kept = len.div_ceil(BLOCK);
        state.allocated.retain(|block| *block < kept);
        let op = LoggedOp::SetLen(len);
        apply(&mut state.volatile, &op);
        state.log.push(op);
        state.counters.set_lens += 1;
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        let mut state = self.disk.lock();
        state.admit(DiskOp::Sync)?;
        let log = std::mem::take(&mut state.log);
        for op in &log {
            apply(&mut state.durable, op);
        }
        state.counters.syncs += 1;
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.disk.lock();
        state.admit(DiskOp::Write)?;
        state.within_quota(offset + data.len() as u64)?;
        state.within_allocation_quota(offset, data.len())?;
        let blocks = DiskState::blocks(offset, data.len());
        state.allocated.extend(blocks);
        let op = LoggedOp::Write {
            offset,
            data: data.to_vec(),
        };
        apply(&mut state.volatile, &op);
        state.log.push(op);
        state.counters.writes += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_makes_the_log_durable_and_reads_see_unsynced_writes() {
        let disk = CrashDisk::new();
        let backend = disk.backend();
        backend.set_len(1024).unwrap();
        backend.write(10, b"abc").unwrap();
        let mut out = [0u8; 3];
        backend.read(10, &mut out).unwrap();
        assert_eq!(&out, b"abc");
        assert_eq!(disk.durable_image(), Vec::<u8>::new());
        assert_eq!(disk.unsynced().len(), 2);
        backend.sync_data().unwrap();
        assert!(disk.unsynced().is_empty());
        assert_eq!(disk.durable_image().len(), 1024);
        assert_eq!(&disk.durable_image()[10..13], b"abc");
    }

    #[test]
    fn crash_images_follow_the_mode() {
        let disk = CrashDisk::from_image(vec![0xee; 2048]);
        let backend = disk.backend();
        backend.write(0, &[1; 1024]).unwrap();
        backend.set_len(4096).unwrap();
        backend.write(3000, &[2; 100]).unwrap();

        assert_eq!(disk.crash_image(CrashMode::DurableOnly), vec![0xee; 2048]);
        let all = disk.crash_image(CrashMode::AllUnsynced);
        assert_eq!(all.len(), 4096);
        assert_eq!(&all[..1024], &[1; 1024][..]);
        assert_eq!(&all[1024..2048], &[0xee; 1024][..]);
        assert_eq!(&all[3000..3100], &[2; 100][..]);
        assert_eq!(all, disk.visible_image());

        for seed in 0..64 {
            for granularity in [Granularity::Sector, Granularity::Byte] {
                let torn = disk.crash_image(CrashMode::Torn { seed, granularity });
                // Bytes never written keep their old value.
                assert!(torn[1024..2048.min(torn.len())].iter().all(|b| *b == 0xee));
                // Every byte of the first write is either old or new.
                assert!(torn[..1024].iter().all(|b| *b == 0xee || *b == 1));
                if granularity == Granularity::Sector {
                    // Sector tearing keeps each 512-byte sector whole.
                    for sector in torn[..1024].chunks(SECTOR) {
                        assert!(sector.iter().all(|b| *b == sector[0]));
                    }
                }
            }
        }
        // Different seeds choose different subsets.
        let images: std::collections::BTreeSet<Vec<u8>> = (0..16)
            .map(|seed| {
                disk.crash_image(CrashMode::Torn {
                    seed,
                    granularity: Granularity::Sector,
                })
            })
            .collect();
        assert!(images.len() > 4);
    }

    #[test]
    fn cut_off_fails_every_call_and_persists_nothing() {
        let disk = CrashDisk::new();
        let backend = disk.backend();
        backend.write(0, b"synced").unwrap();
        backend.sync_data().unwrap();
        backend.write(0, b"lost!!").unwrap();
        let image = disk.crash(backend.clone(), CrashMode::DurableOnly);
        assert_eq!(image, b"synced");
        assert!(backend.sync_data().is_err());
        assert!(backend.write(0, b"x").is_err());
        assert!(backend.read(0, &mut [0; 1]).is_err());
        assert!(backend.len().is_err());
        assert_eq!(disk.durable_image(), b"synced");
    }

    #[test]
    fn crash_point_counts_mutating_calls() {
        let disk = CrashDisk::new();
        let backend = disk.backend();
        disk.crash_after(2);
        backend.write(0, b"a").unwrap();
        let mut out = [0u8; 1];
        backend.read(0, &mut out).unwrap();
        backend.sync_data().unwrap();
        assert!(backend.set_len(10).is_err());
        assert!(disk.is_cut_off());
        assert_eq!(disk.counters().mutating_calls, 3);
        assert_eq!(disk.durable_image(), b"a");
    }

    #[test]
    fn quota_and_injected_faults() {
        let disk = CrashDisk::new();
        let backend = disk.backend();
        disk.set_quota(Some(100));
        assert_eq!(
            backend.set_len(101).unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(
            backend.write(90, &[0; 11]).unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        backend.write(90, &[0; 10]).unwrap();
        assert_eq!(disk.visible_image().len(), 100);

        disk.fail(DiskOp::Write, 1, io::ErrorKind::Other);
        backend.write(0, b"ok").unwrap();
        assert!(backend.write(0, b"no").is_err());
        backend.write(0, b"ok").unwrap();

        disk.fail_all(DiskOp::Sync, io::ErrorKind::Other);
        assert!(backend.sync_data().is_err());
        assert!(backend.sync_data().is_err());
        disk.clear_faults();
        backend.sync_data().unwrap();
    }

    #[test]
    fn allocation_quota_charges_written_blocks_not_length() {
        let disk = CrashDisk::new();
        let backend = disk.backend();
        disk.set_allocation_quota(Some(3 * BLOCK));
        // Growing the length is sparse and free.
        backend.set_len(100 * BLOCK).unwrap();
        assert_eq!(disk.allocated_bytes(), 0);
        // One byte allocates one block; a write straddling two blocks two.
        backend.write(10, b"x").unwrap();
        backend.write(2 * BLOCK - 1, b"yz").unwrap();
        assert_eq!(disk.allocated_bytes(), 3 * BLOCK);
        // Rewriting allocated blocks is free; a fourth block is refused and
        // changes nothing.
        backend.write(0, &[7; 2 * BLOCK as usize]).unwrap();
        let before = disk.visible_image();
        assert_eq!(
            backend.write(50 * BLOCK, b"w").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(disk.visible_image(), before);
        // Shrinking frees blocks beyond the new length.
        backend.set_len(BLOCK + 1).unwrap();
        assert_eq!(disk.allocated_bytes(), 2 * BLOCK);
        backend.write(BLOCK + 1, b"a").unwrap();
        assert_eq!(disk.allocated_bytes(), 2 * BLOCK);
        // An image counts as fully allocated.
        let copy = CrashDisk::from_image(vec![0; 2 * BLOCK as usize + 1]);
        assert_eq!(copy.allocated_bytes(), 3 * BLOCK);
    }

    #[test]
    fn a_sync_losing_unsynced_writes_keeps_them_readable_but_never_durable() {
        let disk = CrashDisk::new();
        let backend = disk.backend();
        backend.write(0, b"old").unwrap();
        backend.sync_data().unwrap();
        backend.write(0, b"new").unwrap();
        disk.fail_sync_losing_unsynced(0);
        assert!(backend.sync_data().is_err());
        let mut out = [0u8; 3];
        backend.read(0, &mut out).unwrap();
        assert_eq!(&out, b"new");
        // A later sync succeeds but persists nothing of the lost write.
        backend.sync_data().unwrap();
        assert_eq!(disk.durable_image(), b"old");
        assert_eq!(disk.crash_image(CrashMode::AllUnsynced), b"old");
    }
}
