//! Lock access that outlives a panic in one holder.
//!
//! A poisoned `std` lock fails every later `lock()`, so one panicking request would take down
//! every later request that touches the same state. Two remedies, by what the lock guards:
//!
//! * [`Unpoisoned`]: state that changes in single steps (an insert, a remove, a whole-value
//!   replace), so it's consistent whenever a guard is released, even by unwinding. The database
//!   registry, the user catalog, the Studio sidecar and the task list are like that.
//! * [`CacheLock`]: caches, where a panic mid-update could leave internal bookkeeping half done.
//!   A poisoned cache starts over empty; it only ever held work that can be redone.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Guards taken whether or not the lock is poisoned; see the module docs for when that's right.
pub(crate) trait Unpoisoned<T: ?Sized> {
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T>;
    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> Unpoisoned<T> for RwLock<T> {
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// [`Unpoisoned`] for a `Mutex`.
pub(crate) trait LockUnpoisoned<T: ?Sized> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockUnpoisoned<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A cache behind a lock: if a panic poisoned it, it's emptied and the poison cleared.
pub(crate) trait CacheLock<T: Default> {
    fn cache_read(&self) -> RwLockReadGuard<'_, T>;
    fn cache_write(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: Default> CacheLock<T> for RwLock<T> {
    fn cache_write(&self) -> RwLockWriteGuard<'_, T> {
        match self.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                *guard = T::default();
                self.clear_poison();
                guard
            }
        }
    }

    fn cache_read(&self) -> RwLockReadGuard<'_, T> {
        loop {
            match self.read() {
                Ok(guard) => return guard,
                // the failed read still holds a guard: let it go before taking the write lock
                Err(poisoned) => drop(poisoned),
            }
            // empty it under the write lock, then read again
            drop(self.cache_write());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn poison<T: Send + Sync + 'static>(lock: &'static RwLock<T>) {
        let _ = std::thread::spawn(move || {
            let _g = lock.write().unwrap();
            panic!("poison");
        })
        .join();
        assert!(lock.is_poisoned());
    }

    #[test]
    fn state_survives_a_poisoned_lock() {
        static MAP: RwLock<Vec<u32>> = RwLock::new(Vec::new());
        MAP.write().unwrap().push(1);
        poison(&MAP);
        assert_eq!(*MAP.read_unpoisoned(), vec![1]);
        MAP.write_unpoisoned().push(2);
        assert_eq!(*MAP.read_unpoisoned(), vec![1, 2]);
    }

    #[test]
    fn a_poisoned_cache_starts_over() {
        static CACHE: std::sync::LazyLock<RwLock<HashMap<u32, u32>>> =
            std::sync::LazyLock::new(|| RwLock::new(HashMap::from([(1, 1)])));
        poison(&CACHE);
        assert!(CACHE.cache_read().is_empty());
        assert!(!CACHE.is_poisoned());
        CACHE.cache_write().insert(2, 2);
        assert_eq!(CACHE.cache_read().get(&2), Some(&2));
    }
}
