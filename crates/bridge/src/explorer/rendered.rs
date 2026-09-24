//! Notable games already rendered for explorer answers, for all databases
//! together: a cache whose bytes are held in the search budget, which
//! searches may evict, and which stays under a cap.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::search::memory::{Evict, Hold, budget, register};

/// A cached game: its rating, for ranking, and its `topGames` entry.
pub type Game = (u16, Arc<str>);

/// What one entry is counted as beside its JSON: the key, the value and the
/// map's slot.
const ENTRY_OVERHEAD: usize = 96;

/// The cap: a 64th of the budget, from 256 KiB to 8 MiB.
pub fn cap() -> usize {
    (budget() / 64).clamp(256 << 10, 8 << 20)
}

#[derive(Default)]
struct Inner {
    /// By index (see `Loaded`) and game number.
    games: HashMap<(u64, u32), Game>,
    hold: Hold,
}

#[derive(Default)]
pub struct Rendered {
    inner: Mutex<Inner>,
}

impl Evict for Rendered {
    fn evict(&self) {
        let mut inner = lock(&self.inner);
        inner.games = HashMap::new();
        inner.hold = Hold::default();
    }
}

/// The cache of the process, registered for eviction when first used.
pub fn cache() -> &'static Arc<Rendered> {
    static CACHE: OnceLock<Arc<Rendered>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let cache = Arc::new(Rendered::default());
        let weak: Weak<dyn Evict> = Arc::downgrade(&(Arc::clone(&cache) as Arc<dyn Evict>));
        register(weak);
        cache
    })
}

impl Rendered {
    pub fn get(&self, key: (u64, u32)) -> Option<Game> {
        lock(&self.inner).games.get(&key).cloned()
    }

    /// Keeps `game` under `key` when the budget has room without taking any
    /// from searches, the cache emptied first when it would pass its cap.
    pub fn put(&self, key: (u64, u32), game: &Game) {
        let bytes = game.1.len() + ENTRY_OVERHEAD;
        let mut inner = lock(&self.inner);
        if inner.hold.bytes() + bytes > cap() {
            inner.games = HashMap::new();
            inner.hold = Hold::default();
        }
        if bytes > cap() || inner.games.try_reserve(1).is_err() || inner.hold.grow_quietly(bytes).is_err() {
            return;
        }
        inner.games.insert(key, game.clone());
    }

    /// The bytes the cache holds in the budget.
    pub fn bytes(&self) -> usize {
        lock(&self.inner).hold.bytes()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
