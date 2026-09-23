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
/// Bytes a worker reserves at a time as a structure grows.
pub const STEP: usize = 1 << 20;

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
/// reserves [`STEP`] bytes at a time in the structure's shared hold, and gives
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
            let step = (bytes - self.left).max(STEP);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_grow_shrink_and_return() {
        let before = held();
        assert_eq!(Hold::reserve(budget() + 1).unwrap_err(), Refused::TooLarge);
        let mut h = Hold::reserve(1000).unwrap();
        h.grow(24).unwrap();
        assert_eq!(h.bytes(), 1024);
        assert_eq!(h.grow(usize::MAX).unwrap_err(), Refused::TooLarge);
        h.shrink(10);
        assert_eq!(h.bytes(), 10);
        drop(h);
        assert_eq!(held(), before);
        let shared = Mutex::new(Hold::default());
        {
            let mut a = Allowance::new(&shared);
            a.take(10).unwrap();
            assert_eq!(shared.lock().unwrap().bytes(), STEP);
        }
        assert_eq!(shared.lock().unwrap().bytes(), 10, "the unused part is given back");
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
}
