//! The games of a position that a list is narrowed to (#148): one bit a
//! record, which the workers that find them set at once, held in the budget.

use std::ops::Range;
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
        Members::with(len, Hold::reserve(len.div_ceil(64) * 8)?)
    }

    /// The empty set of the numbers `0..len`, as [`Members::new`] makes it,
    /// but held without evicting what searches retained, as work that is
    /// kept for later is.
    pub fn new_quietly(len: usize) -> Result<Members, Refused> {
        Members::with(len, Hold::reserve_quietly(len.div_ceil(64) * 8)?)
    }

    fn with(len: usize, hold: Hold) -> Result<Members, Refused> {
        let n = len.div_ceil(64);
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

    /// Adds the numbers `64 * word + i` of the bits `i` set in `bits`: how
    /// many were not in the set before. Numbers beyond the bound are never
    /// added.
    pub fn insert_bits(&self, word: usize, bits: u64) -> u32 {
        let bits = bits & self.below(word);
        match self.words.get(word) {
            Some(w) if bits != 0 => (bits & !w.fetch_or(bits, Ordering::Relaxed)).count_ones(),
            _ => 0,
        }
    }

    /// Adds every number of `other`, a word of them at a time: how many were
    /// not in the set before.
    pub fn union(&self, other: &Members) -> u64 {
        let words = other.words.iter().enumerate();
        words.map(|(i, w)| u64::from(self.insert_bits(i, w.load(Ordering::Relaxed)))).sum()
    }

    /// The bits of word `word` whose numbers are below the bound.
    fn below(&self, word: usize) -> u64 {
        match self.len.saturating_sub(word.saturating_mul(64)) {
            n if n >= 64 => !0,
            n => (1 << n) - 1,
        }
    }

    pub fn contains(&self, number: u32) -> bool {
        let n = number as usize;
        self.words.get(n / 64).is_some_and(|w| w.load(Ordering::Relaxed) & (1 << (n % 64)) != 0)
    }

    /// How many numbers the set holds.
    pub fn count(&self) -> u64 {
        self.words.iter().map(|w| u64::from(w.load(Ordering::Relaxed).count_ones())).sum()
    }

    /// The words of bits the set has, 64 numbers each.
    pub fn words(&self) -> usize {
        self.words.len()
    }

    /// The numbers the set holds, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.iter_in(0..self.words.len())
    }

    /// How many numbers the set holds in the words `words`.
    pub fn count_in(&self, words: Range<usize>) -> usize {
        let end = words.end.min(self.words.len());
        let start = words.start.min(end);
        self.words[start..end].iter().map(|w| w.load(Ordering::Relaxed).count_ones() as usize).sum()
    }

    /// The numbers the set holds in the words `words`, ascending.
    pub fn iter_in(&self, words: Range<usize>) -> impl Iterator<Item = u32> + '_ {
        let end = words.end.min(self.words.len());
        let start = words.start.min(end);
        self.words[start..end].iter().zip(start..).flat_map(|(w, i)| {
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

/// Numbers added to a set a word of them at a time, as they come in
/// ascending order; the last word when dropped.
pub struct Adding<'a> {
    set: &'a Members,
    word: usize,
    bits: u64,
}

impl<'a> Adding<'a> {
    pub fn to(set: &'a Members) -> Adding<'a> {
        Adding { set, word: 0, bits: 0 }
    }

    pub fn add(&mut self, number: u32) {
        let word = number as usize / 64;
        if word != self.word {
            self.flush();
            self.word = word;
        }
        self.bits |= 1 << (number % 64);
    }

    fn flush(&mut self) {
        if self.bits != 0 {
            self.set.insert_bits(self.word, self.bits);
            self.bits = 0;
        }
    }
}

impl Drop for Adding<'_> {
    fn drop(&mut self) {
        self.flush();
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

    #[test]
    fn bits_and_sets_are_added_a_word_at_a_time() {
        let set = Members::new(130).unwrap();
        assert_eq!(set.insert_bits(0, 0b1011), 3);
        assert_eq!(set.insert_bits(0, 0b0110), 1, "1 was there");
        assert_eq!(set.insert_bits(2, !0), 2, "128 and 129, below the bound");
        assert_eq!(set.insert_bits(3, !0), 0, "past every word");
        let other = Members::new_quietly(130).unwrap();
        for n in [0, 2, 64, 100, 129] {
            other.insert(n);
        }
        assert_eq!(set.union(&other), 2, "64 and 100");
        assert_eq!(set.iter().collect::<Vec<_>>(), [0, 1, 2, 3, 64, 100, 128, 129]);
        assert_eq!(set.count(), 8);
        let short = Members::new(66).unwrap();
        assert_eq!(short.union(&set), 5, "0-3 and 64; the rest is past its bound");
        assert_eq!(short.iter().collect::<Vec<_>>(), [0, 1, 2, 3, 64]);
        assert_eq!(set.iter_in(1..2).collect::<Vec<_>>(), [64, 100]);
        assert_eq!(set.iter_in(2..9).collect::<Vec<_>>(), [128, 129]);
        assert_eq!(set.iter_in(5..9).count(), 0);
        assert_eq!((set.count_in(1..2), set.count_in(0..9), set.count_in(7..9)), (2, 8, 0));
        assert_eq!(set.words(), 3);
        let added = Members::new(200).unwrap();
        {
            let mut adding = Adding::to(&added);
            for n in [1, 5, 63, 64, 70, 199, 7] {
                adding.add(n);
            }
            assert_eq!(added.count(), 6, "a word at a time: 7 not yet");
        }
        assert_eq!(added.iter().collect::<Vec<_>>(), [1, 5, 7, 63, 64, 70, 199]);
    }
}
