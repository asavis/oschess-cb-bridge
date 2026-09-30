//! The passes of an index build on several workers at once: sixteen workers
//! and a 64 MiB budget. The budget is read once per process, so each test
//! runs itself again in a child process with them set.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use bridge::explorer::runs::{Limits, MEMORY_WAIT, Progress};
use bridge::explorer::{self, Loaded};
use bridge::search::memory::{Hold, budget, held};
use bridge::search::workers::threads;
use cbformat::v2::Database;
use chesscore::Board;

mod common;
use common::{built_bytes, random_games};

/// Whether this is the child that runs the test's body. The parent runs the
/// test `name` in a child with a 64 MiB budget and sixteen workers, and
/// checks it passed.
fn in_child(name: &str) -> bool {
    let env = [("OSCHESS_BRIDGE_SEARCH_MIB", "64"), ("OSCHESS_BRIDGE_THREADS", "16")];
    common::in_child(name, "BRIDGE_PARALLEL_MERGE_CHILD", &env)
}

/// Builds the index of `d` within `limits` in a new folder named after
/// `name`: the index, its folder, the passes of the tree and the deep
/// section, and the most of the budget the build held at once.
fn build(d: &Database, name: &str, limits: &Limits) -> (Loaded, PathBuf, (u64, u64), usize) {
    let dir = std::env::temp_dir().join(format!("bridge-parallel-passes-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let progress = Progress::default();
    let started = Instant::now();
    let (before, done, most) = (held(), AtomicBool::new(false), AtomicUsize::new(0));
    let built = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                most.fetch_max(held().saturating_sub(before), Ordering::Relaxed);
                std::thread::yield_now();
            }
        });
        let built = explorer::prepare_with(d, 1, &dir, "db", &progress, limits);
        done.store(true, Ordering::Relaxed);
        built
    })
    .unwrap();
    // A build that waited for memory it holds itself would wait all of
    // MEMORY_WAIT; this one takes seconds.
    assert!(started.elapsed() < MEMORY_WAIT, "{name}: the build waited for memory it holds itself");
    let names: BTreeSet<String> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, ["db.idx".to_string(), "db.moves".to_string()].into(), "{name}: the index, and nothing else");
    let passes = (progress.tree_passes.load(Ordering::Relaxed), progress.deep_passes.load(Ordering::Relaxed));
    (built, dir, passes, most.into_inner())
}

/// Passes on many workers write the index byte for byte as one pass of each
/// kind does, however many passes the room makes: the room a test gives, or
/// what a search leaves free while it holds all of the budget but 5 MiB,
/// less than the build holds when it has the room, which leaves room for
/// fewer workers than the build asks for, and less room. The move
/// stream holds the same records, and as many bytes: only the order its
/// workers appended them in differs.
#[test]
fn passes_on_many_workers_write_the_same_index() {
    if !in_child("passes_on_many_workers_write_the_same_index") {
        return;
    }
    assert_eq!((budget(), threads()), (64 << 20, 16));
    let db = random_games("parallel-passes", 5_000, 11);
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let (one, one_dir, passes, most) = build(&d, "one", &Limits::default());
    assert_eq!(passes, (1, 1));
    let (many, many_dir, passes, _) = build(&d, "many", &Limits { pass_bytes: Some(256 << 10), ..Limits::default() });
    assert!(passes.0 > 2 && passes.1 > 2, "{passes:?}");
    // Room for one worker of each pass, and little beside it: less than the
    // build holds when it has the room.
    let free = 5 << 20;
    assert!(most > free, "the build held {most} bytes at most");
    let search = Hold::reserve(budget() - held() - free).unwrap();
    let (held_back, held_dir, _, most) = build(&d, "held", &Limits::default());
    assert!(most <= free, "the build held {most} bytes at most");
    drop(search);
    let records = d.record_count();
    // A record as read, but for where its tail lies.
    let game = |index: &Loaded, n: u32| {
        let mut game = index.stream.game(n).unwrap();
        game.entry.tail = 0;
        game
    };
    for (other, dir) in [(&many, &many_dir), (&held_back, &held_dir)] {
        assert!(built_bytes(&one_dir.join("db.idx")) == built_bytes(&dir.join("db.idx")), "{}", dir.display());
        let lengths = [&one_dir, dir].map(|d| std::fs::metadata(d.join("db.moves")).unwrap().len());
        assert_eq!(lengths[0], lengths[1]);
        for n in 1..=records {
            assert_eq!(game(&one, n), game(other, n), "game {n}");
        }
    }
    let start = one.lookup(Board::startpos().hash()).unwrap().unwrap();
    assert_eq!(start.counts.games, u64::from(records));
    drop((one, many, held_back));
    assert_eq!(held(), 0, "the builds returned what they held, and the indexes their tables");
    for dir in [one_dir, many_dir, held_dir] {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
