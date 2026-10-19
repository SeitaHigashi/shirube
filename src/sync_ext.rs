//! Poison-tolerant accessors for `std::sync` locks.
//!
//! `RwLock::read()` / `Mutex::lock()` return `Err(PoisonError)` once *any*
//! thread has panicked while holding the guard. Calling `.unwrap()` on that
//! result turns a single unrelated panic into a cascading failure: every
//! subsequent access to the same lock panics too, permanently bricking the
//! component.
//!
//! NOTE: The data this bot guards (mock balances, rate-limiter tokens) is
//! plain state with no cross-field invariant that a mid-update panic could
//! leave half-applied in a dangerous way. Recovering the guard via
//! `PoisonError::into_inner()` is therefore preferable to propagating the
//! panic — a degraded value is better than a dead trading loop.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Poison-tolerant `RwLock` accessors.
pub trait RwLockExt<T: ?Sized> {
    /// Acquire a read guard, recovering the inner value if the lock is poisoned.
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T>;
    /// Acquire a write guard, recovering the inner value if the lock is poisoned.
    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    fn read_or_recover(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_or_recover(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Poison-tolerant `Mutex` accessor.
pub trait MutexExt<T: ?Sized> {
    /// Acquire the lock, recovering the inner value if the lock is poisoned.
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn rwlock_recovers_after_poisoning() {
        let lock = Arc::new(RwLock::new(41));
        let clone = Arc::clone(&lock);
        // Poison the lock by panicking while the write guard is held.
        let _ = std::thread::spawn(move || {
            let _guard = clone.write().unwrap();
            panic!("intentional panic to poison the lock");
        })
        .join();

        assert!(lock.read().is_err(), "lock should be poisoned");
        *lock.write_or_recover() += 1;
        assert_eq!(*lock.read_or_recover(), 42);
    }

    #[test]
    fn mutex_recovers_after_poisoning() {
        let lock = Arc::new(Mutex::new(1));
        let clone = Arc::clone(&lock);
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock().unwrap();
            panic!("intentional panic to poison the lock");
        })
        .join();

        assert!(lock.lock().is_err(), "lock should be poisoned");
        *lock.lock_or_recover() += 1;
        assert_eq!(*lock.lock_or_recover(), 2);
    }
}
