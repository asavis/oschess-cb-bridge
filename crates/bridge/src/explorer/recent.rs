//! The latest explorer answers narrowed by a search (#268), for all databases
//! together: a position asked for again with the same search, as when a user
//! steps back and forth through a game, is answered from memory instead of
//! walking its games again. Each answer's bytes are held in the search budget,
//! which searches may take back, and at most [`ENTRIES`] are kept.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::search::memory::{Evict, Hold, register};
use crate::sync::lock;

use super::format::{Counts, Stats};

/// Answers kept, the oldest dropped first.
pub const ENTRIES: usize = 64;

/// What one entry is counted as beside its moves and notable games.
const ENTRY_OVERHEAD: usize = 256;

/// An answer's index (by its build), the database's generation, the
/// position's key and the search's text.
pub type Key = (u64, u64, u64, String);

/// A narrowed answer: its counts, moves and notable games, `None` when no
/// game is selected, and the games of the position before the search.
pub struct Narrowed {
    pub stats: Option<Stats>,
    pub games: u64,
}

#[derive(Default)]
pub struct Recent {
    entries: Mutex<VecDeque<(Key, Arc<Narrowed>, Hold)>>,
}

impl Evict for Recent {
    fn evict(&self) {
        if let Ok(mut entries) = self.entries.try_lock() {
            entries.clear();
        }
    }
}

/// The cache of the process, registered for eviction when first used.
pub fn cache() -> &'static Arc<Recent> {
    static CACHE: OnceLock<Arc<Recent>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let cache = Arc::new(Recent::default());
        let weak: Weak<dyn Evict> = Arc::downgrade(&(Arc::clone(&cache) as Arc<dyn Evict>));
        register(weak);
        cache
    })
}

impl Recent {
    pub fn get(&self, key: &Key) -> Option<Arc<Narrowed>> {
        lock(&self.entries).iter().rev().find(|(k, ..)| k == key).map(|(_, n, _)| Arc::clone(n))
    }

    /// Keeps `answer` under `key` when the budget has room without taking any
    /// from searches, the oldest answer dropped when [`ENTRIES`] are kept.
    pub fn put(&self, key: Key, answer: Arc<Narrowed>) {
        let moves = answer
            .stats
            .as_ref()
            .map_or(0, |s| s.moves.len() * size_of::<(u16, Counts)>() + (s.top.len() + s.featured.len()) * 4);
        let Ok(hold) = Hold::reserve_quietly(ENTRY_OVERHEAD + key.3.len() + moves) else { return };
        let mut entries = lock(&self.entries);
        entries.retain(|(k, ..)| *k != key);
        while entries.len() >= ENTRIES {
            entries.pop_front();
        }
        if entries.try_reserve(1).is_ok() {
            entries.push_back((key, answer, hold));
        }
    }

    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(games: u64) -> Arc<Narrowed> {
        Arc::new(Narrowed { stats: None, games })
    }

    #[test]
    fn the_latest_answers_are_kept_and_the_oldest_go_first() {
        let recent = Recent::default();
        let key = |n: u64| (1, 2, n, "tc:normal".to_string());
        for n in 0..ENTRIES as u64 + 3 {
            recent.put(key(n), answer(n));
        }
        assert_eq!(recent.len(), ENTRIES);
        assert!(recent.get(&key(0)).is_none() && recent.get(&key(2)).is_none());
        assert_eq!(recent.get(&key(3)).map(|a| a.games), Some(3));
        // The same key again replaces its answer, keeping one entry.
        recent.put(key(3), answer(33));
        assert_eq!((recent.len(), recent.get(&key(3)).map(|a| a.games)), (ENTRIES, Some(33)));
        // Another search, generation or index is another answer.
        assert!(recent.get(&(1, 2, 3, "tc:blitz".to_string())).is_none());
        assert!(recent.get(&(1, 3, 3, "tc:normal".to_string())).is_none());
        recent.evict();
        assert!(recent.is_empty());
    }
}
