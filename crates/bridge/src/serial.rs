//! A queue of jobs run one after another on a background thread of its own
//! (#176): the downloads of cloud-only databases, the builds of heads files
//! and the header index builds of PGN files each run in one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::sync::{lock, unpoisoned};

type Job = Box<dyn FnOnce() + Send>;

/// Runs jobs one after another on a background thread, which starts when a
/// job arrives and ends when none is left.
pub struct Serial {
    /// The jobs waiting, and whether a thread runs them.
    queue: Mutex<(VecDeque<Job>, bool)>,
    /// Told when the thread ends, the queue empty ([`Serial::wait_idle`]).
    idle: Condvar,
    /// Starting a thread fails, for tests.
    refuse: AtomicBool,
    /// What the jobs do, for the thread's name and messages.
    label: &'static str,
}

impl Serial {
    /// A queue whose thread and messages are named `label`.
    pub fn labelled(label: &'static str) -> Serial {
        Serial { queue: Mutex::default(), idle: Condvar::new(), refuse: AtomicBool::new(false), label }
    }

    /// What the jobs do, as the thread is named.
    pub fn label(&self) -> &'static str {
        self.label
    }

    /// Waits until no job waits or runs, until `deadline`: whether none does
    /// (#236).
    pub fn wait_idle(&self, deadline: Instant) -> bool {
        let mut queue = lock(&self.queue);
        while queue.1 || !queue.0.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return false;
            };
            queue = unpoisoned(self.idle.wait_timeout(queue, left)).0;
        }
        true
    }

    /// Queues `job`, starting the thread when none runs. When the thread
    /// cannot start, `job` is dropped unrun and `false` returned: nothing is
    /// left waiting for a thread that does not exist.
    pub fn submit(self: &Arc<Self>, job: Job) -> bool {
        let mut queue = lock(&self.queue);
        queue.0.push_back(job);
        if queue.1 {
            return true;
        }
        let me = Arc::clone(self);
        let started = if self.refuse.load(Ordering::Relaxed) {
            Err(std::io::Error::other("refused for a test"))
        } else {
            std::thread::Builder::new()
                .name(self.label.into())
                .stack_size(crate::THREAD_STACK)
                .spawn(move || me.run())
                .map(drop)
        };
        match started {
            Ok(()) => {
                queue.1 = true;
                true
            }
            Err(e) => {
                crate::log!("cannot start a {} thread: {e}", self.label);
                // No thread runs, so this is the only job queued.
                let job = queue.0.pop_back();
                drop(queue);
                drop(job);
                false
            }
        }
    }

    /// Makes starting the thread fail while `refuse` holds. Tests use it.
    pub fn refuse_starts(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::Relaxed);
    }

    fn run(&self) {
        // Should the thread end by a panic all the same, the jobs still
        // waiting are dropped unrun and the next job submitted starts a
        // new thread.
        let _exit = Exit(self);
        loop {
            let job = {
                let mut queue = lock(&self.queue);
                match queue.0.pop_front() {
                    Some(job) => job,
                    None => {
                        queue.1 = false;
                        self.idle.notify_all();
                        return;
                    }
                }
            };
            // A panicking job must not end the thread.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                crate::log!("a {} job failed with a bug", self.label);
            }
        }
    }
}

struct Exit<'a>(&'a Serial);

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let jobs = {
                let mut queue = lock(&self.0.queue);
                queue.1 = false;
                self.0.idle.notify_all();
                std::mem::take(&mut queue.0)
            };
            drop(jobs);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;

    /// A thread that cannot start leaves no job waiting, and the next job
    /// submitted starts one.
    #[test]
    fn a_failed_start_leaves_nothing_waiting() {
        let serial = Arc::new(Serial::labelled("test"));
        let ran = Arc::new(AtomicU64::new(0));
        let job = |ran: &Arc<AtomicU64>| -> Job {
            let ran = Arc::clone(ran);
            Box::new(move || {
                ran.fetch_add(1, Ordering::SeqCst);
            })
        };
        serial.refuse_starts(true);
        assert!(!serial.submit(job(&ran)));
        assert!(!serial.submit(job(&ran)));
        assert!(lock(&serial.queue).0.is_empty());
        serial.refuse_starts(false);
        assert!(serial.submit(job(&ran)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ran.load(Ordering::SeqCst) < 1 || lock(&serial.queue).1 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// The queue is idle only once no job waits or runs (#236): a wait ends
    /// with `false` at its deadline while a job is held, and with `true` once
    /// every job queued has run, when the thread ends rather than at its
    /// deadline.
    #[test]
    fn waits_until_no_job_waits_or_runs() {
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(300);
        let serial = Arc::new(Serial::labelled("test"));
        assert!(serial.wait_idle(Instant::now()), "idle before any job");
        let (release, held) = std::sync::mpsc::channel::<()>();
        let ran = Arc::new(AtomicU64::new(0));
        let first = Arc::clone(&ran);
        assert!(serial.submit(Box::new(move || {
            let _ = held.recv();
            first.fetch_add(1, Ordering::SeqCst);
        })));
        let second = Arc::clone(&ran);
        assert!(serial.submit(Box::new(move || {
            second.fetch_add(1, Ordering::SeqCst);
        })));
        assert!(!serial.wait_idle(Instant::now() + std::time::Duration::from_millis(50)), "a job runs");
        release.send(()).unwrap();
        let waited = Instant::now();
        assert!(serial.wait_idle(waited + 2 * LIMIT));
        assert!(waited.elapsed() < LIMIT, "woken when idle, not at the deadline");
        assert_eq!(ran.load(Ordering::SeqCst), 2, "both ran before it was idle");
    }
}
