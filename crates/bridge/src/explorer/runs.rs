//! What the passes of a build share (#147): the tree's entries, a game
//! passing through a position, the progress it reports, the limits it keeps
//! and the memory it takes from the search budget, waiting for searches to
//! give it back rather than taking theirs.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::machine::{self, Priority};
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

/// How far a build has come: waiting for its turn, then records read, then
/// entries and postings written.
pub struct Progress {
    pub phase: Mutex<&'static str>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// Distinct positions written so far.
    pub positions: AtomicU64,
    /// Games left out because their moves could not be read.
    pub skipped: AtomicU64,
    /// The passes the tree and the deep section took.
    pub tree_passes: AtomicU64,
    pub deep_passes: AtomicU64,
    /// Where the build's time went.
    pub timings: Mutex<Timings>,
    /// Set to stop the build at its next batch (#149).
    pub stop: AtomicBool,
    /// The priority its threads run at, as a [`Priority`] code: a request
    /// for the database raises a background build's while it runs.
    pub priority: AtomicU8,
}

/// A build that waits for its turn.
impl Default for Progress {
    fn default() -> Progress {
        Progress {
            phase: Mutex::new("waiting"),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            positions: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            tree_passes: AtomicU64::new(0),
            deep_passes: AtomicU64::new(0),
            timings: Mutex::default(),
            stop: AtomicBool::new(false),
            priority: AtomicU8::new(Priority::Normal as u8),
        }
    }
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

    /// Notes something of where the build's time went.
    pub fn time(&self, note: impl FnOnce(&mut Timings)) {
        note(&mut self.timings.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Where the build's time went, so far.
    pub fn timings(&self) -> Timings {
        self.timings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether the build was asked to stop, which each of its threads asks
    /// between batches; the thread takes the build's priority as it asks.
    pub fn stopped(&self) -> bool {
        machine::follow(Priority::from_code(self.priority.load(Ordering::Relaxed)));
        self.stop.load(Ordering::Relaxed)
    }

    /// Makes the progress of a build that stopped before it was done that of
    /// one that waits for its turn again, with nothing done.
    pub fn again(&self) {
        self.start("waiting", 0);
        for count in [&self.positions, &self.skipped, &self.tree_passes, &self.deep_passes] {
            count.store(0, Ordering::Relaxed);
        }
        *self.timings.lock().unwrap_or_else(|e| e.into_inner()) = Timings::default();
        self.stop.store(false, Ordering::Relaxed);
    }
}

/// Where a build's time went (#147), phase by phase and pass by pass, which
/// `cbtool profile` and the `index_oracle` example print.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    /// The stream pass: the games read and the move stream written.
    pub reading: Duration,
    /// Each pass of the tree, then of the deep section.
    pub tree: Vec<PassTime>,
    pub deep: Vec<PassTime>,
    /// The index file's header written and the file synced.
    pub closing: Duration,
    /// The move stream and the index renamed into place.
    pub renaming: Duration,
}

/// One pass: its games replayed, then what they gave written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassTime {
    pub replay: Duration,
    pub write: Duration,
}

impl PassTime {
    pub fn total(&self) -> Duration {
        self.replay + self.write
    }
}

impl Timings {
    pub fn total(&self) -> Duration {
        let passes: Duration = self.tree.iter().chain(&self.deep).map(PassTime::total).sum();
        self.reading + passes + self.closing + self.renaming
    }

    /// One line of numbers, in microseconds, which [`Timings::parse`] reads:
    /// `reading=5903000 tree=512000+230000,500000+210000 deep=… closing=… renaming=…`.
    pub fn line(&self) -> String {
        let us = |d: Duration| d.as_micros();
        let passes = |all: &[PassTime]| {
            all.iter().map(|p| format!("{}+{}", us(p.replay), us(p.write))).collect::<Vec<_>>().join(",")
        };
        format!(
            "reading={} tree={} deep={} closing={} renaming={}",
            us(self.reading),
            passes(&self.tree),
            passes(&self.deep),
            us(self.closing),
            us(self.renaming)
        )
    }

    /// The timings [`Timings::line`] wrote; `None` for any other text.
    pub fn parse(line: &str) -> Option<Timings> {
        let us = |v: &str| v.parse::<u64>().ok().map(Duration::from_micros);
        let passes = |v: &str| -> Option<Vec<PassTime>> {
            v.split(',')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (replay, write) = p.split_once('+')?;
                    Some(PassTime { replay: us(replay)?, write: us(write)? })
                })
                .collect()
        };
        let mut t = Timings::default();
        let mut seen = 0;
        for item in line.split_whitespace() {
            let (key, value) = item.split_once('=')?;
            match key {
                "reading" => t.reading = us(value)?,
                "tree" => t.tree = passes(value)?,
                "deep" => t.deep = passes(value)?,
                "closing" => t.closing = us(value)?,
                "renaming" => t.renaming = us(value)?,
                _ => return None,
            }
            seen += 1;
        }
        (seen == 5).then_some(t)
    }
}

/// `reading 9.81 s, tree 12 passes 11.20 s (replay 7.10 s, write 4.10 s), ...`:
/// each phase on one line.
impl std::fmt::Display for Timings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = |d: Duration| format!("{:.2} s", d.as_secs_f64());
        let passes = |name: &str, all: &[PassTime]| {
            let (replay, write) = (all.iter().map(|p| p.replay).sum(), all.iter().map(|p| p.write).sum());
            format!("{name} {} passes {} (replay {}, write {})", all.len(), s(replay + write), s(replay), s(write))
        };
        write!(
            f,
            "reading {}, {}, {}, closing {}, renaming {}; total {}",
            s(self.reading),
            passes("tree", &self.tree),
            passes("deep", &self.deep),
            s(self.closing),
            s(self.renaming),
            s(self.total())
        )
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
            Err(Refused::Busy) if Instant::now() < deadline && !progress.stopped() => {
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
    /// the share holds, and of the entries a worker of the stream pass folds;
    /// tests use it to make many passes from few games.
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

    /// A build's timings read back from their line, and nothing else.
    #[test]
    fn timings_read_back_from_their_line() {
        let ms = Duration::from_millis;
        let pass = |replay, write| PassTime { replay: ms(replay), write: ms(write) };
        let t = Timings {
            reading: Duration::from_micros(5_903_217),
            tree: vec![pass(512, 230), pass(500, 210)],
            deep: vec![pass(800, 300)],
            closing: ms(30),
            renaming: ms(0),
        };
        assert_eq!(Timings::parse(&t.line()), Some(t.clone()));
        assert_eq!(t.total(), ms(5_903 + 742 + 710 + 1_100 + 30) + Duration::from_micros(217));
        assert!(t.to_string().starts_with("reading 5.90 s, tree 2 passes 1.45 s (replay 1.01 s, write 0.44 s)"));
        let none = Timings::default();
        assert_eq!(Timings::parse(&none.line()), Some(none));
        for bad in ["", "reading=1", "reading=x tree= deep= closing=0 renaming=0", "port 80"] {
            assert_eq!(Timings::parse(bad), None, "{bad}");
        }
    }

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
