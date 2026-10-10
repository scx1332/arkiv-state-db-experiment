//! State-history retention: the policy, the durable floor and reader pins.
//!
//! A durable engine keeps every snapshot (`Retention::Archive`) unless it is
//! configured with a window. With `Retention::Window(w)` the **state floor**
//! is `max(previous floor, head - w)` (saturating at zero): snapshots
//! `floor..=head` are readable and every request for an earlier snapshot is
//! refused with [`EngineError::HistoryPruned`](crate::EngineError) before
//! any manifest, trie object or cache entry is consulted, even while the
//! data is still on disk. The floor never moves backwards.
//!
//! Durability: each commit whose floor is above zero writes the insert-only
//! metadata record `engine/state-floor/<sequence>` (the floor as 8 bytes,
//! big-endian) in the same atomic batch as its manifest, so the floor that
//! belongs to the durable head is read back on open. Opening with a smaller
//! window raises the floor at once for reads; the raised value becomes
//! durable with the next commit. Opening with a larger window, or reopening
//! before that commit, keeps the durable floor: history once declared gone
//! never comes back. Opening a database whose durable floor is above zero
//! with [`Retention::Archive`] is refused.
//!
//! Pins: every [`Snapshot`](crate::Snapshot) handle the durable engine hands
//! out (the head, a historical snapshot, a prepared publication) registers
//! its sequence here until its last clone (including branches built on it)
//! is dropped. Pins do not hold the logical floor back (answers stay a pure
//! function of head and window), and a handle opened before the floor passed
//! keeps reading until it is dropped. Physical deletion, which is not
//! implemented yet, must not remove anything at or above
//! `min(durable floor, lowest pinned sequence)`.

use std::{
    collections::BTreeMap,
    fmt,
    str::FromStr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

/// How much state history a durable engine keeps readable.
///
/// This is a runtime option of one node, not part of genesis or of any
/// logical hash: two nodes of one chain may run different policies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Retention {
    /// Keep and serve every snapshot (the default).
    #[default]
    Archive,
    /// Serve only the head and the `w` snapshots before it. `Window(0)` is
    /// head-only, like golem-db today.
    Window(u64),
}

impl Retention {
    /// The floor this policy alone implies for `head`.
    pub const fn floor_at(self, head: u64) -> u64 {
        match self {
            Self::Archive => 0,
            Self::Window(window) => head.saturating_sub(window),
        }
    }

    /// Stable label for status output: `"archive"` or `"window"`.
    pub const fn mode(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Window(_) => "window",
        }
    }

    /// The window in snapshots, `None` for archive.
    pub const fn window(self) -> Option<u64> {
        match self {
            Self::Archive => None,
            Self::Window(window) => Some(window),
        }
    }
}

impl fmt::Display for Retention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archive => f.write_str("archive"),
            Self::Window(window) => write!(f, "{window}"),
        }
    }
}

impl FromStr for Retention {
    type Err = String;
    /// `archive`, or a window as a decimal number of snapshots.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "archive" {
            return Ok(Self::Archive);
        }
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "state retention must be `archive` or a decimal snapshot count, got {s:?}"
            ));
        }
        s.parse()
            .map(Self::Window)
            .map_err(|_| format!("state retention window out of range: {s:?}"))
    }
}

/// Metadata name of the floor record written by the commit of `sequence`.
pub(crate) fn floor_key(sequence: u64) -> String {
    format!("engine/state-floor/{sequence:016x}")
}

/// The live floor and the pin registry, shared by the engine and every
/// snapshot handle it issued.
#[derive(Debug, Default)]
pub(crate) struct RetentionState {
    /// The floor answered to readers (may run ahead of `durable` after an
    /// open with a smaller window, until the next commit).
    floor: AtomicU64,
    /// The floor recorded with the durable head.
    durable: AtomicU64,
    pins: Mutex<BTreeMap<u64, usize>>,
}

