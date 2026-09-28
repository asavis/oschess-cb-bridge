//! The position index under the smallest search budget, 16 MiB, with one
//! worker. The budget is read once per process, so each test runs itself again
//! in a child process with that budget set.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::catalog::Catalog;
use bridge::explorer::Lookup;
use bridge::explorer::format::MAX_PLY;
use bridge::explorer::runs::{Limits, Progress, RUN_BUFFER, fan_ins};
use bridge::explorer::{self, WRITER_BYTES, deep, rendered};
use bridge::search::memory::{Hold, budget, held};
use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;
use cbformat::view::Base;
use chesscore::Board;

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

/// The reviewer's shape: a build whose runs are far more than the final
/// merge can take beside the writer within half of a 16 MiB budget. The runs
/// are merged in passes that fit, the build never waits for memory it holds
/// itself, and the index equals one built in a single merge.
#[test]
fn a_build_of_many_runs_merges_within_a_small_budget() {
    if !in_child("a_build_of_many_runs_merges_within_a_small_budget") {
        return;
    }
    assert_eq!(budget(), 16 << 20);
    let db = pawns("small-budget-runs", 2_000);
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let limits = Limits { run_entries: Some(300), ..Limits::default() };
    let (_, last) = fan_ins(limits.share, WRITER_BYTES).unwrap();
    let dir = std::env::temp_dir().join(format!("bridge-small-budget-runs-{}", std::process::id()));
    let progress = Progress::default();
    let started = Instant::now();
    let built = explorer::prepare_with(&d, 1, &dir, "db", &progress, &limits).unwrap();
    assert!(started.elapsed() < Duration::from_secs(30), "no wait for memory");
    let entries = progress.total.load(std::sync::atomic::Ordering::Relaxed) as usize;
    assert_eq!(entries, 2_000 * (usize::from(MAX_PLY).min(32) + 1));
    assert!(entries.div_ceil(300) > 4 * last, "{} runs for a final fan-in of {last}", entries.div_ceil(300));
    // The same index from runs that a single merge takes.
    let one_dir = std::env::temp_dir().join(format!("bridge-small-budget-one-{}", std::process::id()));
    let one = explorer::prepare_with(&d, 1, &one_dir, "db", &Progress::default(), &Limits::default()).unwrap();
    let mut board = Board::startpos();
    for uci in ["a2a3", "a7a6", "b2b3", "b7b6"] {
        assert_eq!(built.lookup(board.hash()).unwrap(), one.lookup(board.hash()).unwrap(), "{uci}");
        board.play_checked(uci.parse().unwrap()).unwrap();
    }
    assert_eq!(built.lookup(Board::startpos().hash()).unwrap().unwrap().counts.games, 2_000);
    // A share that cannot hold the writer and two runs is refused at once.
    let tight = Limits { share: WRITER_BYTES + RUN_BUFFER, run_entries: None };
    let started = Instant::now();
    let refused = explorer::prepare_with(&d, 2, &dir, "db2", &Progress::default(), &tight).err().unwrap();
    assert!(refused.contains("too small") && started.elapsed() < Duration::from_secs(5), "{refused}");
    drop((built, one));
    assert_eq!(held(), 0, "the builds returned what they held, and the indexes their tables");
    for dir in [dir, one_dir] {
        std::fs::remove_dir_all(&dir).unwrap();
    }
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

/// The reviewer's shape for the deep section: one structure that every game
/// holds, so that its partition, 12 MB, is larger than what the build's
/// share leaves beside the writer it holds. The partition is sorted on disk
/// within the share, at once, into the blocks an in-memory sort writes.
#[test]
fn a_deep_partition_larger_than_the_share_is_written_within_it() {
    if !in_child("a_deep_partition_larger_than_the_share_is_written_within_it") {
        return;
    }
    let games = 1_500_000u64;
    let write = |name: &str, memory: usize| {
        let dir = std::env::temp_dir().join(format!("bridge-small-budget-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = deep::Sink::create(&dir, 12, &Progress::default()).unwrap();
        // A structure of bucket 1234, held beyond the tree's plies by every
        // other game.
        let structure = 1234u64 << 52;
        for first in (1..=games).step_by(100_000) {
            let postings = (first..first + 100_000).map(|g| deep::posting(structure, 12, g as u32, g % 2 == 0));
            sink.add(&mut postings.collect()).unwrap();
        }
        let parts = sink.finish().unwrap();
        let mut out = Vec::new();
        let started = Instant::now();
        let result = deep::write_section(&parts, 12, &mut out, 0, &dir.join("x"), &Progress::default(), memory);
        std::fs::remove_dir_all(&dir).unwrap();
        (result.map(|(table, kept)| (out, table, kept)), started.elapsed())
    };
    let share = Limits::default().share;
    let writer = Hold::reserve(WRITER_BYTES).unwrap();
    let (within, took) = write("share", share - WRITER_BYTES);
    assert!(took < Duration::from_secs(20), "no wait for memory the build holds: {took:?}");
    let within = within.unwrap();
    assert_eq!(within.2, games);
    drop(writer);
    assert_eq!(held(), 0);
    let (in_memory, _) = write("memory", 64 << 20);
    assert_eq!(within, in_memory.unwrap());
}

/// The deep sink's partition buffers are held in the budget from the start
/// of a build's reading until the partitions are closed.
#[test]
fn a_deep_sinks_buffers_are_held_in_the_budget() {
    if !in_child("a_deep_sinks_buffers_are_held_in_the_budget") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("bridge-small-budget-sink-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let before = held();
    let sink = deep::Sink::create(&dir, 21, &Progress::default()).unwrap();
    assert!(sink.bytes() > 0 && sink.bytes() <= 1 << 20, "{}", sink.bytes());
    assert_eq!(held(), before + sink.bytes());
    sink.finish().unwrap();
    assert_eq!(held(), before);
    std::fs::remove_dir_all(&dir).unwrap();
}
