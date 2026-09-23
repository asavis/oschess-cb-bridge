//! Worker threads for passes over a database, bounded for the whole process:
//! however many requests search at once, no more than [`threads`] workers run,
//! and their batch buffers are reserved in the search budget before they are
//! allocated.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::SearchError;
use super::memory::{Cancel, Hold, Refused, budget};

/// How long a pass waits for a free worker before it is answered `busy`.
pub const WAIT: Duration = Duration::from_secs(5);
/// How often a waiting pass looks whether it was superseded.
const RECHECK: Duration = Duration::from_millis(50);

/// Workers for all passes together: `OSCHESS_BRIDGE_THREADS` when set (1 to
/// 64), else the machine's cores, at most 16.
pub fn threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        let set = std::env::var("OSCHESS_BRIDGE_THREADS").ok().and_then(|v| v.trim().parse::<usize>().ok());
        match set {
            Some(n) => n.clamp(1, 64),
            None => std::thread::available_parallelism().map_or(1, |n| n.get()).min(16),
        }
    })
}

/// Workers taken by running passes.
static TAKEN: Mutex<usize> = Mutex::new(0);
static RETURNED: Condvar = Condvar::new();

/// Workers a pass holds, returned when dropped.
struct Slots(usize);

impl Slots {
    /// Returns the workers above `count`.
    fn shrink(&mut self, count: usize) {
        if count < self.0 {
            *TAKEN.lock().unwrap_or_else(|e| e.into_inner()) -= self.0 - count;
            self.0 = count;
            RETURNED.notify_all();
        }
    }
}

impl Drop for Slots {
    fn drop(&mut self) {
        *TAKEN.lock().unwrap_or_else(|e| e.into_inner()) -= self.0;
        RETURNED.notify_all();
    }
}

/// Up to `want` workers, at least one: as many as are free, after waiting up
/// to [`WAIT`] for the first.
fn acquire(want: usize, cancel: &Cancel) -> Result<Slots, SearchError> {
    let deadline = Instant::now() + WAIT;
    let mut taken = TAKEN.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let free = threads().saturating_sub(*taken);
        if free > 0 {
            let n = want.clamp(1, free);
            *taken += n;
            return Ok(Slots(n));
        }
        if cancel.is_cancelled() {
            return Err(SearchError::Superseded);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(SearchError::Busy);
        }
        taken = RETURNED.wait_timeout(taken, left.min(RECHECK)).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// Workers taken now, for tests and diagnostics.
pub fn taken() -> usize {
    *TAKEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a worker is given: its number and the number of workers, a flag that
/// any worker raises when it fails and every worker checks between batches,
/// and room for its batch buffer, already reserved.
pub struct Worker<'a> {
    pub index: usize,
    pub count: usize,
    pub stop: &'a AtomicBool,
    pub workspace: usize,
}

impl Worker<'_> {
    /// Whether another worker failed, so this one should stop.
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// A buffer of `workspace` zero bytes, allocated fallibly.
    pub fn buffer(&self) -> Result<Vec<u8>, Refused> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(self.workspace).map_err(|_| Refused::Busy)?;
        buf.resize(self.workspace, 0);
        Ok(buf)
    }
}

/// Runs `task` on up to `want` workers, each with `workspace` bytes of buffer
/// reserved in the budget, and returns their results in worker order. The
/// buffers of the workers it asks for take at most half the budget, so the
/// other half stays for what the workers build, and with little budget left it
/// runs on fewer, down to one. When not even one buffer fits now, or a worker
/// cannot be started, it answers `Busy`; a buffer larger than the whole budget
/// is `TooLarge`. The first failure stops the other workers.
pub fn run<T: Send>(
    want: usize,
    workspace: usize,
    cancel: &Cancel,
    task: impl Fn(&Worker<'_>) -> Result<T, SearchError> + Sync,
) -> Result<Vec<T>, SearchError> {
    let fit = (budget() / 2).checked_div(workspace).unwrap_or(usize::MAX).max(1);
    let mut slots = acquire(want.min(fit), cancel)?;
    let _buffers = loop {
        match Hold::reserve(slots.0.checked_mul(workspace).ok_or(Refused::TooLarge)?) {
            Ok(hold) => break hold,
            Err(Refused::Busy | Refused::TooLarge) if slots.0 > 1 => {
                let fewer = slots.0 / 2;
                slots.shrink(fewer);
            }
            Err(refused) => return Err(refused.into()),
        }
    };
    let count = slots.0;
    let stop = AtomicBool::new(false);
    let task = &task;
    let results: Vec<Result<T, SearchError>> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        if handles.try_reserve_exact(count).is_err() {
            return vec![Err(SearchError::Busy)];
        }
        for index in 0..count {
            let worker = Worker { index, count, stop: &stop, workspace };
            let spawned = std::thread::Builder::new().name("bridge-search".into()).spawn_scoped(s, move || {
                let result = task(&worker);
                if result.is_err() {
                    worker.stop.store(true, Ordering::Relaxed);
                }
                result
            });
            match spawned {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        let spawned_all = handles.len() == count;
        let mut results: Vec<Result<T, SearchError>> =
            handles.into_iter().map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect();
        if !spawned_all {
            results.push(Err(SearchError::Busy));
        }
        results
    });
    // The first real failure explains the others, which only stopped for it.
    let mut out = Vec::with_capacity(results.len());
    let mut stopped = None;
    for result in results {
        match result {
            Ok(v) => out.push(v),
            Err(SearchError::Superseded) => stopped = Some(SearchError::Superseded),
            Err(e) => return Err(e),
        }
    }
    match stopped {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_are_bounded_and_returned() {
        let got = run(1000, 0, &Cancel::never(), |w| {
            assert!(w.count <= threads());
            Ok((w.index, w.count))
        })
        .ok()
        .unwrap();
        assert_eq!(got.len(), got[0].1);
        assert!(got.iter().enumerate().all(|(i, &(index, _))| i == index));
        let failed = run(4, 0, &Cancel::never(), |w| if w.index == 0 { Err(SearchError::Busy) } else { Ok(()) });
        assert!(matches!(failed, Err(SearchError::Busy)));
    }

    #[test]
    fn a_pass_takes_no_more_workers_than_the_budget_has_buffers_for() {
        // Buffers take at most half the budget: two of a quarter each, not three.
        let got = run(3, budget() / 4, &Cancel::never(), |w| Ok(w.count));
        assert!(!matches!(got, Err(SearchError::TooLarge)), "two buffers fit half the budget");
        if let Ok(counts) = got {
            assert!(counts.len() <= 2 && counts.iter().all(|&c| c == counts.len()), "{counts:?}");
        }
        // Two buffers of just over half the budget never fit together, so the
        // pass runs on one worker instead of being refused as too large.
        let got = run(2, budget() / 2 + 1, &Cancel::never(), |w| Ok(w.count));
        assert!(!matches!(got, Err(SearchError::TooLarge)), "one buffer fits the budget");
        if let Ok(counts) = got {
            assert_eq!(counts, [1]);
        }
        // A buffer larger than the whole budget is too large on any number of workers.
        assert!(matches!(run(2, budget() + 1, &Cancel::never(), |_| Ok(())), Err(SearchError::TooLarge)));
    }
}
