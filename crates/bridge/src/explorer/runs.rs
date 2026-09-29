//! What the passes of a build share (#147): the tree's entries, a game
//! passing through a position, the progress it reports, the limits it keeps
//! and the memory it takes from the search budget, waiting for searches to
//! give it back rather than taking theirs.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::search::SearchError;
use crate::search::memory::{Hold, Refused};

use super::format::Outcome;

/// One game passing through one position, in 16 bytes: the key, the game and
/// its outcome, and the move and rating. A position that many games pass
/// through is folded, as a pass collects it, into weighted entries: the games
/// that played one move with one outcome, counted, which rank for no notable
/// game.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub key: u64,
    /// The game number, or for a weighted entry its count of games, in the
    /// high 30 bits; the outcome in the low 2.
    pub game_outcome: u32,
    /// The move in the low 14 bits, whether the entry is weighted in bit 14,
    /// the rating in the top 12.
    pub meta: u32,
}

pub const ENTRY_BYTES: usize = 16;
/// The largest game number an entry holds, and the most games a weighted
/// entry counts.
pub const MAX_GAME: u32 = (1 << 30) - 1;
const WEIGHTED: u32 = 1 << 14;

impl Entry {
    pub fn new(key: u64, game: u32, outcome: Outcome, mv: u16, elo: u16) -> Entry {
        Entry {
            key,
            game_outcome: game << 2 | outcome as u32,
            meta: u32::from(mv & 0x3fff) | u32::from(elo.min(4095)) << 20,
        }
    }

    /// `games` games through position `key` that played `mv` and ended with
    /// `outcome`.
    pub fn weighted(key: u64, games: u32, outcome: Outcome, mv: u16) -> Entry {
        Entry { key, game_outcome: games.min(MAX_GAME) << 2 | outcome as u32, meta: u32::from(mv & 0x3fff) | WEIGHTED }
    }

    pub fn game(&self) -> u32 {
        self.game_outcome >> 2
    }
    pub fn outcome(&self) -> Outcome {
        Outcome::from_bits(self.game_outcome)
    }
    pub fn mv(&self) -> u16 {
        (self.meta & 0x3fff) as u16
    }
    pub fn elo(&self) -> u16 {
        (self.meta >> 20) as u16
    }

    /// Whether the entry counts games apart from its number.
    pub fn is_weighted(&self) -> bool {
        self.meta & WEIGHTED != 0
    }

    /// The games the entry counts.
    pub fn games(&self) -> u64 {
        if self.is_weighted() { u64::from(self.game()) } else { 1 }
    }
}

/// How far a build has come: records read, then entries and postings
/// written.
#[derive(Default)]
pub struct Progress {
    pub phase: std::sync::Mutex<&'static str>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// Distinct positions written so far.
    pub positions: AtomicU64,
    /// Games left out because their moves could not be read.
    pub skipped: AtomicU64,
    /// The passes the tree and the deep section took.
    pub tree_passes: AtomicU64,
    pub deep_passes: AtomicU64,
    pub stop: AtomicBool,
}

impl Progress {
    pub fn start(&self, phase: &'static str, total: u64) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn phase(&self) -> &'static str {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn io(path: &Path, e: std::io::Error) -> SearchError {
    SearchError::Read(cbformat::Error::Io(path.to_path_buf(), e))
}

/// How long a build waits for memory that searches hold before it gives up.
pub const MEMORY_WAIT: Duration = Duration::from_secs(60);

/// Reserves `bytes` without evicting what searches keep, waiting while they
/// hold the budget: a build yields to searches, and fails `Busy` only after
/// [`MEMORY_WAIT`].
pub fn reserve(bytes: usize, progress: &Progress) -> Result<Hold, SearchError> {
    let mut hold = Hold::default();
    grow(&mut hold, bytes, progress)?;
    Ok(hold)
}

/// What a build's passes take: `fixed` bytes, `each` for each of up to
/// `workers` workers, and from `least` to `room` bytes of entries or
/// postings.
pub struct Room {
    pub fixed: usize,
    pub each: usize,
    pub workers: usize,
    pub least: usize,
    pub room: usize,
}

impl Room {
    /// Reserves as much of it as the budget has free now: fewer workers, by
    /// halves, then less room, by halves down to `least`; when not even one
    /// worker and `least` are free, waits for them as [`reserve`] waits. A
    /// build yields to searches that hold the budget by taking fewer workers
    /// and running more passes. Returns the hold, the workers and the room.
    pub fn reserve(&self, progress: &Progress) -> Result<(Hold, usize, usize), SearchError> {
        let mut workers = self.workers.max(1);
        loop {
            if let Ok(mut hold) = Hold::reserve_quietly(self.fixed + workers * self.each + self.least) {
                let mut more = self.room.saturating_sub(self.least);
                while more > 0 && hold.grow_quietly(more).is_err() {
                    more /= 2;
                }
                return Ok((hold, workers, self.least + more));
            }
            if workers == 1 {
                break;
            }
            workers /= 2;
        }
        Ok((reserve(self.fixed + self.each + self.least, progress)?, 1, self.least))
    }
}

/// Adds `more` bytes to `hold` as [`reserve`] reserves them.
pub fn grow(hold: &mut Hold, more: usize, progress: &Progress) -> Result<(), SearchError> {
    let deadline = Instant::now() + MEMORY_WAIT;
    loop {
        match hold.grow_quietly(more) {
            Ok(()) => return Ok(()),
            Err(Refused::Busy) if Instant::now() < deadline && !progress.stop.load(Ordering::Relaxed) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(refused) => return Err(refused.into()),
        }
    }
}

/// What a build may use.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The most the build holds in the search budget at once: half of it, so
    /// that searches keep the rest.
    pub share: usize,
    /// At most this many bytes of entries or postings in a pass, below what
    /// the share holds; tests use it to make many passes from few games.
    pub pass_bytes: Option<usize>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { share: crate::search::memory::budget() / 2, pass_bytes: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_pack_every_field() {
        let e = Entry::new(u64::MAX - 3, MAX_GAME, Outcome::Black, 0x3fff, 4095);
        assert_eq!(
            (e.key, e.game(), e.outcome(), e.mv(), e.elo(), e.is_weighted(), e.games()),
            (u64::MAX - 3, MAX_GAME, Outcome::Black, 0x3fff, 4095, false, 1)
        );
        assert_eq!(Entry::new(1, 2, Outcome::Draw, 5, 9999).elo(), 4095, "ratings are capped");
        let w = Entry::weighted(7, 1_000, Outcome::Other, 0x3fff);
        assert_eq!(
            (w.key, w.games(), w.outcome(), w.mv(), w.elo(), w.is_weighted()),
            (7, 1_000, Outcome::Other, 0x3fff, 0, true)
        );
    }
}
