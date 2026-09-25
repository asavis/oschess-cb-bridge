//! Builds the position index of a 2CBH database and of its PGN export, and
//! compares their answers for sampled positions: the games, results and moves
//! of each. The two indexes read their games through independent paths, the
//! 2CBH move records and the PGN text. Prints numbers only.
//!
//! ```text
//! cargo run --release -p bridge --example pgn_index_pairs -- <db.2cbh> <export.pgn> <index dir> [positions] [seed]
//! ```
//!
//! The PGN holds the database's games only, texts and analyses left out, and
//! a deleted game as any other; a position that a deleted game reaches is
//! compared less the deleted games' part.

use std::path::Path;
use std::time::Instant;

use bridge::explorer::format::Stats;
use bridge::explorer::runs::Progress;
use bridge::explorer::{self, Loaded};
use cbformat::codepage::CodePage;
use cbformat::game::RecordKind;
use cbformat::pgnfile;
use cbformat::replay::{self, start_board};
use cbformat::v2::Database;
use cbformat::view::Base;

fn build(db: &Base, dir: &Path, id: &str) -> (Loaded, f64) {
    let started = Instant::now();
    let loaded = explorer::prepare(db, 0, dir, id, &Progress::default()).expect("build");
    (loaded, started.elapsed().as_secs_f64())
}

/// Counts and moves, the notable games left out: they are numbered by record
/// in one and by game in the other.
fn key(stats: Option<Stats>) -> String {
    let stats = stats.unwrap_or_default();
    let c = &stats.counts;
    let mut moves: Vec<String> =
        stats.moves.iter().map(|(code, m)| format!("{code}:{}/{}/{}/{}", m.games, m.white, m.draws, m.black)).collect();
    moves.sort();
    format!("{}/{}/{}/{} {}", c.games, c.white, c.draws, c.black, moves.join(","))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (base, pgn, dir) = (&args[1], Path::new(&args[2]), Path::new(&args[3]));
    let want: usize = args.get(4).and_then(|a| a.parse().ok()).unwrap_or(2000);
    let mut seed: u64 = args.get(5).and_then(|a| a.parse().ok()).unwrap_or(2026);
    std::fs::create_dir_all(dir).unwrap();

    let started = Instant::now();
    let head = dir.join("pairs.head");
    pgnfile::build(pgn, &head, 0, CodePage::WESTERN, &mut |_| true).expect("PGN index");
    let head_secs = started.elapsed().as_secs_f64();
    let pdb = Base::Pgn(pgnfile::Database::open(pgn, &head, 0, CodePage::WESTERN).expect("PGN"));
    let db2 = Base::TwoCbh(Database::open(base).expect("2CBH"));
    let (i2, s2) = build(&db2, dir, "pairs-2cbh");
    let (ip, sp) = build(&pdb, dir, "pairs-pgn");
    let head_bytes = std::fs::metadata(&head).map_or(0, |m| m.len());
    println!("PGN header index: {head_secs:.1} s, {head_bytes} bytes");
    println!("position index: 2CBH {s2:.1} s, {} games; PGN {sp:.1} s, {} games", i2.games(), ip.games());

    let Base::TwoCbh(db) = &db2 else { unreachable!() };
    let deleted: Vec<u32> = (1..=db.record_count())
        .filter(|&id| db.record(id).is_ok_and(|r| r.kind() == RecordKind::Game && r.is_deleted()))
        .collect();
    println!("deleted games in the 2CBH database: {}", deleted.len());

    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut compared, mut equal, mut with_deleted) = (0usize, 0usize, 0usize);
    let mut first: Vec<u32> = Vec::new();
    while compared < want {
        let id = (next() % u64::from(db.record_count())) as u32 + 1;
        let Ok(r) = db.record(id) else { continue };
        if r.kind() != RecordKind::Game || r.is_deleted() {
            continue;
        }
        let Ok(data) = db.moves_of(&r) else { continue };
        let Ok(moves) = data.moves() else { continue };
        let Ok(mut board) = moves.start().and_then(|s| start_board(&s)) else { continue };
        if board.is_chess960() || moves.is_chess960() {
            continue;
        }
        let stop = (next() % 30) as usize;
        for word in moves.main_line().take(stop) {
            if !matches!(replay::play(&mut board, word), Ok(Some(_))) {
                break;
            }
        }
        let (a, b) = (key(i2.lookup(board.hash()).ok().flatten()), key(ip.lookup(board.hash()).ok().flatten()));
        compared += 1;
        if a == b {
            equal += 1;
        } else if !deleted.is_empty() {
            with_deleted += 1;
        } else if first.len() < 5 {
            first.push(id);
        }
    }
    println!("positions compared {compared}: equal {equal}, differing where deleted games may count {with_deleted}");
    println!("first differing games {first:?}");
    let _ = std::fs::remove_file(&head);
    for id in ["pairs-2cbh", "pairs-pgn"] {
        let (file, work) = explorer::paths(dir, id);
        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_dir_all(work);
    }
}
