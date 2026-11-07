//! What the passes of a build share (#147): the tree's entries, a game
//! passing through a position, the progress it reports, the limits it keeps
//! and the memory it takes from the search budget, waiting for searches to
//! give it back rather than taking theirs.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::foreground;
use crate::machine::{self, Priority};
use crate::search::SearchError;
use crate::search::memory::{Cancel, Hold, Refused};
use crate::search::workers::{self, Worker};
use crate::sync::lock;

use super::file::Bad;
use super::format::Outcome;

/// One game passing through one position, in 24 bytes: the position key,
/// game and outcome, move, legacy rating and precomputed selection key. A position that many games pass
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
    pub featured: u32,
}

pub const ENTRY_BYTES: usize = std::mem::size_of::<Entry>();
/// The largest game number an entry holds, and the most games a weighted
/// entry counts.
pub const MAX_GAME: u32 = (1 << 30) - 1;
const WEIGHTED: u32 = 1 << 14;

impl Entry {
    pub fn new(key: u64, game: u32, outcome: Outcome, mv: u16, elo: u16) -> Entry {
        Entry {
            key,
            game_outcome: game << 2 | outcome as u32,
            featured: 0,
            meta: u32::from(mv & 0x3fff) | u32::from(elo.min(4095)) << 20,
        }
    }

    /// `games` games through position `key` that played `mv` and ended with
    /// `outcome`.
    pub fn weighted(key: u64, games: u32, outcome: Outcome, mv: u16) -> Entry {
        Entry {
            featured: 0,
            key,
            game_outcome: games.min(MAX_GAME) << 2 | outcome as u32,
            meta: u32::from(mv & 0x3fff) | WEIGHTED,
        }
    }

    pub fn with_featured(mut self, rank: u32) -> Self {
        self.featured = rank;
        self
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
    /// Set to stop the build at its next batch (#149), through
    /// [`Progress::ask_stop`]. Its threads read it through
    /// [`Progress::stopped`] alone, and its passes' waits for a worker
    /// through [`Progress::cancel`] (#180).
    stop: Arc<AtomicBool>,
    /// The priority its threads run at, as a [`Priority`] code: a request
    /// for the database raises a background build's while it runs.
    pub priority: AtomicU8,
    /// How long each of its threads gives way to foreground work at most, at
    /// a time, in microseconds ([`Progress::give_way`]): a background
    /// build's patience; 0 for a build that never gives way, as a requested
    /// one (#149).
    patience: AtomicU64,
    /// Whether a request waits for the build, which then takes the workers
    /// searches leave free ([`pass_workers`]): a build queued in the
    /// background is not, until a request promotes it. A build run outside
    /// the queues, as `cbtool profile` runs one, is.
    requested: AtomicBool,
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
            stop: Arc::default(),
            priority: AtomicU8::new(Priority::Normal as u8),
            patience: AtomicU64::new(0),
            requested: AtomicBool::new(true),
        }
    }
}

impl Progress {
    pub fn start(&self, phase: &'static str, total: u64) {
        *lock(&self.phase) = phase;
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn phase(&self) -> &'static str {
        *lock(&self.phase)
    }

    /// Notes something of where the build's time went.
    pub fn time(&self, note: impl FnOnce(&mut Timings)) {
        note(&mut lock(&self.timings));
    }

    /// Where the build's time went, so far.
    pub fn timings(&self) -> Timings {
        lock(&self.timings).clone()
    }

    /// Whether the build was asked to stop, which each of its threads asks
    /// between batches, and while it waits; the thread takes the build's
    /// priority as it asks.
    pub fn stopped(&self) -> bool {
        machine::follow(Priority::from_code(self.priority.load(Ordering::Relaxed)));
        self.stop.load(Ordering::Relaxed)
    }

    /// What stops its passes waiting for a worker ([`on_workers`]) once the
    /// build is asked to stop.
    fn cancel(&self) -> Cancel {
        Cancel::when(&self.stop)
    }

