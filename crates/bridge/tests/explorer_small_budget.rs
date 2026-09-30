//! The position index under the smallest search budget, 16 MiB, with one
//! worker. The budget is read once per process, so each test runs itself again
//! in a child process with that budget set.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bridge::catalog::Catalog;
use bridge::explorer::file::Bad;
use bridge::explorer::format::structure;
use bridge::explorer::runs::{Limits, Progress};
use bridge::explorer::{self, Loaded, Lookup, rendered};
use bridge::search::memory::{Cancel, Hold, budget, held};
use bridge::search::workers::{self, WAIT, threads};
use cbformat::fixture::{Builder, TempDb, quiet, words};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;
use cbformat::view::Base;
use chesscore::Board;

mod common;
use common::{built_bytes, random_games};

const CHILD: &str = "BRIDGE_SMALL_BUDGET_CHILD";

/// Whether this is the child that runs the test's body. The parent runs the
/// test `name` in a child with a 16 MiB budget and one worker, and checks it
/// passed.
fn in_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("OSCHESS_BRIDGE_SEARCH_MIB", "16")
        .env("OSCHESS_BRIDGE_THREADS", "1")
        .output()
        .unwrap();
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
    false
}

/// `games` games of 32 quiet pawn moves, a3 a6 … h3 h6 then a4 a5 … h4 h5,
/// with results by turns.
fn pawns(name: &str, games: usize) -> TempDb {
    let mut words = vec![MOVES];
    for (from, to, back) in [('2', '3', ('7', '6')), ('3', '4', ('6', '5'))] {
        for file in 'a'..='h' {
            words.push(quiet(Color::White, Piece::Pawn, &format!("{file}{}", from), &format!("{file}{}", to)));
            words.push(quiet(Color::Black, Piece::Pawn, &format!("{file}{}", back.0), &format!("{file}{}", back.1)));
        }
    }
    words.push(END_OF_LINE);
    let mut b = Builder::new();
    let at = b.moves(1, &words);
    for g in 0..games {
        b.game(at)[0x58] = (g % 3) as u8;
    }
    b.write(name)
}

/// A build of `d` within `limits` in a new folder named after `name`, watched
/// as it runs: the index, its progress, every name the folder held, and the
/// most the budget held.
struct Watched {
    loaded: Loaded,
    progress: Arc<Progress>,
    names: BTreeSet<String>,
    most: usize,
    dir: PathBuf,
}

fn watched(d: &Database, name: &str, limits: &Limits) -> Result<Watched, String> {
    let dir = std::env::temp_dir().join(format!("bridge-small-budget-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (done, most, names) = (AtomicBool::new(false), AtomicU64::new(0), Mutex::new(BTreeSet::new()));
    let progress = Arc::new(Progress::default());
    let built = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                most.fetch_max(held() as u64, Ordering::Relaxed);
                for e in std::fs::read_dir(&dir).unwrap().flatten() {
                    names.lock().unwrap().insert(e.file_name().into_string().unwrap());
                }
                std::thread::yield_now();
            }
        });
        let built = explorer::prepare_with(d, 1, &dir, "db", &progress, limits);
        done.store(true, Ordering::Relaxed);
        built
    })?;
    let (names, most) = (names.into_inner().unwrap(), most.into_inner() as usize);
    Ok(Watched { loaded: built, progress, names, most, dir })
}

/// A build's passes: the tree's and the deep section's.
fn passes(p: &Progress) -> (u64, u64) {
    (p.tree_passes.load(Ordering::Relaxed), p.deep_passes.load(Ordering::Relaxed))
}

/// The two files of the index built in `dir`.
fn files(dir: &Path) -> [PathBuf; 2] {
    [dir.join("db.idx"), dir.join("db.moves")]
}

