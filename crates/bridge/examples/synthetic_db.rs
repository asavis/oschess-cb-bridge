//! A synthetic 2CBH database for measuring the position index (#142), and
//! the index's build timed, printing numbers only:
//!
//! ```text
//! cargo run --release -p bridge --example synthetic_db -- generate <dir> <games> [seed]
//! cargo run --release -p bridge --example synthetic_db -- build <dir> <index dir>
//! ```
//!
//! `generate` writes `db.2cbh`, `db.2cbg` and `db.2lid` into `<dir>`: games
//! of 60 to 100 plies from the standard start, each ply's legal moves ranked
//! by a hash of the position and the move and one picked geometrically, the
//! best one more often in the opening (p = 0.55 before ply 8, 0.35 to 16, 0.2
//! to 30, uniform after), so that openings crowd as in a real database. A
//! game ends early at a mate or a stalemate. Each game is drawn from the seed
//! and its number alone, so the database is the same whatever the threads.
//! Random games transpose less than real ones: they stand for the hot paths
//! of a build, not for the shape of its tree. No player or tournament has a
//! name.
//!
//! `build` builds the position index of `<dir>`'s database in `<index dir>`
//! (`OSCHESS_BRIDGE_THREADS` and `OSCHESS_BRIDGE_SEARCH_MIB` apply), then
//! prints where its time went, in seconds: the stream pass, the tree's
//! passes and the deep section's, each replay then write, the end, and the
//! whole build.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use bridge::explorer::runs::{PassTime, Progress};
use bridge::explorer::{self};
use cbformat::fixture::{bytes, framed, lid_header};
use cbformat::movetable::{END_OF_LINE, MOVES};
use cbformat::replay::word_of;
use cbformat::v2::Database;
use chesscore::{Board, Move};

/// Games generated at a time, over every thread, then written in order.
const CHUNK: u64 = 1 << 16;

/// The splitmix64 generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Whether an event of probability `p` happens.
    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

/// The rank of `mv` in `board`: the same in every game that reaches it.
fn rank(board: &Board, mv: Move) -> u64 {
    let bits = u64::from(mv.from.index() as u8) | u64::from(mv.to.index() as u8) << 6;
    let promotion = mv.promotion.map_or(0, |p| p.index() as u64 + 1) << 12;
    let mut r = Rng(board.hash() ^ (bits | promotion).wrapping_mul(0xff51_afd7_ed55_8ccd));
    r.next()
}

/// Game `number`'s move record content and header record.
fn game(seed: u64, number: u64) -> (Vec<u8>, [u8; 192]) {
    let mut rng = Rng(seed ^ number.wrapping_mul(0xd6e8_feb8_6659_fd93));
    let plies = 60 + rng.next() % 41;
    let mut board = Board::startpos();
    let mut words = vec![MOVES];
    let mut moves: Vec<(u64, Move)> = Vec::with_capacity(64);
    for ply in 0..plies {
        moves.clear();
        moves.extend(board.legal_moves().into_iter().map(|mv| (rank(&board, mv), mv)));
        if moves.is_empty() {
            break;
        }
        let p = match ply {
            0..8 => 0.55,
            8..16 => 0.35,
            16..30 => 0.2,
            _ => 0.0,
        };
        let pick = if p > 0.0 {
            moves.sort_unstable_by_key(|m| m.0);
            let mut k = 0;
            while k + 1 < moves.len() && !rng.chance(p) {
                k += 1;
            }
            k
        } else {
            (rng.next() % moves.len() as u64) as usize
        };
        let mv = moves[pick].1;
        words.push(word_of(&board, mv).expect("a legal move has a word"));
        board.play_unchecked(mv);
    }
    words.push(END_OF_LINE);
    let mut rec = [0u8; 192];
    rec[0] = 1;
    rec[2] = 1;
    rec[3] = 1;
    rec[0x58] = (rng.next() % 3) as u8;
    let elo = |r: &mut Rng| (2000 + r.next() % 800) as i16;
    rec[0x60..0x62].copy_from_slice(&elo(&mut rng).to_le_bytes());
    rec[0x70..0x72].copy_from_slice(&elo(&mut rng).to_le_bytes());
    (framed(1, &bytes(&words)), rec)
}