    /// Asks the build to stop at its next batch: a thread giving way to
    /// foreground work stops at once.
    pub fn ask_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        foreground::wake();
    }

    /// Has the build's threads give way to foreground work for at most
    /// `patience` at a time from their next batch on; never, from now on,
    /// with none (#149).
    pub fn set_patience(&self, patience: Duration) {
        let micros = u64::try_from(patience.as_micros()).unwrap_or(u64::MAX);
        self.patience.store(micros, Ordering::Relaxed);
        foreground::wake();
    }

    /// How long the build's threads give way to foreground work at most, at a
    /// time; none for a build that never gives way.
    pub fn patience(&self) -> Duration {
        Duration::from_micros(self.patience.load(Ordering::Relaxed))
    }

    /// Makes the build one a request waits for, or one queued in the
    /// background, from its next pass on ([`pass_workers`]).
    pub fn set_requested(&self, requested: bool) {
        self.requested.store(requested, Ordering::Relaxed);
    }

    /// Whether a request waits for the build.
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// Gives way to foreground work (#149), as each of the build's threads
    /// does before it takes its next batch: while searches, sorts, lists or
    /// explorer answers run ([`foreground`]), the thread of a build with
    /// patience waits for them to end, for at most its patience at a time;
    /// then it goes on with one batch whatever runs, so that the build ends
    /// even while they never do. It waits no longer once the build is asked
    /// to stop, or has no patience left, as a requested build has none.
    pub fn give_way(&self) {
        let patience = self.patience.load(Ordering::Relaxed);
        if patience == 0 || !foreground::running() {
            return;
        }
        foreground::wait(Duration::from_micros(patience), &|| {
            self.patience.load(Ordering::Relaxed) == 0 || self.stopped()
        });
    }

    /// Makes the progress of a build that stopped before it was done that of
    /// one that waits for its turn again, with nothing done.
    pub fn again(&self) {
        self.start("waiting", 0);
        for count in [&self.positions, &self.skipped, &self.tree_passes, &self.deep_passes] {
            count.store(0, Ordering::Relaxed);
        }
        *lock(&self.timings) = Timings::default();
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

/// One pass: its games replayed, then what they gave written, and the
/// workers that replayed them (#346).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassTime {
    pub replay: Duration,
    pub write: Duration,
    pub workers: usize,
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

    /// One line of numbers, in microseconds, which [`Timings::parse`] reads,
    /// each pass with its workers after a colon:
    /// `reading=5903000 tree=512000+230000:8,500000+210000:8 deep=… closing=… renaming=…`.
    pub fn line(&self) -> String {
        let us = |d: Duration| d.as_micros();
        let passes = |all: &[PassTime]| {
            let each = all.iter().map(|p| format!("{}+{}:{}", us(p.replay), us(p.write), p.workers));
            each.collect::<Vec<_>>().join(",")
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
                    let (times, workers) = p.split_once(':').unwrap_or((p, "0"));
                    let (replay, write) = times.split_once('+')?;
                    Some(PassTime { replay: us(replay)?, write: us(write)?, workers: workers.parse().ok()? })
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
/// How long a pass of a build waits for a worker, while searches hold every
/// one, before the build gives up (#180): as long as it waits for memory,
/// where a search waits [`workers::WAIT`].
pub const WORKERS_WAIT: Duration = MEMORY_WAIT;

/// The most workers a pass of the build that reports on `progress` takes
/// (#346): a requested build, the one a request waits for, all the search
/// workers but two and never fewer than half of them, so that a search that
/// arrives meanwhile still finds two free; a background build half of them,
/// so that searches keep the rest. A pass takes fewer when its share of the
/// budget holds fewer, or when it has less work.
pub fn pass_workers(progress: &Progress) -> usize {
    let (all, half) = (workers::threads(), workers::threads().div_ceil(2));
    match progress.requested() {
        true => all.saturating_sub(2).max(half),
        false => half,
    }
}

/// Runs `task` on up to `want` workers for a pass of the build that reports
/// on `progress` ([`workers::run`]): the pass waits for its first worker up
/// to `limits.workers_wait`, then fails `WorkersBusy`, and no longer once the
/// build is asked to stop, `Superseded`.
pub fn on_workers<T: Send>(
    want: usize,
    progress: &Progress,
    limits: &Limits,
    task: impl Fn(&Worker<'_>) -> Result<T, SearchError> + Sync,
) -> Result<Vec<T>, SearchError> {
    workers::run_waiting(want, 0, &progress.cancel(), limits.workers_wait, task)
}

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
    /// worker and `least` are free, waits for them as [`reserve`] waits, then
    /// takes what more is free by then. A build yields to searches that hold
    /// the budget by taking fewer workers and running more passes, never by
    /// taking less than `least`. Returns the hold, the workers and the room.
    pub fn reserve(&self, progress: &Progress) -> Result<(Hold, usize, usize), SearchError> {
        let mut workers = self.workers.max(1);
        let mut hold = loop {
            if let Ok(hold) = Hold::reserve_quietly(self.fixed + workers * self.each + self.least) {
                break hold;
            }
            if workers == 1 {
                break reserve(self.fixed + self.each + self.least, progress)?;
            }
            workers /= 2;
        };
        let mut more = self.room.saturating_sub(self.least);
        while more > 0 && hold.grow_quietly(more).is_err() {
            more /= 2;
        }
        Ok((hold, workers, self.least + more))
    }

    /// The workers and the room of the passes of a section still to come,
    /// for a build whose `hold` holds the passes' `workers` and `room` bytes
    /// of entries or postings, when these passes, planned again as `self`,
    /// take more workers (#346): a build that a request promoted while the
    /// section runs takes a requested build's workers from its next pass on,
    /// within its share as a requested build does. The most of
    /// `self.workers` that fit: the room gives each worker more the bytes
    /// that the share no longer holds beside it, keeping no more than `self`
    /// plans for its workers and each one fewer's bytes, and the budget
    /// gives what more they take, while it has that free now, never waiting.
    /// The workers and the room as they were when it has not even one more
    /// worker's free.
    pub fn more_workers(&self, hold: &mut Hold, workers: usize, room: usize) -> (usize, usize) {
        for more in (workers + 1..=self.workers).rev() {
            let fits = room.min(self.room + (self.workers - more) * self.each);
            let (added, freed) = ((more - workers) * self.each, room - fits);
            if added <= freed {
                hold.shrink(hold.bytes() + added - freed);
                return (more, fits);
            }
            if hold.grow_quietly(added - freed).is_ok() {
                return (more, fits);
            }
        }
        (workers, room)
    }
}

/// Opens with `open` a file the build wrote, whose tables it holds in the
/// budget: while searches hold the budget, waits for them as [`reserve`]
/// waits, rather than fail the build `Busy` at once.
pub fn opened<T>(progress: &Progress, open: impl Fn() -> Result<T, Bad>) -> Result<T, Bad> {
    let deadline = Instant::now() + MEMORY_WAIT;
    loop {
        match open() {
            Err(Bad::Busy) if Instant::now() < deadline && !progress.stopped() => {
                std::thread::sleep(Duration::from_millis(100));
            }
            other => return other,
        }
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
    /// How long a pass waits for a worker while searches hold every one:
    /// [`WORKERS_WAIT`]; tests shorten it.
    pub workers_wait: Duration,
    /// How much later than the first every other worker of a pass takes its
    /// first chunk; tests use it to start the workers as a busy machine does
    /// (#237).
    pub late_start: Duration,
    /// Called as each pass of the tree or of the deep section starts, its
    /// workers taken; tests use it to promote a build at a given pass (#346).
    pub pass_started: Option<fn(&Progress)>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            share: crate::search::memory::budget() / 2,
            pass_bytes: None,
            workers_wait: WORKERS_WAIT,
            late_start: Duration::ZERO,
            pass_started: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// A thread of a build with patience gives way while foreground work
    /// runs, until the work ends, the build is asked to stop or is left
    /// without patience, whichever comes first; one of a build without
    /// patience goes on at once, and one with little waits no longer (#149).
    #[test]
    fn a_build_with_patience_gives_way_to_foreground_work() {
        let _serial = lock(&foreground::tests::SERIAL);
        // Far shorter than the hour's patience it is told from, and longer
        // than a loaded machine stalls a thread (#238).
        let soon = crate::search::workers::tests::PATIENCE;
        let progress = Arc::new(Progress::default());
        let giving = |progress: &Arc<Progress>| {
            let progress = Arc::clone(progress);
            std::thread::spawn(move || progress.give_way())
        };
        // Held until a build that gives way has waited a while.
        let held = |waiting: &std::thread::JoinHandle<()>| {
            std::thread::sleep(Duration::from_millis(100));
            assert!(!waiting.is_finished(), "went on while foreground work ran");
        };
        let working = foreground::begin();
        let started = Instant::now();
        progress.give_way();
        assert!(started.elapsed() < soon, "no patience, no wait");

        // Until the work ends.
        progress.set_patience(Duration::from_secs(3600));
        assert_eq!(progress.patience(), Duration::from_secs(3600));
        let waiting = giving(&progress);
        held(&waiting);
        drop(working);
        waiting.join().unwrap();

        // Until the build is left without patience, as a request leaves it.
        let working = foreground::begin();
        let waiting = giving(&progress);
        held(&waiting);
        progress.set_patience(Duration::ZERO);
        waiting.join().unwrap();

        // Until the build is asked to stop.
        progress.set_patience(Duration::from_secs(3600));
        let waiting = giving(&progress);
        held(&waiting);
        progress.ask_stop();
        waiting.join().unwrap();
        assert!(progress.stopped());

        // For its patience at most.
        progress.again();
        progress.set_patience(Duration::from_millis(50));
        let started = Instant::now();
        progress.give_way();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(50) && waited < soon, "{waited:?}");
        drop(working);
    }

    /// A requested build's passes take every worker but two, never fewer than
    /// half; a background build's half; a promoted build's, from then on, a
    /// requested one's (#346).
    #[test]
    fn a_requested_build_takes_all_workers_but_two() {
        let (all, half) = (workers::threads(), workers::threads().div_ceil(2));
        let requested = all.saturating_sub(2).max(half);
        let progress = Progress::default();
        assert!(progress.requested(), "a build outside the queues is waited for");
        assert_eq!(pass_workers(&progress), requested);
        progress.set_requested(false);
        assert_eq!(pass_workers(&progress), half);
        progress.set_requested(true);
        assert_eq!(pass_workers(&progress), requested);
        assert!(all - requested >= 2 || requested == half, "{all} workers, {requested} taken");
    }

    /// A promoted build's passes take their workers more within the share
    /// they were planned in (#346): while their room fills it, each worker
    /// more takes its bytes from the room and the hold stays as it was;
    /// while the room is smaller than the share holds beside them, from the
    /// budget; in between, from both. The room never grows.
    #[test]
    fn more_workers_keep_to_the_share() {
        let kib = 1 << 10;
        // A 4 KiB table and workers of 8 KiB each in a 64 KiB share: room
        // for 28 KiB beside four workers, 12 KiB beside six.
        let plan = |workers: usize, room: usize| Room { fixed: 4 * kib, each: 8 * kib, workers, least: kib, room };
        for (four, six, held) in [(28, 12, 64), (6, 6, 58), (20, 12, 64)] {
            let (mut hold, workers, room) = plan(4, four * kib).reserve(&Progress::default()).unwrap();
            assert_eq!((workers, room), (4, four * kib));
            let more = plan(6, six * kib).more_workers(&mut hold, workers, room);
            assert_eq!((more, hold.bytes()), ((6, six * kib), held * kib), "{four} KiB of room for four");
        }
        let (mut hold, workers, room) = plan(4, 28 * kib).reserve(&Progress::default()).unwrap();
        assert_eq!(plan(4, 28 * kib).more_workers(&mut hold, workers, room), (4, 28 * kib), "none more");
        assert_eq!(hold.bytes(), 64 * kib);
    }

    /// A build's timings read back from their line, and nothing else.
    #[test]
    fn timings_read_back_from_their_line() {
        let ms = Duration::from_millis;
        let pass = |replay, write| PassTime { replay: ms(replay), write: ms(write), workers: 8 };
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
        assert!(t.line().contains("tree=512000+230000:8,500000+210000:8 "), "{}", t.line());
        // A line of an older build, without the workers, reads with none.
        let older = Timings::parse("reading=1 tree=2+3 deep= closing=0 renaming=0").unwrap();
        assert_eq!(
            older.tree,
            [PassTime { replay: Duration::from_micros(2), write: Duration::from_micros(3), workers: 0 }]
        );
        let none = Timings::default();
        assert_eq!(Timings::parse(&none.line()), Some(none));
        for bad in [
            "",
            "reading=1",
            "reading=x tree= deep= closing=0 renaming=0",
            "port 80",
            "reading=1 tree=2+3:x deep= closing=0 renaming=0",
        ] {
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
