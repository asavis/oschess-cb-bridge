//! An index build keeps to its memory (#147): the tree's table is reserved
//! with the room of its passes, a crowded bucket of the deep section that no
//! pass holds at once is written a range of its games at a time, and the
//! heap the build allocates, which this binary's allocator counts, stays
//! within its share of the budget. The budget and the workers are read once
//! per process, so each build runs in a child process with its own.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use bridge::explorer::format::structure;
use bridge::explorer::runs::{Limits, MEMORY_WAIT, Progress};
use bridge::explorer::{self, Loaded};
use bridge::search::memory::{Hold, budget, held};
use bridge::search::workers::threads;
use cbformat::fixture::{Builder, TempDb, words};
use cbformat::movetable::{END_OF_LINE, MOVES};
use cbformat::v2::Database;
use chesscore::Board;

mod common;
use common::{ChildTest, Ended, add_random_games, built_bytes, index_dir, is_child};

/// The bytes the process has allocated and not freed, and the most since
/// [`Counting::reset`].
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static HEAP: Counting = Counting;

impl Counting {
    fn grew(bytes: usize) {
        let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }

    /// The bytes allocated now, from which the most is counted again.
    fn reset() -> usize {
        let live = LIVE.load(Ordering::Relaxed);
        PEAK.store(live, Ordering::Relaxed);
        live
    }
}

// SAFETY: every call goes to the system allocator as it came; only the
// counts are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            Counting::grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            Counting::grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, size) };
        if !q.is_null() {
            match size.checked_sub(layout.size()) {
                Some(more) => Counting::grew(more),
                None => {
                    LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
                }
            }
        }
        q
    }
}

const CHILD: &str = "BRIDGE_BUILD_MEMORY_CHILD";
/// Where a child finds the database it builds, and the folder of its index.
const DATABASE: &str = "BRIDGE_BUILD_MEMORY_DATABASE";
const INDEX: &str = "BRIDGE_BUILD_MEMORY_INDEX";

/// Starts test `name` in a child process with a budget of `mib` MiB,
/// `workers` workers and `vars` set.
fn child(name: &str, mib: usize, workers: usize, vars: &[(&str, &Path)]) -> ChildTest {
    let (mib, workers) = (mib.to_string(), workers.to_string());
    let mut env = vec![("OSCHESS_BRIDGE_SEARCH_MIB", mib.as_str()), ("OSCHESS_BRIDGE_THREADS", workers.as_str())];
    env.extend(vars.iter().map(|&(name, path)| (name, path.to_str().unwrap())));
    ChildTest::start(name, CHILD, &env)
}

/// Checks that a child passed, and shows what its build reported.
fn passed(ended: Ended) {
    let text = ended.passed();
    text.lines().filter(|l| l.contains(" MiB, ")).for_each(|l| println!("{l}"));
}

/// Folders removed when dropped, as a failed test unwinds too.
struct Folders(Vec<PathBuf>);

