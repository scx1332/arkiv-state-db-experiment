//! Fixed-cardinality, allocation-free operation timing counters.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

/// Number of finite histogram bounds.
pub const DURATION_BUCKETS: usize = 26;

/// Inclusive histogram bounds, in nanoseconds. The count includes overflow.
///
/// 1-2.5-5 steps from 1 us to 10 s, with 7.5, 15, 20, 30 and 75 ms added
/// where durable commits fall (a commit is one or two `fsync`s: a few
/// milliseconds to a few tens). The twelve bounds of the first version (1 us,
/// 10 us, 100 us, 500 us, 1 ms, 5 ms, 10 ms, 50 ms, 100 ms, 500 ms, 1 s,
/// 10 s) are all kept, so queries written against them still work.
pub const DURATION_BUCKETS_NANOSECONDS: [u64; DURATION_BUCKETS] = [
    1_000,
    2_500,
    5_000,
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    7_500_000,
    10_000_000,
    15_000_000,
    20_000_000,
    30_000_000,
    50_000_000,
    75_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
    10_000_000_000,
];

/// Cumulative counters since this store/probe was created, including failures.
/// Concurrent snapshots are approximate: fields are loaded independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationTimingSnapshot {
    /// Fixed operation name; never includes keys or namespaces.
    pub operation: &'static str,
    /// Number of completed calls, including those above the largest bound.
    pub count: u64,
    /// Calls returning failure (and unwound calls).
    pub errors: u64,
    /// Total wall-clock elapsed time, including waiting for locks.
    pub total_nanoseconds: u64,
    /// Counts at or below each [`DURATION_BUCKETS_NANOSECONDS`] bound.
    pub cumulative_buckets: [u64; DURATION_BUCKETS],
}

#[derive(Debug, Default)]
pub(crate) struct OperationTiming {
    errors: AtomicU64,
    total_nanoseconds: AtomicU64,
    // Disjoint buckets keep the hot path to three atomic additions; the
    // last one counts calls above the largest bound.
    buckets: [AtomicU64; DURATION_BUCKETS + 1],
}

impl OperationTiming {
    pub(crate) fn start(&self) -> TimingGuard<'_> {
        TimingGuard {
            metric: self,
            started: Instant::now(),
            success: false,
        }
    }

    pub(crate) fn snapshot(&self, operation: &'static str) -> OperationTimingSnapshot {
        let mut count = 0;
        let cumulative_buckets = std::array::from_fn(|index| {
            count += self.buckets[index].load(Ordering::Relaxed);
            count
        });
        count += self.buckets[DURATION_BUCKETS].load(Ordering::Relaxed);
        OperationTimingSnapshot {
            operation,
            count,
            errors: self.errors.load(Ordering::Relaxed),
            total_nanoseconds: self.total_nanoseconds.load(Ordering::Relaxed),
            cumulative_buckets,
        }
    }
}

pub(crate) struct TimingGuard<'a> {
    metric: &'a OperationTiming,
    started: Instant,
    success: bool,
}

impl TimingGuard<'_> {
    pub(crate) fn success(&mut self) {
        self.success = true;
    }
}

impl Drop for TimingGuard<'_> {
    fn drop(&mut self) {
        let nanos = self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let index = DURATION_BUCKETS_NANOSECONDS.partition_point(|bound| *bound < nanos);
        self.metric.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.metric
            .total_nanoseconds
            .fetch_add(nanos, Ordering::Relaxed);
        if !self.success {
            self.metric.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_failure_and_unwind_are_counted() {
        let metric = OperationTiming::default();
        {
            let mut guard = metric.start();
            guard.success();
        }
        {
            let _guard = metric.start();
        }
        let _ = std::panic::catch_unwind(|| {
            let _guard = metric.start();
            panic!("test");
        });
        let snapshot = metric.snapshot("test");
        assert_eq!(snapshot.count, 3);
        assert_eq!(snapshot.errors, 2);
        assert!(
            snapshot
                .cumulative_buckets
                .windows(2)
                .all(|pair| pair[0] <= pair[1])
        );
        assert!(snapshot.cumulative_buckets[DURATION_BUCKETS - 1] <= snapshot.count);
    }

    #[test]
    fn bounds_ascend_and_keep_the_first_twelve() {
        assert!(
            DURATION_BUCKETS_NANOSECONDS
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        for bound in [
            1_000,
            10_000,
            100_000,
            500_000,
            1_000_000,
            5_000_000,
            10_000_000,
            50_000_000,
            100_000_000,
            500_000_000,
            1_000_000_000,
            10_000_000_000,
        ] {
            assert!(DURATION_BUCKETS_NANOSECONDS.contains(&bound), "{bound}");
        }
    }
}
