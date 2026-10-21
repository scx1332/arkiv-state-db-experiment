//! Detached reads: heavy full-node reads evaluated outside the node lock.
//!
//! The node lock serializes production, admission and every other request.
//! A read whose cost grows with the state (`arkiv_query`,
//! `arkiv_getEntityCount`, equality and range proofs, the state tree
//! explorer) holds it only to check heights and capture what it needs: a
//! pinned [`arkiv_db::Snapshot`] plus identity and limits. It then
//! evaluates on a blocking thread after releasing the lock, so status,
//! light reads and block production proceed meanwhile.
//!
//! The captured snapshot is a consistent view while blocks are produced:
//! it fixes the state header (every map root) of a published height, and
//! every trie node below those roots is a content-addressed, hash-verified,
//! committed object that later commits never change or delete. Each object
//! read is its own short redb read transaction; the decoded-node cache is
//! keyed by object id and shared by all snapshots. The read pins its height
//! against retention for as long as it runs. A detached read therefore
//! returns what the same read would have returned under the lock at the
//! moment it was captured.
//!
//! Reads never open a write transaction. redb itself may write from a read:
//! after each durable commit it frees superseded pages in a non-durable
//! epilogue commit whose few pages stay in its write buffer, and the next
//! page-cache miss under cache pressure writes them to the file (best
//! effort, no sync, no header change). That is why the first heavy read
//! after a commit changes `node.redb`'s mtime; durable state and roots are
//! untouched, and redb runs this concurrently with its writer by design.
//!
//! Detached reads are bounded twice:
//!
//! - **Concurrency.** At most [`ReadLimits::concurrency`] run at once;
//!   another is refused immediately with `ServerBusy` (HTTP 429), before it
//!   touches the node lock. Waiting would only queue work behind reads that
//!   may run until their deadline.
//! - **Deadline and departure.** A read runs at most
//!   [`ReadLimits::deadline`]; then it is answered `ReadDeadlineExceeded`
//!   (HTTP 503). If the caller goes away first, the read is abandoned. In
//!   both cases its [`Interrupt`] is raised; its metered walk (an
//!   interruptible budget) stops at the next charge and releases its
//!   permit. Metering, budgets and every successful response are unchanged.
use super::{Error, Result};
use arkiv_db::Interrupt;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The error of a detached read that ran past its deadline.
pub const DEADLINE_EXCEEDED: &str = "ReadDeadlineExceeded";

/// Bounds of detached reads, set by `--read-concurrency` and
/// `--read-deadline-secs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    /// Detached reads evaluated at once (1 to [`Self::MAX_CONCURRENCY`]).
    pub concurrency: usize,
    /// Wall-clock limit of one detached read (1 s to [`Self::MAX_DEADLINE`]).
    pub deadline: Duration,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            concurrency: 4,
            deadline: Duration::from_secs(30),
        }
    }
}

impl ReadLimits {
    pub const MAX_CONCURRENCY: usize = 64;
    pub const MAX_DEADLINE: Duration = Duration::from_secs(3600);

    pub fn validate(&self) -> Result<()> {
        if !(1..=Self::MAX_CONCURRENCY).contains(&self.concurrency)
            || !(Duration::from_secs(1)..=Self::MAX_DEADLINE).contains(&self.deadline)
        {
            return Err(Error("InvalidRequest"));
        }
        Ok(())
    }
}

/// The admission gate of detached reads.
pub(super) struct Reads {
    limits: ReadLimits,
    permits: Arc<Semaphore>,
}

impl Reads {
    pub(super) fn new(limits: ReadLimits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits,
            permits: Arc::new(Semaphore::new(limits.concurrency)),
        })
    }

    pub(super) fn limits(&self) -> ReadLimits {
        self.limits
    }

    /// Detached reads holding a permit, including abandoned ones that have
    /// not reached their next charge yet.
    pub(super) fn in_flight(&self) -> usize {
        self.limits.concurrency - self.permits.available_permits()
    }

    /// Admits one detached read, or refuses it at once with `ServerBusy`.
    pub(super) fn admit(&self) -> Result<Admitted> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error("ServerBusy"))?;
        Ok(Admitted {
            permit,
            deadline: self.limits.deadline,
        })
    }
}

/// One admitted detached read.
pub(super) struct Admitted {
    permit: OwnedSemaphorePermit,
    deadline: Duration,
}

/// Raises the read's interrupt when the awaiting request goes away: the
/// client disconnected, the deadline passed or the read finished (then the
/// flag is no longer observed).
struct RaiseOnDrop(Interrupt);
impl Drop for RaiseOnDrop {
    fn drop(&mut self) {
        self.0.raise();
    }
}

impl Admitted {
    /// Runs `job` on a blocking thread inside an interrupt scope, holding
    /// the permit until the job returns. `job` captures under the node lock,
    /// releases it and evaluates; its metered reads must use interruptible
    /// budgets. `label` names the read in the log line of an interrupted one.
    pub(super) async fn run<T: Send + 'static>(
        self,
        label: &str,
        job: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let interrupt = Interrupt::new();
        let _raise = RaiseOnDrop(interrupt.clone());
        let label = label.to_owned();
        let permit = self.permit;
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let started = Instant::now();
            let result = interrupt.scope(job);
            if interrupt.is_raised() {
                eprintln!(
                    "signed node: {label} read interrupted after {} ms",
                    started.elapsed().as_millis()
                );
                return Err(Error(DEADLINE_EXCEEDED));
            }
            result
        });
        match tokio::time::timeout(self.deadline, worker).await {
            Ok(joined) => joined.unwrap_or(Err(Error("StorageUnavailable"))),
            // `_raise` stops the abandoned job; its permit returns when it does.
            Err(_) => Err(Error(DEADLINE_EXCEEDED)),
        }
    }
}
