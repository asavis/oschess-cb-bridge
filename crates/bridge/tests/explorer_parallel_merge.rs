//! The final merge of an index build on several workers at once: sixteen
//! workers and a 64 MiB budget. The budget is read once per process, so each
//! test runs itself again in a child process with them set, under a limit of
//! open files where it asks for one.

use std::process::Command;
use std::time::{Duration, Instant};

use bridge::explorer::runs::{Limits, Progress, RUN_BUFFER};
use bridge::explorer::{self, WRITER_BYTES};
use bridge::search::memory::{Hold, budget, held};
use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;
use chesscore::Board;

const CHILD: &str = "BRIDGE_PARALLEL_MERGE_CHILD";

/// Whether this is the child that runs the test's body. The parent runs the
/// test `name` in a child with a 64 MiB budget and sixteen workers, under
/// `ulimit -n files` when given, and checks it passed.
fn in_child(name: &str, files: Option<u32>) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let exe = std::env::current_exe().unwrap();
    let mut command = match files {
        Some(n) => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(format!("ulimit -n {n} && exec \"$0\" \"$@\"")).arg(&exe);
            c
        }
        None => Command::new(&exe),
    };
    let out = command
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("OSCHESS_BRIDGE_SEARCH_MIB", "64")
        .env("OSCHESS_BRIDGE_THREADS", "16")
        .output()
        .unwrap();
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
    false
}

/// Games of 32 quiet pawn moves, a3 a6 … h3 h6 then a4 a5 … h4 h5.
const GAMES: usize = 16_384;
/// At most this many entries a run: the games' 540,672 entries make about 64
/// runs. Four workers read the games, each within a quarter of the budget's
/// quarter, 4 MiB.
const RUN_ENTRIES: usize = 9_000;

fn pawns(name: &str) -> TempDb {
    let mut words = vec![MOVES];
    for (from, to, back) in [('2', '3', ('7', '6')), ('3', '4', ('6', '5'))] {
        for file in 'a'..='h' {
            words.push(quiet(Color::White, Piece::Pawn, &format!("{file}{from}"), &format!("{file}{to}")));
            words.push(quiet(Color::Black, Piece::Pawn, &format!("{file}{}", back.0), &format!("{file}{}", back.1)));
        }
    }
    words.push(END_OF_LINE);
    let mut b = Builder::new();
    let at = b.moves(1, &words);
    for g in 0..GAMES {
        b.game(at)[0x58] = (g % 3) as u8;
    }
    b.write(name)
}

/// Builds the index of `db` in a new folder named after `name`, from runs
/// of at most [`RUN_ENTRIES`] entries, and checks every game passes the start.
fn build(db: &TempDb, name: &str) {
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let limits = Limits { run_entries: Some(RUN_ENTRIES), ..Limits::default() };
    let dir = std::env::temp_dir().join(format!("bridge-parallel-merge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let started = Instant::now();
    let built = explorer::prepare_with(&d, 1, &dir, "db", &Progress::default(), &limits).unwrap();
    assert!(started.elapsed() < Duration::from_secs(30), "the build waited for memory it holds itself");
    assert_eq!(built.lookup(Board::startpos().hash()).unwrap().unwrap().counts.games, GAMES as u64);
    drop(built);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A search holds all of the budget but two writers and 32 run buffers: room
/// for one range's writer and its buffers for the 64 or so runs, and for the
/// reading workers, but not for two ranges. The ranges then merge one after
/// another, and no worker holds a writer while it waits for run buffers that
/// another's writer took.
#[test]
fn ranges_merge_one_at_a_time_when_the_budget_holds_one() {
    if !in_child("ranges_merge_one_at_a_time_when_the_budget_holds_one", None) {
        return;
    }
    let db = pawns("parallel-merge-memory");
    let search = Hold::reserve(budget() - held() - (2 * WRITER_BYTES + 32 * RUN_BUFFER)).unwrap();
    build(&db, "memory");
    drop(search);
    assert_eq!(held(), 0, "the build returned what it held");
}

/// Under a limit of 100 open files, three ranges merge at once from the same
/// 64 or so runs: each run's file is opened once for all the ranges.
#[cfg(unix)]
#[test]
fn ranges_merged_at_once_open_each_run_once() {
    if !in_child("ranges_merged_at_once_open_each_run_once", Some(100)) {
        return;
    }
    build(&pawns("parallel-merge-files"), "files");
}
