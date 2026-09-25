//! The memory budget of search structures — name tables, ranks, sort orders,
//! match sets and kept results, retained or being built — and the cancellation
//! of superseded searches.
//!
//! Every structure reserves its bytes here before it allocates them: a database
//! whose structures could never fit is refused up front, whatever its header
//! claims, and one that does not fit now makes room by evicting what other
//! searches retained, else waits for a retry.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The budget when `OSCHESS_BRIDGE_SEARCH_MIB` does not set another: 1 GiB. A
/// Mega Database of 12 million games needs about 330 MB with three sort orders.
pub const DEFAULT_BUDGET_MIB: usize = 1024;
/// Bytes a worker reserves at a time as a structure grows: a 256th of the
/// budget, from 64 KiB to 1 MiB, so that sixteen workers' unused steps stay a
/// small part of even the smallest budget.
pub fn step() -> usize {
    (budget() / 256).clamp(64 << 10, 1 << 20)
}

/// The budget in bytes: `OSCHESS_BRIDGE_SEARCH_MIB` (16 to 65,536) or the default.
pub fn budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let set = std::env::var("OSCHESS_BRIDGE_SEARCH_MIB").ok().and_then(|v| v.trim().parse::<usize>().ok());
        set.map_or(DEFAULT_BUDGET_MIB, |m| m.clamp(16, 65_536)) << 20
    })
}

static HELD: Mutex<usize> = Mutex::new(0);

/// Why memory was not granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// More than the whole budget: the database is too large to search.
    TooLarge,
    /// The budget is taken by other searches now, even after eviction.
    Busy,
}

/// Bytes reserved in the budget, returned when dropped.
#[derive(Debug, Default)]
pub struct Hold(usize);

impl Hold {
    pub fn reserve(bytes: usize) -> Result<Hold, Refused> {
        let mut hold = Hold(0);
        hold.grow(bytes)?;
        Ok(hold)
    }

    /// Reserves `bytes` without evicting what searches retained: background
    /// work such as a position index yields to searches, and is refused
    /// `Busy` rather than taking their memory.
    pub fn reserve_quietly(bytes: usize) -> Result<Hold, Refused> {
        let mut hold = Hold(0);
        hold.grow_quietly(bytes)?;
        Ok(hold)
    }

    /// Reserves `more` bytes on top of those held, as [`Hold::reserve_quietly`]
    /// does: never evicting.
    pub fn grow_quietly(&mut self, more: usize) -> Result<(), Refused> {
        let total = self.0.checked_add(more).filter(|&t| t <= budget()).ok_or(Refused::TooLarge)?;
        if !take(more) {
            return Err(Refused::Busy);
        }
        self.0 = total;
        Ok(())
    }

    /// Reserves `more` bytes on top of those held.
    pub fn grow(&mut self, more: usize) -> Result<(), Refused> {
        let total = self.0.checked_add(more).filter(|&t| t <= budget()).ok_or(Refused::TooLarge)?;
        if !take(more) {
            evict_all();
            if !take(more) {
                return Err(Refused::Busy);
            }
        }
        self.0 = total;
        Ok(())
    }

    /// Returns what is held above `bytes`.
    pub fn shrink(&mut self, bytes: usize) {
        if bytes < self.0 {
            give(self.0 - bytes);
            self.0 = bytes;
        }
    }

    pub fn bytes(&self) -> usize {
        self.0
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        give(self.0);
    }
}

fn take(bytes: usize) -> bool {
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if bytes > budget() - *held {
        return false;
    }
    *held += bytes;
    true
}

fn give(bytes: usize) {
    *HELD.lock().unwrap_or_else(|e| e.into_inner()) -= bytes;
}

/// Bytes held in the budget now.
pub fn held() -> usize {
    *HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// A value and the budget its memory holds, returned with it.
pub struct Held<T> {
    value: T,
    _hold: Hold,
}

impl<T> Held<T> {
    pub fn new(value: T, hold: Hold) -> Held<T> {
        Held { value, _hold: hold }
    }
}

impl<T> std::ops::Deref for Held<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

/// One worker's share of a structure that several workers grow together: it
/// reserves [`step`] bytes at a time in the structure's shared hold, and gives
/// back what it did not use when dropped.
pub struct Allowance<'a> {
    shared: &'a Mutex<Hold>,
    left: usize,
}

impl<'a> Allowance<'a> {
    pub fn new(shared: &'a Mutex<Hold>) -> Allowance<'a> {
        Allowance { shared, left: 0 }
    }

    pub fn take(&mut self, bytes: usize) -> Result<(), Refused> {
        if bytes > self.left {
            let step = (bytes - self.left).max(step());
            self.shared.lock().unwrap_or_else(|e| e.into_inner()).grow(step)?;
            self.left += step;
        }
        self.left -= bytes;
        Ok(())
    }
}

impl Drop for Allowance<'_> {
    fn drop(&mut self) {
        let mut hold = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let keep = hold.bytes() - self.left;
        hold.shrink(keep);
    }
}

/// A cache that can drop what it retains when the budget runs short.
pub trait Evict: Send + Sync {
    fn evict(&self);
}

static CACHES: Mutex<Vec<Weak<dyn Evict>>> = Mutex::new(Vec::new());

