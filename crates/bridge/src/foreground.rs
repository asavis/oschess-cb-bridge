//! The work a user waits for (#149): the answers about a database's games
//! and positions while they run ([`crate::api`]: lists, searches, sorts, a
//! game's PGN, suggestions and explorer answers), which the threads of a
//! background build give way to between their batches
//! ([`crate::explorer::runs::Progress::give_way`]).
//!
//! Foreground work raises a process-wide count while it runs, which the
//! build's threads read between batches; a thread that finds it raised waits
//! on a condition variable until it falls to nothing, or until its patience
//! runs out, when it goes on with one batch whatever runs, so that a build
//! still ends while foreground work never does. A thread that waits within
//! foreground work for a worker, which a build may hold, sets its work
//! aside meanwhile ([`aside`]), so that the build it waits for does not
//! give way to it.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The foreground work running, and the threads waiting for it to end.
struct Foreground {
    running: AtomicUsize,
    /// Held to look at `running` before a wait, and to wake the waiting.
    lock: Mutex<()>,
    changed: Condvar,
}

impl Foreground {
    const fn new() -> Foreground {
        Foreground { running: AtomicUsize::new(0), lock: Mutex::new(()), changed: Condvar::new() }
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst) > 0
    }

    fn raise(&self, work: usize) {
        self.running.fetch_add(work, Ordering::SeqCst);
    }

    /// Ends `work` of the work running; the waiting look again once none
    /// runs.
    fn lower(&self, work: usize) {
        if self.running.fetch_sub(work, Ordering::SeqCst) == work {
            self.wake();
        }
    }

    fn wait(&self, patience: Duration, enough: &dyn Fn() -> bool) {
        let deadline = Instant::now().checked_add(patience);
        let mut held = self.held();
        while self.running() && !enough() {
            let left = deadline.map_or(patience, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return;
            }
            held = self.changed.wait_timeout(held, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn wake(&self) {
        let _held = self.held();
        self.changed.notify_all();
    }

    fn held(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }
}

static FOREGROUND: Foreground = Foreground::new();

thread_local! {
    /// The foreground work the calling thread runs.
    static OWN: Cell<usize> = const { Cell::new(0) };
}

/// Foreground work running on the thread that began it, until dropped there.
pub struct Working {
    _thread: PhantomData<*const ()>,
}

/// Begins foreground work on the calling thread, which runs until the guard
/// is dropped.
pub fn begin() -> Working {
    FOREGROUND.raise(1);
    OWN.set(OWN.get() + 1);
    Working { _thread: PhantomData }
}

impl Drop for Working {
    fn drop(&mut self) {
        OWN.set(OWN.get().saturating_sub(1));
        FOREGROUND.lower(1);
    }
}

/// Whether foreground work runs now.
pub fn running() -> bool {
    FOREGROUND.running()
}

/// Waits while foreground work runs, for at most `patience`, and no longer
/// once `enough` holds, which is asked again whenever the waiting threads are
/// woken ([`wake`]).
pub fn wait(patience: Duration, enough: &dyn Fn() -> bool) {
    FOREGROUND.wait(patience, enough);
}

/// Wakes the threads waiting for foreground work to end, so that they ask
/// again whether they have waited enough: a build asked to stop, or made a
/// requested one, waits no longer.
pub fn wake() {
    FOREGROUND.wake();
}

/// Runs `waiting`, in which the calling thread waits for something a
/// background build may hold, such as a worker: the foreground work the
/// thread runs does not count meanwhile, so that the build does not give way
/// to it and hold what it waits for all the longer.
pub fn aside<T>(waiting: impl FnOnce() -> T) -> T {
    /// Counts the work again, even when `waiting` panics.
    struct Back(usize);
    impl Drop for Back {
        fn drop(&mut self) {
            FOREGROUND.raise(self.0);
        }
    }
    let own = OWN.get();
    if own == 0 {
        return waiting();
    }
    FOREGROUND.lower(own);
    let _back = Back(own);
    waiting()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    /// Held by the tests that begin foreground work, which count it.
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

    /// A thread waiting while foreground work runs goes on once it ends,
    /// once its patience runs out, or once woken with enough waited; and
    /// waits not at all while none runs.
    #[test]
    fn a_thread_waits_while_foreground_work_runs() {
        let fg = Arc::new(Foreground::new());
        let long = Duration::from_secs(60);
        let started = Instant::now();
        fg.wait(long, &|| false);
        assert!(started.elapsed() < Duration::from_secs(1), "nothing ran");

        // Until the work ends.
        fg.raise(2);
        let waiter = {
            let fg = Arc::clone(&fg);
            std::thread::spawn(move || fg.wait(long, &|| false))
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "went on while work ran");
        fg.lower(1);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished(), "went on while work still ran");
        fg.lower(1);
        waiter.join().unwrap();

        // Until its patience runs out.
        fg.raise(1);
        let started = Instant::now();
        fg.wait(Duration::from_millis(100), &|| false);
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(100) && waited < Duration::from_secs(30), "{waited:?}");

        // Until woken with enough waited.
        let enough = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (fg, enough) = (Arc::clone(&fg), Arc::clone(&enough));
            std::thread::spawn(move || fg.wait(long, &|| enough.load(Ordering::SeqCst)))
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiter.is_finished());
        enough.store(true, Ordering::SeqCst);
        fg.wake();
        waiter.join().unwrap();
        fg.lower(1);
        assert!(!fg.running());
    }

    /// Work begun on a thread counts until its guard is dropped, and not
    /// while the thread waits aside, even when the wait panics.
    #[test]
    fn work_set_aside_does_not_count() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::thread::spawn(|| {
            assert_eq!(aside(|| OWN.get()), 0);
            let first = begin();
            let second = begin();
            assert_eq!(OWN.get(), 2);
            let before = FOREGROUND.running.load(Ordering::SeqCst);
            assert!(before >= 2);
            let during = aside(|| FOREGROUND.running.load(Ordering::SeqCst));
            assert_eq!(during + 2, before, "the thread's own work is set aside");
            assert_eq!(FOREGROUND.running.load(Ordering::SeqCst), before);
            let panicked = std::panic::catch_unwind(|| aside::<()>(|| panic!("in the wait")));
            assert!(panicked.is_err());
            assert_eq!(FOREGROUND.running.load(Ordering::SeqCst), before, "counted again");
            drop((first, second));
            assert_eq!(OWN.get(), 0);
        })
        .join()
        .unwrap();
    }
}
