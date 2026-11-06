//! The background work of one catalog (#236): the jobs of its download, heads
//! and PGN queues, its index builds, the writers of its names files and the
//! keeper's looks, each counted from when it is queued or begins until it
//! ends, however it ends, so that [`crate::catalog::Catalog::settle`] waits
//! until none of it waits or runs.
//!
//! One count serves them all, because work of one kind queues work of
//! another: a download may queue a build, a build the header index of a PGN
//! file. Such work is queued, and counted, before the work that queues it
//! ends, so the count never falls to nothing while work passes from one queue
//! to another. Once it is nothing, nothing of the catalog waits or runs, and
//! only a request or the keeper can raise it again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::sync::{lock, unpoisoned};

#[derive(Default)]
pub struct Activity {
    /// Each kind of work counted so far, in the order the kinds first began,
    /// with how much of it waits or runs now.
    counts: Mutex<Vec<(&'static str, usize)>>,
    /// Told when no work waits or runs any more.
    idle: Condvar,
    /// How many times work began or ended (#348).
    changes: AtomicU64,
}

/// Work of one kind, counted until the guard is dropped.
pub struct Active {
    activity: Arc<Activity>,
    kind: &'static str,
}

impl Activity {
    /// Counts work of `kind` until the guard is dropped.
    pub fn begin(self: &Arc<Self>, kind: &'static str) -> Active {
        let mut counts = lock(&self.counts);
        match counts.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += 1,
            None => counts.push((kind, 1)),
        }
        self.changes.fetch_add(1, Ordering::Relaxed);
        Active { activity: Arc::clone(self), kind }
    }

    /// How many times work began or ended so far (#348): a reader that sees
    /// it change knows that the work it shows may have, without asking each
    /// queue.
    pub fn changes(&self) -> u64 {
        self.changes.load(Ordering::Relaxed)
    }

    /// Waits until no work waits or runs, until `deadline`: the kinds of the
    /// work that still does then, in the order they first began.
    pub fn wait_idle(&self, deadline: Instant) -> Result<(), Vec<&'static str>> {
        let mut counts = lock(&self.counts);
        while counts.iter().any(|(_, n)| *n > 0) {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return Err(counts.iter().filter(|(_, n)| *n > 0).map(|(k, _)| *k).collect());
            };
            counts = unpoisoned(self.idle.wait_timeout(counts, left)).0;
        }
        Ok(())
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        let mut counts = lock(&self.activity.counts);
        if let Some((_, n)) = counts.iter_mut().find(|(k, _)| *k == self.kind) {
            *n = n.saturating_sub(1);
        }
        self.activity.changes.fetch_add(1, Ordering::Relaxed);
        if counts.iter().all(|(_, n)| *n == 0) {
            self.activity.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Each beginning and each end of work changes the count of changes, of
    /// whichever kind, and nothing else does (#348).
    #[test]
    fn each_beginning_and_end_is_a_change() {
        let activity = Arc::new(Activity::default());
        assert_eq!(activity.changes(), 0);
        let a = activity.begin("a");
        let b = activity.begin("b");
        assert_eq!(activity.changes(), 2);
        assert_eq!(activity.wait_idle(Instant::now()), Err(vec!["a", "b"]));
        assert_eq!(activity.changes(), 2, "a wait changes nothing");
        drop(a);
        assert_eq!(activity.changes(), 3);
        drop(b);
        assert_eq!(activity.changes(), 4);
    }

    /// Work counts from its beginning until its guard drops, of whichever
    /// kind: a wait ends at its deadline naming the kinds still counted, and
    /// once the last guard drops, then rather than at the deadline.
    #[test]
    fn waits_until_no_work_is_counted() {
        const LIMIT: Duration = Duration::from_secs(300);
        let activity = Arc::new(Activity::default());
        assert_eq!(activity.wait_idle(Instant::now()), Ok(()), "idle before any work");
        let (a, b, again) = (activity.begin("a"), activity.begin("b"), activity.begin("a"));
        drop(a);
        assert_eq!(activity.wait_idle(Instant::now() + Duration::from_millis(20)), Err(vec!["a", "b"]));
        // Work that passes to another kind begins it before it ends: the
        // count never falls to nothing in between.
        let passed = activity.begin("c");
        drop(b);
        drop(again);
        assert_eq!(activity.wait_idle(Instant::now()), Err(vec!["c"]));
        let (release, held) = std::sync::mpsc::channel::<()>();
        let ends = std::thread::spawn(move || {
            let _ = held.recv();
            drop(passed);
        });
        release.send(()).unwrap();
        let waited = Instant::now();
        assert_eq!(activity.wait_idle(waited + 2 * LIMIT), Ok(()));
        assert!(waited.elapsed() < LIMIT, "woken when the work ended, not at the deadline");
        ends.join().unwrap();
    }
}