impl Drop for Folders {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The tree's table of blocks is reserved with the room of its passes
/// (#147), so that a search holding all of the budget but the build's share,
/// less the table's bytes or a few more or fewer, leaves no room the build
/// waits for: an empty database builds at once.
#[test]
fn the_tree_table_is_reserved_with_its_passes() {
    if !is_child(CHILD) {
        return passed(child("the_tree_table_is_reserved_with_its_passes", 64, 1, &[]).end());
    }
    assert_eq!((budget(), threads()), (64 << 20, 1));
    let db = Builder::new().write("build-memory-empty");
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    assert_eq!(d.record_count(), 0);
    // An empty database's table: 28 bytes for each of 32 blocks.
    for less in [0, 64, 896, 4 << 10] {
        let search = Hold::reserve(budget() / 2 + less).unwrap();
        let dir = index_dir("empty");
        let started = Instant::now();
        let loaded = explorer::prepare(&d, 1, &dir, "db", &Progress::default());
        let took = started.elapsed();
        // A build that waited would wait all of MEMORY_WAIT; an empty
        // database builds in milliseconds.
        assert!(took < MEMORY_WAIT / 2, "{less} bytes less: the build waited {took:?}");
        assert_eq!(loaded.unwrap().games(), 0);
        drop(search);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    assert_eq!(held(), 0);
}

/// The heap a build allocates beyond its share at most: a few KiB, see
/// [`build_crowded`].
const BOOKKEEPING: usize = 64 << 10;

/// Games of a database that hold one structure past the tree's plies, the
/// start's: `g1f3 g8f6 f3g1 f6g8` six times.
const CROWDED: u32 = 800_000;
/// Games before them, of many structures.
const OTHERS: u32 = 2_000;

/// A bucket of the deep section that 800,000 games of one structure crowd
/// (#147) takes several passes of a small budget, on one worker and on many,
/// each a range of its games. The index is byte for byte the one a build
/// that holds the bucket at once writes, and its answers count every game.
/// No build allocates more heap than its share of the budget in any phase.
#[test]
fn a_crowded_bucket_is_built_in_parts_within_the_share() {
    const NAME: &str = "a_crowded_bucket_is_built_in_parts_within_the_share";
    if is_child(CHILD) {
        let var = |name| PathBuf::from(std::env::var_os(name).unwrap());
        return build_crowded(&var(DATABASE), &var(INDEX));
    }
    let db = crowded("build-memory-crowded");
    // One pass holds the bucket at 64 MiB; at 24 and 16 MiB none does. The
    // builds run beside each other.
    let settings = [(64, 16), (16, 1), (16, 16), (24, 16)];
    let dirs = Folders(settings.iter().map(|(mib, workers)| index_dir(&format!("crowded-{mib}-{workers}"))).collect());
    let mut children: Vec<ChildTest> = settings
        .iter()
        .zip(&dirs.0)
        .map(|(&(mib, workers), dir)| child(NAME, mib, workers, &[(DATABASE, db.dir()), (INDEX, dir)]))
        .collect();
    // Each ended before any is checked, so that none outlives a failure.
    let ended: Vec<Ended> = children.iter_mut().map(ChildTest::end).collect();
    ended.into_iter().for_each(passed);
    let built = settings.iter().zip(&dirs.0).map(|(&(mib, workers), dir)| {
        let passes: u64 = std::fs::read_to_string(dir.join("passes")).unwrap().trim().parse().unwrap();
        (format!("{mib} MiB, {workers} workers"), passes, built_bytes(&dir.join("db.idx")))
    });
    let built: Vec<_> = built.collect();
    let (one, passes, bytes) = &built[0];
    for (other, more, other_bytes) in &built[1..] {
        assert!(more > passes, "{other}: {more} passes of the deep section, {one}: {passes}");
        assert!(other_bytes == bytes, "{other}: the index differs from the one of {one}");
    }
}

/// [`OTHERS`] games of many structures, then [`CROWDED`] games of one. The
/// crowded games' records are appended to the file as the builder wrote the
/// first, so that the fixture is written without being held.
fn crowded(name: &str) -> TempDb {
    let mut b = Builder::new();
    add_random_games(&mut b, OTHERS as usize, 3);
    let mut line = vec![MOVES];
    line.extend(words(&mut Board::startpos(), &"g1f3 g8f6 f3g1 f6g8 ".repeat(6)));
    line.push(END_OF_LINE);
    let at = b.moves(1, &line);
    b.game(at);
    let db = b.write(name);
    let mut cbh = std::fs::OpenOptions::new().read(true).append(true).open(db.dir().join("db.2cbh")).unwrap();
    let mut record = [0u8; 192];
    cbh.seek(SeekFrom::End(-192)).unwrap();
    cbh.read_exact(&mut record).unwrap();
    let mut out = std::io::BufWriter::with_capacity(1 << 20, cbh);
    for _ in 1..CROWDED {
        out.write_all(&record).unwrap();
    }
    out.flush().unwrap();
    db
}

/// The child's build of the crowded database at `db` into `dir`: within its
/// share of the heap in every phase, and every game counted. Leaves the index
/// and the deep section's passes in `dir`.
fn build_crowded(db: &Path, dir: &Path) {
    let setting = format!("{} MiB, {} workers", budget() >> 20, threads());
    warm_up();
    let d = Database::open(db.join("db.2cbh")).unwrap();
    let progress = Progress::default();
    let (loaded, phases) = measured(|| explorer::prepare(&d, 1, dir, "db", &progress), &progress);
    let loaded = loaded.unwrap_or_else(|e| panic!("{setting}: {e}"));
    let passes = progress.deep_passes.load(Ordering::Relaxed);
    println!("{setting}: {passes} passes of the deep section, heap {phases:?}; {}", progress.timings());
    // Beside what its passes hold, a build keeps a few KiB of its own: its
    // paths, its threads, the move stream's table of blocks.
    let share = Limits::default().share + BOOKKEEPING;
    assert!(phases.iter().all(|&(_, most)| most <= share), "{setting}: heap {phases:?} over {share} bytes");
    counts_every_game(&loaded);
    drop(loaded);
    std::fs::write(dir.join("passes"), passes.to_string()).unwrap();
}

/// Makes the tables of moves that every build reads, which a process makes
/// once, on its first build, and keeps (the move words, the tree's steps and
/// the deep section's effects, about 1.4 MB): a build of a few games.
fn warm_up() {
    let mut b = Builder::new();
    add_random_games(&mut b, 20, 1);
    let db = b.write("build-memory-warm-up");
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let dir = index_dir("warm-up");
    drop(explorer::prepare(&d, 1, &dir, "db", &Progress::default()).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What `build` gives, and the most heap it allocated at once beyond what
/// the process held before, in each phase that `progress` went through.
fn measured<T>(build: impl FnOnce() -> T, progress: &Progress) -> (T, Vec<(&'static str, usize)>) {
    let before = Counting::reset();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        let watcher = s.spawn(|| {
            let (mut phases, mut phase) = (Vec::with_capacity(16), progress.phase());
            loop {
                let finished = done.load(Ordering::Relaxed);
                let now = progress.phase();
                if now != phase || finished {
                    // The most since the phase began, counted afresh for the
                    // next one.
                    let most = PEAK.swap(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
                    phases.push((phase, most.saturating_sub(before)));
                    phase = now;
                }
                if finished {
                    return phases;
                }
                std::thread::yield_now();
            }
        });
        let built = build();
        done.store(true, Ordering::Relaxed);
        (built, watcher.join().unwrap())
    })
}

/// The index of the crowded database counts every game at the start, and
/// finds each crowded game in the bucket of the start's structure, beyond
/// the tree's plies.
fn counts_every_game(loaded: &Loaded) {
    let start = Board::startpos();
    let all = u64::from(OTHERS + CROWDED);
    assert_eq!((loaded.games(), loaded.lookup(start.hash()).unwrap().unwrap().counts.games), (all, all));
    let (games, _memory) = loaded.base.deep_games(structure(&start), true).unwrap();
    let crowded: Vec<u32> = games.into_iter().filter(|&g| g > OTHERS).collect();
    assert!(crowded.iter().copied().eq(OTHERS + 1..=OTHERS + CROWDED), "{} crowded games found", crowded.len());
}
