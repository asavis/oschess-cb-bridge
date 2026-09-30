//! Locks that outlive a panic (#176). A thread that panics while it holds a
//! lock poisons it. The bridge catches such panics, in a job, a request or a
//! build, and goes on serving, so it takes every lock past the poison rather
//! than failing each later request that needs the lock.

use std::sync::{LockResult, Mutex, MutexGuard, PoisonError};

/// Locks `m`, whether or not a thread panicked while it held it.
pub fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    unpoisoned(m.lock())
}

/// What a lock's operation gives, whether or not a thread panicked while it
/// held the lock: the guard a condition variable's wait gives back, or the
/// value a mutex that is no longer shared holds.
pub fn unpoisoned<T>(result: LockResult<T>) -> T {
    result.unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Condvar};
    use std::time::Duration;

    use super::*;

    /// A mutex a thread panicked while holding is poisoned; it is locked all
    /// the same, with what the thread left in it, and waited on, and given up.
    #[test]
    fn a_poisoned_mutex_is_locked_all_the_same() {
        let m = Arc::new(Mutex::new(1));
        let held = Arc::clone(&m);
        let panicked = std::thread::spawn(move || {
            let mut guard = held.lock().unwrap();
            *guard = 2;
            panic!("a panic while the lock is held");
        })
        .join();
        assert!(panicked.is_err());
        assert!(m.is_poisoned());
        *lock(&m) += 1;
        assert_eq!(*lock(&m), 3);

        let changed = Condvar::new();
        let (guard, waited) = unpoisoned(changed.wait_timeout(lock(&m), Duration::from_millis(1)));
        assert!(waited.timed_out());
        assert_eq!(*guard, 3);
        drop(guard);
        let Ok(m) = Arc::try_unwrap(m) else { panic!("the thread still holds the mutex") };
        assert_eq!(unpoisoned(m.into_inner()), 3);
    }
}
