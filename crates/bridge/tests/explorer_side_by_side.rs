//! Index builds that share one process's search budget (#148): side by side,
//! as the tests of one binary run them, and beside a search that takes the
//! whole budget in bursts. Every build completes. A build takes no more of the
//! budget than its entries use, never a room smaller than the largest part of
//! the tree's keys when its share holds that, and waits for the memory
//! searches hold rather than refuse the budget as too small or answer busy.
//! The budget and the workers are read once per process, so each test runs
//! itself again in a child process with the default budget and 64 workers.

use std::path::PathBuf;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bridge::explorer::{self, Loaded, runs::Progress};
use bridge::search::memory::{DEFAULT_BUDGET_MIB, Hold, budget, held};
use bridge::search::workers::threads;
use cbformat::fixture::TempDb;
use cbformat::v2::Database;

mod common;
use common::random_games;

const CHILD: &str = "BRIDGE_SIDE_BY_SIDE_CHILD";

/// Whether this is the child that runs the test's body. The parent runs the
/// test `name` in a child with the default budget and 64 workers, and checks
/// it passed.
fn in_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("OSCHESS_BRIDGE_SEARCH_MIB", DEFAULT_BUDGET_MIB.to_string())
        .env("OSCHESS_BRIDGE_THREADS", "64")
        .output()
        .unwrap();
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
    false
}

/// Games of each database: its parts of the keys hold a few hundred entries
/// each, which fold little.
const GAMES: usize = 150;

/// `count` databases of [`GAMES`] random games, each of its own.
fn databases(name: &str, count: usize) -> Vec<TempDb> {
    (0..count).map(|i| random_games(&format!("side-by-side-{name}-{i}"), GAMES, i as u64 + 1)).collect()
}

/// Builds the index of `db` in a new folder named after `name` and `i`:
/// the index, or why it was not built, and the folder.
fn build(db: &TempDb, name: &str, i: usize) -> (Result<Loaded, String>, PathBuf) {
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let dir = std::env::temp_dir().join(format!("bridge-side-by-side-{name}-{i}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (explorer::prepare(&d, 1, &dir, "db", &Progress::default()), dir)
}

/// Checks that every build completed with all the games, and that the builds
/// and the indexes returned what they held; removes the folders.
fn all_built(built: Vec<(Result<Loaded, String>, PathBuf)>) {
    let failed: Vec<&String> = built.iter().filter_map(|(b, _)| b.as_ref().err()).collect();
    assert!(failed.is_empty(), "{} of {} failed: {failed:?}", failed.len(), built.len());
    assert!(built.iter().all(|(b, _)| b.as_ref().is_ok_and(|l| l.games() == GAMES as u64)));
    let dirs: Vec<PathBuf> = built.into_iter().map(|(_, dir)| dir).collect();
    assert_eq!(held(), 0, "the builds returned what they held, and the indexes their tables");
    for dir in dirs {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Thirty-two builds started at once, each within its share of the default
/// budget, half of it: all complete. Each used to reserve nearly all its
/// share for a pass of a few thousand entries, so that the later builds found
/// the budget taken, waited for a room of 64 entries and refused it as too
/// small for a part of the keys. Together they now hold less than one share.
#[test]
fn builds_side_by_side_all_complete() {
    if !in_child("builds_side_by_side_all_complete") {
        return;
    }
    assert_eq!((budget(), threads()), (DEFAULT_BUDGET_MIB << 20, 64));
    let dbs = databases("together", 32);
    let (start, done, most) = (Barrier::new(dbs.len()), AtomicBool::new(false), AtomicUsize::new(0));
    let built = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                most.fetch_max(held(), Ordering::Relaxed);
                std::thread::yield_now();
            }
        });
        let builds: Vec<_> = dbs
            .iter()
            .enumerate()
            .map(|(i, db)| {
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    build(db, "together", i)
                })
            })
            .collect();
        let built: Vec<_> = builds.into_iter().map(|b| b.join().unwrap()).collect();
        done.store(true, Ordering::Relaxed);
        built
    });
    all_built(built);
    let most = most.into_inner();
    assert!(most < budget() / 2, "the builds held {most} bytes at once");
}

/// A search takes all the budget that is free, again and again, for 10 ms
/// at a time, 5 ms apart; builds one after another complete. A build that
/// finds the budget taken waits for it at every step, where it used to answer
/// busy when it opened what it had written, or take a room too small for a
/// part of the keys and refuse the budget as too small.
#[test]
fn builds_beside_a_search_that_takes_the_budget_complete() {
    if !in_child("builds_beside_a_search_that_takes_the_budget_complete") {
        return;
    }
    let dbs = databases("taken", 6);
    let done = AtomicBool::new(false);
    let built = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                // All that is free, and what the build gives back meanwhile.
                let (mut search, until) = (Hold::default(), Instant::now() + Duration::from_millis(10));
                while Instant::now() < until {
                    let _ = search.grow_quietly(budget().saturating_sub(held()));
                    std::thread::sleep(Duration::from_micros(100));
                }
                drop(search);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let built: Vec<_> = dbs.iter().enumerate().map(|(i, db)| build(db, "taken", i)).collect();
        done.store(true, Ordering::Relaxed);
        built
    });
    all_built(built);
}
