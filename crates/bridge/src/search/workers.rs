//! Worker threads for passes over a database, bounded for the whole process:
//! however many requests search at once, no more than [`threads`] workers run,
//! and their batch buffers are reserved in the search budget before they are
//! allocated.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::SearchError;
use super::memory::{Cancel, Hold, Refused, budget, step};

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
pub struct Slots(usize);

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
/// to `wait` for the first, then `WorkersBusy`.
fn acquire(want: usize, cancel: &Cancel, wait: Duration) -> Result<Slots, SearchError> {
    let deadline = Instant::now().checked_add(wait);
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
        let left = deadline.map_or(RECHECK, |d| d.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            return Err(SearchError::WorkersBusy);
        }
        // A background build may hold the workers: it does not give way to
        // the work of a thread that waits for them (#149).
        taken = crate::foreground::aside(|| RETURNED.wait_timeout(taken, left.min(RECHECK)))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// One worker, for a pass so small that the calling thread runs it sooner
/// than a worker started for it would. It counts as a started one does, is
/// waited for as [`run`] waits for its first, up to [`WAIT`], then
/// `WorkersBusy`, and is `Superseded` once `cancel` is; one free is taken
/// under a lock.
pub fn one(cancel: &Cancel) -> Result<Slots, SearchError> {
    acquire(1, cancel, WAIT)
}

/// Workers taken now, for tests and diagnostics.
pub fn taken() -> usize {
    *TAKEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a worker is given: its number and the number of workers, a flag that
/// any worker raises when it fails or panics and every worker checks between
/// batches, and room for its batch buffer, already reserved.
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

/// Raises the workers' stop flag as a worker panics, as a worker that fails
/// raises it, before the pass waits for the workers to end: another worker
/// waiting for what the panicked one would have made, as an index build's
/// worker waits for its turn to write, then stops rather than waiting for
/// ever, and the panic reaches the caller (#172).
struct Unwinding<'a>(&'a AtomicBool);

impl Drop for Unwinding<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

/// Runs `task` on up to `want` workers, each with `workspace` bytes of buffer
/// reserved in the budget, and returns their results in worker order. The
/// workers it asks for take at most half the budget with their buffers and one
/// [`step`] each of what they build, so the other half stays for the rest of
/// what they build, and with little budget left it runs on fewer, down to one. When not even one buffer fits now, or a worker
/// cannot be started, it answers `Busy`; a buffer larger than the whole budget
/// is `TooLarge`. It waits up to [`WAIT`] for its first worker, then answers
/// `WorkersBusy`, and waits no longer once `cancel` is, `Superseded`. The
/// first failure stops the other workers, and so does a worker's panic, which
/// then reaches the caller once they have stopped.
pub fn run<T: Send>(
    want: usize,
    workspace: usize,
    cancel: &Cancel,
    task: impl Fn(&Worker<'_>) -> Result<T, SearchError> + Sync,
) -> Result<Vec<T>, SearchError> {
    run_waiting(want, workspace, cancel, WAIT, task)
}

/// [`run`], waiting up to `wait` for its first worker: an index build waits
/// as long as it waits for memory, and stops waiting once it is asked to stop
/// (#180).
pub fn run_waiting<T: Send>(
    want: usize,
    workspace: usize,
    cancel: &Cancel,
    wait: Duration,
    task: impl Fn(&Worker<'_>) -> Result<T, SearchError> + Sync,
) -> Result<Vec<T>, SearchError> {
    let fit = (budget() / 2 / workspace.saturating_add(step())).max(1);
    let mut slots = acquire(want.min(fit), cancel, wait)?;
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
    // The workers run at the caller's priority: an index build's in the
    // background, a search's at the normal one (#149).
    let priority = crate::machine::current();
    let results: Vec<Result<T, SearchError>> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        if handles.try_reserve_exact(count).is_err() {
            return vec![Err(SearchError::Busy)];
        }
        for index in 0..count {
            let worker = Worker { index, count, stop: &stop, workspace };
            let spawned = std::thread::Builder::new()
                .name("bridge-search".into())
                .stack_size(crate::THREAD_STACK)
                .spawn_scoped(s, move || {
                    let _bug = Unwinding(worker.stop);
                    crate::machine::follow(priority);
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

/// Parts `0..count` of a pass, which its workers take one at a time, each the
/// next that no worker has taken yet, so that a worker whose parts take less
/// time takes more of them.
pub struct Parts {
    next: AtomicUsize,
    count: usize,
}

impl Parts {
    pub fn new(count: usize) -> Parts {
        Parts { next: AtomicUsize::new(0), count }
    }

    /// The part `w` takes next; `None` once every part is taken, and
    /// `Superseded` once another worker failed or `cancel` is.
    pub fn take(&self, w: &Worker<'_>, cancel: &Cancel) -> Result<Option<usize>, SearchError> {
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        if i >= self.count {
            return Ok(None);
        }
        if w.stopped() || cancel.is_cancelled() {
            return Err(SearchError::Superseded);
        }
        Ok(Some(i))
    }
}

/// `task` for each of `parts` parts on the workers, a part at a time
/// ([`Parts`]): what it returned for each, in part order. `Superseded` once
/// `cancel` is.
pub fn each<T: Send>(
    parts: usize,
    cancel: &Cancel,
    task: impl Fn(usize) -> Result<T, SearchError> + Sync,
) -> Result<Vec<T>, SearchError> {
    let taken = Parts::new(parts);
    let done = run(threads().min(parts).max(1), 0, cancel, |w| {
        let mut done = Vec::new();
        while let Some(i) = taken.take(w, cancel)? {
            done.push((i, task(i)?));
        }
        Ok(done)
    })?;
    let mut done: Vec<(usize, T)> = done.into_iter().flatten().collect();
    done.sort_unstable_by_key(|d| d.0);
    Ok(done.into_iter().map(|d| d.1).collect())
}

/// Items a sorting worker takes at least; fewer are sorted on one thread.
const SORT_PART_MIN: usize = 1 << 15;
/// Items a chunk of a sort holds at most, whatever the number of workers:
/// a sort looks whether it was superseded between chunks, so that one worker
/// never sorts a long list whole before it looks.
const SORT_CHUNK_MAX: usize = 1 << 18;

/// Sorts `items` by `cmp` on the workers, with a second buffer as long as
/// `items`, which the caller holds in the budget: the workers sort it in
/// chunks, at least one for each and at most [`SORT_CHUNK_MAX`] items each,
/// and the sorted chunks merge into the second buffer ([`merge_ranges`]),
/// which becomes `items`. Items that `cmp` calls equal come in any order.
/// `Superseded` once `cancel` is: a sort looks before it starts, the workers
/// before each chunk they sort and as they merge, and a sort of one chunk,
/// which the calling thread sorts, once it is done.
pub fn sort_by<T, F>(items: &mut Vec<T>, cmp: &F, cancel: &Cancel) -> Result<(), SearchError>
where
    T: Copy + Send + Sync,
    F: Fn(&T, &T) -> std::cmp::Ordering + Sync,
{
    if cancel.is_cancelled() {
        return Err(SearchError::Superseded);
    }
    let n = items.len();
    let parts = threads().min(n / SORT_PART_MIN).max(1);
    let count = parts.max(n.div_ceil(SORT_CHUNK_MAX));
    if count == 1 {
        items.sort_unstable_by(cmp);
        return match cancel.is_cancelled() {
            true => Err(SearchError::Superseded),
            false => Ok(()),
        };
    }
    let per = n.div_ceil(count);
    let chunks: Vec<Mutex<Option<&mut [T]>>> = items.chunks_mut(per).map(|c| Mutex::new(Some(c))).collect();
    each(chunks.len(), cancel, |k| {
        if let Some(chunk) = chunks[k].lock().unwrap_or_else(|e| e.into_inner()).take() {
            chunk.sort_unstable_by(cmp);
        }
        Ok(())
    })?;
    drop(chunks);
    let mut sorted: Vec<T> = Vec::new();
    sorted.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
    sorted.extend_from_slice(items);
    let chunks: Vec<&[T]> = items.chunks(per).collect();
    merge_ranges(&chunks, parts, cmp, cancel, &mut sorted, |x| x)?;
    drop(chunks);
    *items = sorted;
    Ok(())
}

/// Items a part merges between two looks whether the pass was superseded.
const MERGE_CHECK: usize = 1 << 20;

/// Merges `runs`, each sorted by `cmp`, into `out`, as long as all of them,
/// each item as `map` makes it, in up to `parts` parts on the workers:
/// splitters sampled evenly from every run cut the items into ranges of
/// values, and each range of every run merges into its own part of `out`.
/// Items that `cmp` calls equal come in any order. One part merges on the
/// calling thread. Every [`MERGE_CHECK`] items a part looks whether another
/// failed or `cancel` superseded the pass, and it is then `Superseded`.
pub fn merge_ranges<T, U, R, F, M>(
    runs: &[R],
    parts: usize,
    cmp: &F,
    cancel: &Cancel,
    out: &mut [U],
    map: M,
) -> Result<(), SearchError>
where
    T: Copy + Sync,
    U: Send,
    R: AsRef<[T]> + Sync,
    F: Fn(&T, &T) -> std::cmp::Ordering + Sync,
    M: Fn(T) -> U + Sync,
{
    let mut samples: Vec<T> = runs
        .iter()
        .map(R::as_ref)
        .filter(|r| !r.is_empty())
        .flat_map(|r| (1..parts).map(move |j| r[j * r.len() / parts]))
        .collect();
    samples.sort_unstable_by(cmp);
    let splitters: Vec<T> = match samples.len() {
        0 => Vec::new(),
        n => (1..parts).map(|k| samples[k * n / parts]).collect(),
    };
    // Where each part starts in each run; the last row is where the runs end.
    let mut starts = vec![vec![0; runs.len()]];
    starts.extend(
        splitters.iter().map(|s| runs.iter().map(|r| r.as_ref().partition_point(|x| cmp(x, s).is_lt())).collect()),
    );
    starts.push(runs.iter().map(|r| r.as_ref().len()).collect());
    let merge_part = |k: usize, part: &mut [U], stopped: &dyn Fn() -> bool| -> Result<(), SearchError> {
        let (from, to) = (&starts[k], &starts[k + 1]);
        // The head of each run's range, with its run and place: in order,
        // they are a heap of the least first.
        let mut heap: Vec<(T, usize, usize)> =
            (0..runs.len()).filter(|&r| from[r] < to[r]).map(|r| (runs[r].as_ref()[from[r]], r, from[r])).collect();
        heap.sort_unstable_by(|a, b| cmp(&a.0, &b.0));
        let Some(&(mut least)) = heap.first() else { return Ok(()) };
        for (i, slot) in part.iter_mut().enumerate() {
            if i % MERGE_CHECK == 0 && stopped() {
                return Err(SearchError::Superseded);
            }
            let (item, r, at) = least;
            *slot = map(item);
            // The least head is taken and its run's next item put in its
            // place, which sifts the heap once where a pop and a push would
            // twice; the heap's last head takes the place of a run that ends.
            let next = match at + 1 < to[r] {
                true => (runs[r].as_ref()[at + 1], r, at + 1),
                false => match heap.pop() {
                    Some(last) if !heap.is_empty() => last,
                    _ => break,
                },
            };
            least = sift_down(&mut heap, next, cmp);
        }
        Ok(())
    };
    let parts = splitters.len() + 1;
    if parts == 1 {
        return merge_part(0, out, &|| cancel.is_cancelled());
    }
    let mut slots = Vec::with_capacity(parts);
    let mut rest = out;
    for k in 0..parts {
        let len = (0..runs.len()).map(|r| starts[k + 1][r] - starts[k][r]).sum();
        let (part, tail) = std::mem::take(&mut rest).split_at_mut(len);
        slots.push(Mutex::new(Some(part)));
        rest = tail;
    }
    run(parts, 0, cancel, |w| {
        for k in (w.index..parts).step_by(w.count) {
            let part = slots[k].lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(part) = part {
                merge_part(k, part, &|| w.stopped() || cancel.is_cancelled())?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// Puts `moved` in the place of the least head of `heap`, a heap by `cmp` of
/// the least first, and then down past each lesser child: the least head now.
/// It is returned as it is held, not read back from the heap just written,
/// which would wait for the write when the least head stays at the top, as
/// it does for as long as one run holds the least keys.
fn sift_down<T: Copy, F: Fn(&T, &T) -> std::cmp::Ordering>(
    heap: &mut [(T, usize, usize)],
    moved: (T, usize, usize),
    cmp: &F,
) -> (T, usize, usize) {
    let (end, mut hole, mut least) = (heap.len(), 0, moved);
    loop {
        let mut child = 2 * hole + 1;
        if child >= end {
            break;
        }
        if child + 1 < end {
            child += usize::from(cmp(&heap[child + 1].0, &heap[child].0).is_lt());
        }
        let up = heap[child];
        if !cmp(&up.0, &moved.0).is_lt() {
            break;
        }
        if hole == 0 {
            least = up;
        }
        heap[hole] = up;
        hole = child;
    }
    heap[hole] = moved;
    least
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Items sorted on the workers come in the order one sort gives: many
    /// equal keys, told apart by their numbers, a few items, and none.
    #[test]
    fn a_sort_on_the_workers_orders_as_one_sort() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for n in [0, 1, 7, SORT_PART_MIN * 2 + 3, 300_001] {
            let mut items: Vec<(u32, u32)> = (0..n as u32)
                .map(|i| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ((seed % 1000) as u32, i)
                })
                .collect();
            let mut want = items.clone();
            want.sort_unstable();
            sort_by(&mut items, &|a: &(u32, u32), b: &(u32, u32)| a.cmp(b), &Cancel::never()).unwrap();
            assert_eq!(items, want, "{n} items");
        }
    }

    /// A sort superseded while it runs stops, whether the workers sort it or
    /// one thread does (#179).
    #[test]
    fn a_sort_superseded_while_it_runs_stops() {
        let latest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        for n in [7, SORT_PART_MIN * 2 + 3, 300_001] {
            let cancel = Cancel::newest(&latest);
            let compared = AtomicUsize::new(0);
            // Its first comparison starts a newer search.
            let cmp = |a: &u32, b: &u32| {
                if compared.fetch_add(1, Ordering::Relaxed) == 0 {
                    Cancel::newest(&latest);
                }
                a.cmp(b)
            };
            let mut items: Vec<u32> = (0..n as u32).map(|i| i.wrapping_mul(2_654_435_761)).collect();
            assert!(matches!(sort_by(&mut items, &cmp, &cancel), Err(SearchError::Superseded)), "{n} items");
        }
    }

    /// Whether this is the child that runs the test `name` of this binary.
    /// The parent runs it in a child process with one worker, which
    /// [`threads`] reads once a process, whatever the computer's processors,
    /// and checks it passed.
    fn in_child_with_one_worker(name: &str) -> bool {
        const CHILD: &str = "BRIDGE_WORKERS_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("OSCHESS_BRIDGE_THREADS", "1")
            .output()
            .unwrap();
        let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
        false
    }

    /// A long sort on one worker stops once superseded, before it starts or
    /// while it runs, instead of sorting the whole list first (review of
    /// #220): it sorts a chunk at a time and looks between them. Counted by
    /// its comparisons.
    #[test]
    fn a_long_sort_on_one_worker_stops_once_superseded() {
        if !in_child_with_one_worker("search::workers::tests::a_long_sort_on_one_worker_stops_once_superseded") {
            return;
        }
        assert_eq!(threads(), 1);
        let latest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let numbers: Vec<u64> = (0..1_000_003u64).map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15)).collect();
        // A sort of the numbers that a newer search supersedes at its
        // comparison `at`, before it starts at 0, or never: what it answered,
        // how many comparisons it made, and the numbers as it left them.
        let sort = |at: Option<usize>| {
            let cancel = Cancel::newest(&latest);
            if at == Some(0) {
                Cancel::newest(&latest);
            }
            let compared = AtomicUsize::new(0);
            let cmp = |a: &u64, b: &u64| {
                if Some(compared.fetch_add(1, Ordering::Relaxed) + 1) == at {
                    Cancel::newest(&latest);
                }
                a.cmp(b)
            };
            let mut items = numbers.clone();
            let got = sort_by(&mut items, &cmp, &cancel);
            (got, compared.into_inner(), items)
        };
        let (got, whole, items) = sort(None);
        let mut want = numbers.clone();
        want.sort_unstable();
        assert!(got.is_ok() && items == want, "not superseded, it sorts them all");
        let (got, early, _) = sort(Some(101));
        assert!(matches!(got, Err(SearchError::Superseded)));
        // The chunk it was sorting, one of four, and nothing after.
        assert!(early < whole / 3, "{early} of {whole} comparisons");
        let (got, before, _) = sort(Some(0));
        assert!(matches!(got, Err(SearchError::Superseded)) && before == 0, "{before} comparisons");
    }

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

    /// A pass's workers run at the priority of the thread that started it
    /// (#149): an index build's workers in the background, and a search's at
    /// the normal priority, whatever builds ran before.
    #[test]
    fn workers_run_at_the_callers_priority() {
        use crate::machine::{Priority, at, current};
        for priority in [Priority::Background, Priority::Lowest, Priority::BelowNormal, Priority::Normal] {
            let _at = at(priority);
            let got = run(4, 0, &Cancel::never(), |_| Ok(current())).ok().unwrap();
            assert!(got.iter().all(|&p| p == priority), "{priority:?}: {got:?}");
        }
    }

    #[test]
    fn a_pass_takes_no_more_workers_than_the_budget_has_buffers_for() {
        // Buffers and a step each take at most half the budget: two, not three.
        let got = run(3, budget() / 4 - step(), &Cancel::never(), |w| Ok(w.count));
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