pub fn register(cache: Weak<dyn Evict>) {
    let mut caches = CACHES.lock().unwrap_or_else(|e| e.into_inner());
    caches.retain(|c| c.strong_count() > 0);
    caches.push(cache);
}

fn evict_all() {
    let live: Vec<Arc<dyn Evict>> =
        CACHES.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(Weak::upgrade).collect();
    for cache in live {
        cache.evict();
    }
}

/// Whether a search is still wanted: a newer search on the same database
/// supersedes it.
#[derive(Clone, Default)]
pub struct Cancel {
    latest: Option<Arc<AtomicU64>>,
    ticket: u64,
}

impl Cancel {
    /// A search that nothing supersedes.
    pub fn never() -> Cancel {
        Cancel::default()
    }

    /// A new search on the database whose searches `latest` counts, which
    /// supersedes the one before it.
    pub fn newest(latest: &Arc<AtomicU64>) -> Cancel {
        let ticket = latest.fetch_add(1, Ordering::SeqCst) + 1;
        Cancel { latest: Some(latest.clone()), ticket }
    }

    pub fn is_cancelled(&self) -> bool {
        self.latest.as_ref().is_some_and(|l| l.load(Ordering::SeqCst) != self.ticket)
    }
}

/// The most streams whose searches a database tells apart; the one used least
/// recently is forgotten beyond that.
pub const MAX_STREAMS: usize = 256;

/// The search counters of one database's streams: a client names a stream (a
/// browser tab's list, say), and a search supersedes only the one still running
/// in the same stream.
#[derive(Default)]
pub struct Streams {
    /// Most recently used last.
    counters: Mutex<std::collections::VecDeque<(String, Arc<AtomicU64>)>>,
}

impl Streams {
    /// A new search in `stream`, which supersedes the one before it there.
    pub fn newest(&self, stream: &str) -> Cancel {
        let mut counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        let entry = match counters.iter().position(|(s, _)| s == stream) {
            Some(i) => counters.remove(i).unwrap_or_else(|| (stream.to_string(), Arc::default())),
            None => (stream.to_string(), Arc::default()),
        };
        let cancel = Cancel::newest(&entry.1);
        counters.push_back(entry);
        while counters.len() > MAX_STREAMS {
            counters.pop_front();
        }
        cancel
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the test's own holds are asserted on: the budget's total is the
    /// whole process's, and the tests of this binary run beside each other
    /// (#63). That a hold returns its bytes is asserted by the budget test
    /// binaries, which have the process to themselves.
    #[test]
    fn holds_grow_shrink_and_return() {
        assert_eq!(Hold::reserve(budget() + 1).unwrap_err(), Refused::TooLarge);
        let mut h = Hold::reserve(1000).unwrap();
        h.grow(24).unwrap();
        assert_eq!(h.bytes(), 1024);
        assert_eq!(h.grow(usize::MAX).unwrap_err(), Refused::TooLarge);
        assert_eq!(h.bytes(), 1024, "a refused growth keeps what was held");
        h.shrink(10);
        assert_eq!(h.bytes(), 10);
        h.shrink(20);
        assert_eq!(h.bytes(), 10, "shrinking to more is nothing");
        drop(h);
        let shared = Mutex::new(Hold::default());
        {
            let mut a = Allowance::new(&shared);
            a.take(10).unwrap();
            assert_eq!(shared.lock().unwrap().bytes(), step());
        }
        assert_eq!(shared.lock().unwrap().bytes(), 10, "the unused part is given back");
    }

    #[test]
    fn a_quiet_reservation_never_evicts() {
        struct Flag(std::sync::atomic::AtomicBool);
        impl Evict for Flag {
            fn evict(&self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flag = Arc::new(Flag(std::sync::atomic::AtomicBool::new(false)));
        let weak: Weak<dyn Evict> = Arc::downgrade(&(Arc::clone(&flag) as Arc<dyn Evict>));
        register(weak);
        assert_eq!(Hold::reserve_quietly(budget() + 1).unwrap_err(), Refused::TooLarge);
        let h = Hold::reserve_quietly(4096).unwrap();
        assert_eq!(h.bytes(), 4096);
        drop(h);
        assert!(!flag.0.load(Ordering::SeqCst), "nothing was evicted");
    }

    #[test]
    fn a_newer_search_supersedes() {
        let latest = Arc::new(AtomicU64::new(0));
        let first = Cancel::newest(&latest);
        assert!(!first.is_cancelled());
        let second = Cancel::newest(&latest);
        assert!(first.is_cancelled() && !second.is_cancelled());
        assert!(!Cancel::never().is_cancelled());
    }

    #[test]
    fn streams_are_told_apart_and_bounded() {
        let streams = Streams::default();
        let (a1, b1) = (streams.newest("a"), streams.newest("b"));
        let a2 = streams.newest("a");
        assert!(a1.is_cancelled() && !b1.is_cancelled() && !a2.is_cancelled());
        for i in 0..MAX_STREAMS {
            streams.newest(&format!("s{i}"));
        }
        // "b" was forgotten: its running search is no longer reachable, and a
        // new "b" starts a fresh counter.
        let b2 = streams.newest("b");
        assert!(!b1.is_cancelled() && !b2.is_cancelled());
        assert_eq!(streams.counters.lock().unwrap().len(), MAX_STREAMS);
    }
}
