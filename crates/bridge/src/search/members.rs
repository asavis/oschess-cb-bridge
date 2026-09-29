//! The games of a position that a list is narrowed to (#148): one bit a
//! record, which the workers that find them set at once, held in the budget.

use std::sync::atomic::{AtomicU64, Ordering};

use super::SearchError;
use super::memory::{Cancel, Hold, Refused};

/// A set of record numbers below a bound, one bit each: 1.5 MB for the 12
/// million records of a Mega Database. Workers insert into it at once.
pub struct Members {
    words: Vec<AtomicU64>,
    len: usize,
    _hold: Hold,
}

impl Members {
    /// The empty set of the numbers `0..len`, its bits held in the budget
    /// before they are allocated.
    pub fn new(len: usize) -> Result<Members, Refused> {
        let n = len.div_ceil(64);
        let hold = Hold::reserve(n * 8)?;
        let mut words = Vec::new();
        words.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
        words.extend((0..n).map(|_| AtomicU64::new(0)));
        Ok(Members { words, len, _hold: hold })
    }

    /// Adds `number`: whether it was not in the set before. A number beyond
    /// the bound is never added.
    pub fn insert(&self, number: u32) -> bool {
        let n = number as usize;
        if n >= self.len {
            return false;
        }
        let bit = 1u64 << (n % 64);
        self.words[n / 64].fetch_or(bit, Ordering::Relaxed) & bit == 0
    }

    pub fn contains(&self, number: u32) -> bool {
        let n = number as usize;
        self.words.get(n / 64).is_some_and(|w| w.load(Ordering::Relaxed) & (1 << (n % 64)) != 0)
    }

    /// How many numbers the set holds.
    pub fn count(&self) -> u64 {
        self.words.iter().map(|w| u64::from(w.load(Ordering::Relaxed).count_ones())).sum()
    }

    /// The numbers the set holds, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(i, w)| {
            let mut bits = w.load(Ordering::Relaxed);
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                Some((i * 64) as u32 + bit)
            })
        })
    }
}

/// A position a list is narrowed to: its games, which the position index
/// finds.
pub trait Position {
    /// What tells positions apart among the kept results: two positions of
    /// one key have the same games.
    fn key(&self) -> u64;

    /// The position's games among the database's records, found on the
    /// shared workers; `Superseded` once `cancel` is.
    fn games(&self, cancel: &Cancel) -> Result<Members, SearchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_are_inserted_once_and_listed_in_order() {
        let set = Members::new(130).unwrap();
        for n in [129, 3, 64, 3, 0, 63] {
            set.insert(n);
        }
        assert!(!set.insert(64), "already there");
        assert!(!set.insert(130) && !set.contains(130), "beyond the bound");
        assert!(set.contains(63) && !set.contains(62));
        assert_eq!(set.count(), 5);
        assert_eq!(set.iter().collect::<Vec<_>>(), [0, 3, 63, 64, 129]);
        assert_eq!(Members::new(0).unwrap().iter().count(), 0);
    }
}