fn generate(dir: &Path, games: u64, seed: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut cbh = std::io::BufWriter::new(std::fs::File::create(dir.join("db.2cbh"))?);
    let mut cbg = std::io::BufWriter::new(std::fs::File::create(dir.join("db.2cbg"))?);
    let mut head = vec![0u8; 192];
    head[0x0a..0x0c].copy_from_slice(&192i16.to_le_bytes());
    head[0x0d] = 5;
    cbh.write_all(&head)?;
    // The move file's header, its length filled in at the end.
    cbg.write_all(&[0; 12])?;
    let mut at = 12i64;
    let started = Instant::now();
    let mut first = 1;
    while first <= games {
        let last = (first + CHUNK - 1).min(games);
        let per = (last - first + 1).div_ceil(threads as u64);
        let parts: Vec<Vec<(Vec<u8>, [u8; 192])>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads as u64)
                .map(|t| {
                    let lo = first + t * per;
                    let hi = (lo + per).min(last + 1);
                    s.spawn(move || (lo..hi).map(|n| game(seed, n)).collect())
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("a generator thread")).collect()
        });
        for (record, mut rec) in parts.into_iter().flatten() {
            rec[0x08..0x10].copy_from_slice(&at.to_le_bytes());
            at += record.len() as i64;
            cbg.write_all(&record)?;
            cbh.write_all(&rec)?;
        }
        eprintln!("{last} games, {:.0} s", started.elapsed().as_secs_f64());
        first = last + 1;
    }
    cbh.flush()?;
    let mut cbg = cbg.into_inner().map_err(|e| e.into_error())?;
    let mut header = [0u8; 12];
    header[..8].copy_from_slice(&at.to_le_bytes());
    header[8..10].copy_from_slice(&12i16.to_le_bytes());
    header[10..12].copy_from_slice(&[0, 5]);
    std::io::Seek::seek(&mut cbg, std::io::SeekFrom::Start(0))?;
    cbg.write_all(&header)?;
    std::fs::write(dir.join("db.2lid"), lid_header(1024, 0))
}

fn build(dir: &Path, index: &Path) {
    let db = Database::open(dir.join("db")).expect("database");
    let (idx, moves) = explorer::paths(index, "synthetic");
    let _ = std::fs::remove_file(idx);
    let _ = std::fs::remove_file(moves);
    let progress = Progress::default();
    let started = Instant::now();
    let loaded = explorer::prepare(&db, 0, index, "synthetic", &progress).expect("build");
    let total = started.elapsed().as_secs_f64();
    let t = progress.timings();
    let sum = |passes: &[PassTime], f: fn(&PassTime) -> std::time::Duration| {
        passes.iter().map(|p| f(p).as_secs_f64()).sum::<f64>()
    };
    println!(
        "games {} positions {} postings {} passes {}/{}",
        loaded.games(),
        loaded.base.header.keys,
        loaded.base.header.deep_postings,
        progress.tree_passes.load(Ordering::Relaxed),
        progress.deep_passes.load(Ordering::Relaxed)
    );
    println!(
        "reading {:.2} tree {:.2}+{:.2} deep {:.2}+{:.2} closing {:.2} total {total:.2}",
        t.reading.as_secs_f64(),
        sum(&t.tree, |p| p.replay),
        sum(&t.tree, |p| p.write),
        sum(&t.deep, |p| p.replay),
        sum(&t.deep, |p| p.write),
        t.closing.as_secs_f64() + t.renaming.as_secs_f64(),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "synthetic_db generate <dir> <games> [seed] | build <dir> <index dir>";
    match args.get(1).map(String::as_str) {
        Some("generate") if args.len() >= 4 => {
            let games = args[3].parse().expect("games");
            let seed = args.get(4).map_or(2026, |s| s.parse().expect("seed"));
            generate(Path::new(&args[2]), games, seed).expect("writing the database");
        }
        Some("build") if args.len() >= 4 => build(Path::new(&args[2]), Path::new(&args[3])),
        _ => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
