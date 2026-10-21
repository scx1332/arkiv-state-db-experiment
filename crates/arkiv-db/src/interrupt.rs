//! Cooperative interruption of metered reads by their host.
//!
//! A host that serves reads beside block production bounds them by wall-clock
//! time and abandons them when the caller goes away. Neither is a metering
//! fact: charges, receipts and budgets never depend on time. An [`Interrupt`]
//! is a flag the host raises from any thread; a metered operation observes it
//! only when both hold:
//!
//! - its [`Budget`](crate::Budget) opted in with
//!   [`Budget::interruptible`](crate::Budget::interruptible), and
//! - it runs inside [`Interrupt::scope`] on the current thread.
//!
//! Such an operation checks the flag before each charge and, once it is
//! raised, stops at that charge as if its budget were exhausted
//! ([`Rejection::OutOfBudget`](crate::Rejection::OutOfBudget)), before the
//! work the charge covers. That receipt describes an abandoned read, so the
//! host checks [`Interrupt::is_raised`] and discards the result. Block
//! execution never opts in and never runs in a scope, so a raised flag cannot
//! change a block or its receipts.

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};

thread_local! {
    static ACTIVE: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// A host-owned stop flag for interruptible reads. Clones share the flag.
#[derive(Clone, Debug, Default)]
pub struct Interrupt(Arc<AtomicBool>);

impl Interrupt {
    /// A flag that is not raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks every interruptible read in a scope of this flag to stop at its
    /// next charge. Raising is permanent.
    pub fn raise(&self) {
        self.0.store(true, Relaxed);
    }

    /// Whether the flag was raised.
    pub fn is_raised(&self) -> bool {
        self.0.load(Relaxed)
    }

    /// Runs `f` on this thread with interruptible reads observing this flag.
    /// The previous scope, if any, is restored when `f` returns or unwinds.
    pub fn scope<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Restore(Option<Arc<AtomicBool>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let previous = self.0.take();
                ACTIVE.with(|active| *active.borrow_mut() = previous);
            }
        }
        let previous = ACTIVE.with(|active| active.borrow_mut().replace(self.0.clone()));
        let _restore = Restore(previous);
        f()
    }
}

/// The flag of the innermost scope on this thread, for a meter whose budget
/// opted in.
pub(crate) fn active() -> Option<Arc<AtomicBool>> {
    ACTIVE.with(|active| active.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_nest_and_restore_after_unwinding() {
        let outer = Interrupt::new();
        let inner = Interrupt::new();
        assert!(active().is_none());
        outer.scope(|| {
            assert!(Arc::ptr_eq(&active().unwrap(), &outer.0));
            inner.scope(|| assert!(Arc::ptr_eq(&active().unwrap(), &inner.0)));
            assert!(Arc::ptr_eq(&active().unwrap(), &outer.0));
            let unwound = std::panic::catch_unwind(|| inner.scope(|| panic!("read failed")));
            assert!(unwound.is_err());
            assert!(Arc::ptr_eq(&active().unwrap(), &outer.0));
        });
        assert!(active().is_none());
        // Another thread never sees this thread's scope.
        outer.scope(|| {
            std::thread::spawn(|| assert!(active().is_none()))
                .join()
                .unwrap()
        });
    }
}