impl RetentionState {
    pub(crate) fn new(durable: u64, floor: u64) -> Arc<Self> {
        Arc::new(Self {
            floor: AtomicU64::new(floor),
            durable: AtomicU64::new(durable),
            pins: Mutex::default(),
        })
    }
    pub(crate) fn floor(&self) -> u64 {
        self.floor.load(Ordering::Acquire)
    }
    pub(crate) fn durable(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }
    /// Publishes a committed floor. Both values only ever grow.
    pub(crate) fn committed(&self, floor: u64) {
        self.durable.fetch_max(floor, Ordering::AcqRel);
        self.floor.fetch_max(floor, Ordering::AcqRel);
    }
    pub(crate) fn pin(self: &Arc<Self>, sequence: u64) -> Pin {
        *self
            .pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(sequence)
            .or_default() += 1;
        Pin {
            state: self.clone(),
            sequence,
        }
    }
    pub(crate) fn lowest_pinned(&self) -> Option<u64> {
        self.pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .next()
            .copied()
    }
    pub(crate) fn pinned_handles(&self) -> usize {
        self.pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .sum()
    }
}

/// One registered snapshot handle; dropping it releases the pin.
pub(crate) struct Pin {
    state: Arc<RetentionState>,
    sequence: u64,
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pin({})", self.sequence)
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        let mut pins = self
            .state
            .pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = pins.get_mut(&self.sequence) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&self.sequence);
            }
        }
    }
}

/// Retention counters reported by [`DurableEngine::retention_status`](super::DurableEngine::retention_status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionStatus {
    /// The configured policy.
    pub retention: Retention,
    /// The lowest readable snapshot sequence; every earlier one is refused.
    pub floor: u64,
    /// The floor recorded with the durable head (at most `floor`).
    pub durable_floor: u64,
    /// The lowest sequence an open snapshot handle still reads, if any.
    pub lowest_pinned: Option<u64>,
    /// Open snapshot handles (each counted once, however often cloned).
    pub pinned_handles: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_archive_and_windows() {
        assert_eq!("archive".parse(), Ok(Retention::Archive));
        assert_eq!("0".parse(), Ok(Retention::Window(0)));
        assert_eq!("128".parse(), Ok(Retention::Window(128)));
        for bad in ["", "-1", "1k", "Archive", "99999999999999999999999"] {
            assert!(bad.parse::<Retention>().is_err(), "{bad}");
        }
        assert_eq!(Retention::Window(7).to_string(), "7");
        assert_eq!(Retention::Archive.to_string(), "archive");
    }

    #[test]
    fn floors_saturate_and_archive_keeps_everything() {
        assert_eq!(Retention::Archive.floor_at(1_000), 0);
        assert_eq!(Retention::Window(10).floor_at(3), 0);
        assert_eq!(Retention::Window(10).floor_at(30), 20);
        assert_eq!(Retention::Window(0).floor_at(30), 30);
    }

    #[test]
    fn pins_count_handles_and_release_on_drop() {
        let state = RetentionState::new(0, 0);
        assert_eq!(state.lowest_pinned(), None);
        let a = state.pin(5);
        let b = state.pin(3);
        let c = state.pin(3);
        assert_eq!(
            (state.lowest_pinned(), state.pinned_handles()),
            (Some(3), 3)
        );
        drop(b);
        assert_eq!(state.lowest_pinned(), Some(3));
        drop(c);
        assert_eq!(state.lowest_pinned(), Some(5));
        drop(a);
        assert_eq!((state.lowest_pinned(), state.pinned_handles()), (None, 0));
    }

    #[test]
    fn committed_floor_never_moves_backwards() {
        let state = RetentionState::new(4, 6);
        state.committed(5);
        assert_eq!((state.durable(), state.floor()), (5, 6));
        state.committed(2);
        assert_eq!((state.durable(), state.floor()), (5, 6));
        state.committed(9);
        assert_eq!((state.durable(), state.floor()), (9, 9));
    }
}