/// The key acceptance of #147: a build whose share of the budget holds a
/// fraction of the entries and postings at a time takes many passes, and
/// writes the same files, byte for byte but for the build id, as a build
/// that takes one pass of each kind. Neither writes anything but its two
/// `.partial` files, which become the index, nor holds more of the budget
/// than its share. Games that all play one line fold their crowded positions
/// as the passes collect them; games that part ways fill many parts of the
/// keys and many buckets, and a pass ends inside a block of the deep section,
/// or inside a bucket that alone fills it.
#[test]
fn many_passes_write_the_files_of_one() {
    if !in_child("many_passes_write_the_files_of_one") {
        return;
    }
    assert_eq!((budget(), threads()), (16 << 20, 1));
    // Each of the one line's structures is 2,000 postings of a bucket: some
    // of its blocks hold more than a pass of 32 KiB, which ends inside them,
    // and each bucket more than a pass of 8 KiB, 1,024 postings, which ends
    // at a game of it.
    let dbs = [
        (pawns("small-budget-one-line", 2_000), &[32 << 10, 8 << 10][..]),
        (random_games("small-budget-lines", 1_500, 7), &[128 << 10][..]),
    ];
    for (db, pass) in dbs.iter().flat_map(|(db, sizes)| sizes.iter().map(move |&size| (db, size))) {
        let d = Database::open(db.dir().join("db.2cbh")).unwrap();
        let one = watched(&d, "one", &Limits::default()).unwrap();
        assert_eq!(passes(&one.progress), (1, 1), "one pass of each kind");
        let started = Instant::now();
        let many = watched(&d, "many", &Limits { pass_bytes: Some(pass), ..Limits::default() }).unwrap();
        let (tree, deep) = passes(&many.progress);
        assert!(tree > 2 && deep > 2, "{tree} and {deep} passes");
        assert!(started.elapsed() < Duration::from_secs(60), "no wait for memory");
        for w in [&one, &many] {
            let allowed = ["db.idx", "db.idx.partial", "db.moves", "db.moves.partial"];
            assert!(w.names.iter().all(|n| allowed.contains(&n.as_str())), "{:?}", w.names);
            let left: BTreeSet<String> =
                std::fs::read_dir(&w.dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            assert_eq!(left, ["db.idx".to_string(), "db.moves".to_string()].into(), "the index, and nothing else");
            assert!(w.most <= Limits::default().share + (1 << 20), "{} held", w.most);
        }
        for (a, b) in files(&one.dir).iter().zip(&files(&many.dir)) {
            assert!(built_bytes(a) == built_bytes(b), "{} differs", a.display());
        }
        let (a, b) = (&one.loaded.base.header, &many.loaded.base.header);
        assert_eq!(a.games, d.record_count().into());
        assert_eq!(b.deep_postings, a.deep_postings);
        let start = Board::startpos();
        assert_eq!(many.loaded.lookup(start.hash()).unwrap().unwrap().counts.games, a.games);
        drop((one.loaded, many.loaded));
        for dir in [one.dir, many.dir] {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
    assert_eq!(held(), 0, "the builds returned what they held, and the indexes their tables");
}

/// A share too small for a pass of the stream is refused as too large at
/// once, rather than waiting for memory the build holds itself.
#[test]
fn a_share_too_small_is_refused_at_once() {
    if !in_child("a_share_too_small_is_refused_at_once") {
        return;
    }
    let db = pawns("small-budget-refused", 2_000);
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let started = Instant::now();
    let refused = watched(&d, "share", &Limits { share: 1 << 20, ..Limits::default() }).err().unwrap();
    assert!(refused.contains("too small") && started.elapsed() < Duration::from_secs(10), "{refused}");
    let dir = std::env::temp_dir().join(format!("bridge-small-budget-share-{}", std::process::id()));
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert!(left.is_empty(), "a failed build leaves nothing");
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(held(), 0);
}

/// Rendered notable games are held in the budget, stay under their cap,
/// and are evicted when a search needs the memory; answers stay the same.
#[test]
fn rendered_games_are_budgeted_and_evicted() {
    if !in_child("rendered_games_are_budgeted_and_evicted") {
        return;
    }
    let db = pawns("small-budget-cache", 40);
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let dir = std::env::temp_dir().join(format!("bridge-small-budget-cache-{}", std::process::id()));
    let loaded = explorer::prepare(&d, 1, &dir, "db", &Progress::default()).unwrap();
    let d = Base::TwoCbh(d);
    let cache = rendered::cache();
    let board = Board::startpos();
    let before = held();
    let first = explorer::render(&d, &board, loaded.lookup(board.hash()).unwrap(), &loaded);
    assert!(cache.bytes() > 0, "the twelve games were kept");
    assert_eq!(held(), before + cache.bytes(), "and held in the budget");
    // Many large entries: the cache never passes its cap.
    for n in 1..=200u32 {
        loaded.game(10_000 + n, || Some((0, "x".repeat(8_000))));
        assert!(cache.bytes() <= rendered::cap(), "{} over {}", cache.bytes(), rendered::cap());
    }
    // A search that needs every byte not held elsewhere evicts the cache.
    let others = held() - cache.bytes();
    let search = Hold::reserve(budget() - others).unwrap();
    assert_eq!(cache.bytes(), 0, "evicted");
    drop(search);
    assert_eq!(held(), others);
    let again = explorer::render(&d, &board, loaded.lookup(board.hash()).unwrap(), &loaded);
    assert_eq!(first, again, "the same answer after eviction");
    drop(loaded);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A request that finds no room in the search memory for the table of the
/// index kept on disk is answered busy: nothing is built or reported as
/// building, and once there is room the kept index answers.
#[test]
fn a_kept_index_without_memory_is_busy_not_built() {
    if !in_child("a_kept_index_without_memory_is_busy_not_built") {
        return;
    }
    let db = pawns("small-budget-kept", 10);
    let dir = std::env::temp_dir().join(format!("bridge-small-budget-kept-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let catalog = Catalog::new([db.dir().join("db.2cbh")]);
    catalog.explorer.set_dir(dir.clone());
    let entry = Arc::clone(&catalog.entries()[0]);
    let Ok(open) = entry.open() else { panic!("the database does not open") };
    drop(explorer::prepare(&*open.db, open.generation, &dir, &entry.id, &Progress::default()).unwrap());
    let file = dir.join(format!("{}.idx", entry.id));
    let written = std::fs::metadata(&file).unwrap().modified().unwrap();
    let taken = Hold::reserve(budget() - held()).unwrap();
    assert!(matches!(catalog.explorer.index(Arc::clone(&entry), &open), Lookup::Busy));
    assert!(catalog.explorer.building().is_empty());
    drop(taken);
    assert!(matches!(catalog.explorer.index(Arc::clone(&entry), &open), Lookup::Ready(_)));
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), written, "the file was rewritten");
    // The catalog holds the index, its stream mapped.
    drop(catalog);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Replays keep within the workers' limit (#146): with the one worker taken,
/// a bucket small enough for the calling thread to replay waits for it as a
/// bucket of the workers does, and both are answered busy once the wait is
/// over; a superseded one at once. One that sees the worker come free
/// answers.
#[test]
fn a_replay_on_the_calling_thread_waits_for_a_worker() {
    if !in_child("a_replay_on_the_calling_thread_waits_for_a_worker") {
        return;
    }
    assert_eq!(threads(), 1);
    // 300 games of one line and 10 of another, 60 plies of knights out and
    // back past the tree's depth, each line alone in its structure.
    let hops = "g1f3 g8f6 f3g1 f6g8 ".repeat(15);
    let mut b = Builder::new();
    let mut line = |ucis: String, games: usize| {
        let mut board = Board::startpos();
        let mut stream = vec![MOVES];
        stream.extend(words(&mut board, &ucis));
        stream.push(END_OF_LINE);
        let at = b.moves(1, &stream);
        for _ in 0..games {
            b.game(at);
        }
        board
    };
    let pooled = line(format!("e2e4 e7e5 {hops}d2d3"), 300);
    let inline = line(format!("d2d4 d7d5 {hops}e2e3"), 10);
    let db = b.write("small-budget-workers");
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let dir = std::env::temp_dir().join(format!("bridge-small-budget-workers-{}", std::process::id()));
    let idx = explorer::prepare(&d, 1, &dir, "db", &Progress::default()).unwrap();
    // A worker takes 256 candidates at least: the calling thread replays 10.
    let candidates = |board: &Board| idx.base.deep_games(structure(board), false).unwrap().0.len();
    assert_eq!((candidates(&pooled), candidates(&inline)), (300, 10));
    let games =
        |board: &Board, cancel: &Cancel| explorer::deep_stats(&idx, board, cancel).map(|s| s.map(|s| s.counts.games));
    assert_eq!(games(&pooled, &Cancel::never()).unwrap(), Some(300));
    assert_eq!(games(&inline, &Cancel::never()).unwrap(), Some(10));

    // An answer, and how long it took.
    let waited = |board: &Board| {
        let started = Instant::now();
        (games(board, &Cancel::never()), started.elapsed())
    };
    std::thread::scope(|s| {
        // A search holds the one worker until released, or until a failed
        // check drops `release`.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let search = s.spawn(move || {
            workers::run(1, 0, &Cancel::never(), |_| {
                let _ = held.lock().unwrap().recv();
                Ok(())
            })
        });
        while workers::taken() == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
        let (a, b) = (s.spawn(|| waited(&inline)), s.spawn(|| waited(&pooled)));
        for (what, (answer, took)) in [("inline", a.join().unwrap()), ("pooled", b.join().unwrap())] {
            assert!(matches!(answer, Err(Bad::Busy)) && took >= WAIT, "{what}: {answer:?} after {took:?}");
        }
        let latest = Arc::new(AtomicU64::new(0));
        let old = Cancel::newest(&latest);
        let _newer = Cancel::newest(&latest);
        let started = Instant::now();
        assert!(matches!(games(&inline, &old), Err(Bad::Busy)));
        assert!(started.elapsed() < WAIT / 2, "a superseded replay stops waiting");
        // The worker comes free while a replay waits for it.
        let waiting = s.spawn(|| games(&inline, &Cancel::never()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!waiting.is_finished(), "the replay waits for the worker");
        release.send(()).unwrap();
        assert_eq!(waiting.join().unwrap().unwrap(), Some(10));
        assert!(search.join().unwrap().is_ok());
    });
    assert_eq!(workers::taken(), 0, "every worker was returned");
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}
