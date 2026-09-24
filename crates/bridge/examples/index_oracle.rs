//! Builds the position index of a database and checks it against a
//! brute-force count, printing numbers only (acceptance 1–3 of #24):
//!
//! ```text
//! cargo run --release -p bridge --example index_oracle -- <db.2cbh> <index dir> [positions] [seed] [--keep]
//! ```
//!
//! 1. Builds the index cold, timing it and reading the process's peak memory.
//! 2. Samples positions: a seeded choice of games, and of a ply on each
//!    game's main line within the index's depth.
//! 3. Counts every sampled position over the whole database by walking each
//!    game's move tree with `replay::walk`, an implementation independent of
//!    the index's own reader, and compares the games, results and moves.
//! 4. Times the explorer's answer for each sampled position, warm.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use bridge::explorer::format::{Counts, MAX_PLY, PRUNE_PLY, pack_move};
use bridge::explorer::runs::Progress;
use bridge::explorer::source::outcome;
use bridge::explorer::{self, render};
use cbformat::replay::{self, TreeVisitor};
use cbformat::v2::{Database, MoveData, RecordKind};
use chesscore::{Board, Move};

/// A game's main-line positions to the index's depth, each once, with the
/// move played from it, gathered from the full tree walk.
struct Main {
    ply: u8,
    done: bool,
    positions: Vec<(u64, Option<Move>, u8, Board)>,
    keep_boards: bool,
}

impl Main {
    fn record(&mut self, board: &Board, mv: Option<Move>) {
        let key = board.hash();
        if !self.positions.iter().any(|p| p.0 == key) {
            let b = if self.keep_boards { board.clone() } else { Board::startpos() };
            self.positions.push((key, mv, self.ply, b));
        }
    }
}

impl TreeVisitor for Main {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        if !main_line || self.done {
            return;
        }
        if self.ply >= MAX_PLY || mv.is_none() {
            self.record(before, None);
            self.done = true;
            return;
        }
        self.record(before, mv);
        self.ply += 1;
    }

    fn played(&mut self, after: &Board) {
        if !self.done && self.ply >= MAX_PLY {
            self.record(after, None);
            self.done = true;
        }
    }
}

/// The main line of game `id`, or `None` when the index leaves the game out.
fn main_line(db: &Database, id: u32, keep_boards: bool) -> Option<Main> {
    let r = db.record(id).ok()?;
    if r.kind() != RecordKind::Game || r.is_deleted() {
        return None;
    }
    let data = db.moves_of(&r).ok()?;
    walk_main(&data, keep_boards)
}

/// The main line of a game's move record, walked with `replay::walk`.
fn walk_main(data: &MoveData<'_>, keep_boards: bool) -> Option<Main> {
    let moves = data.moves().ok()?;
    if moves.is_chess960() {
        return None;
    }
    let start = replay::start_board(&moves.start().ok()?).ok()?;
    if start.is_chess960() {
        return None;
    }
    let mut m = Main { ply: 0, done: false, positions: Vec::new(), keep_boards };
    let mut last = start.clone();
    let mut v = Tracker { main: &mut m, last: &mut last };
    let _ = replay::walk(&moves, &mut v);
    if !m.done {
        // The line ended before the index's depth: its last position counts.
        m.record(&last, None);
    }
    Some(m)
}

/// Keeps the last main-line position, for a line that ends early.
struct Tracker<'a> {
    main: &'a mut Main,
    last: &'a mut Board,
}

impl TreeVisitor for Tracker<'_> {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        self.main.play(before, mv, main_line);
    }
    fn played(&mut self, after: &Board) {
        if !self.main.done && self.main.positions.last().is_some() {
            *self.last = after.clone();
        }
        self.main.played(after);
    }
    fn branch(&mut self) {}
    fn resume(&mut self) {
        // After the first end of line, nothing is main line any more.
        if !self.main.done {
            self.main.record(self.last, None);
            self.main.done = true;
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[derive(Default, Clone)]
struct Brute {
    counts: Counts,
    moves: HashMap<u16, u64>,
    min_ply: u8,
}

fn peak_rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (path, dir) = (&args[1], std::path::Path::new(&args[2]));
    let want: usize = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(1000);
    let seed: u64 = args.get(4).and_then(|a| a.parse().ok()).unwrap_or(2026);
    let db = Database::open(path).expect("database");
    let n = db.record_count();

    // With `--keep`, an index built before is checked and reused, as the
    // bridge does; otherwise it is built afresh.
    if !args.iter().any(|a| a == "--keep") {
        let _ = std::fs::remove_file(dir.join("oracle.idx"));
    }
    let progress = Progress::default();
    let t = Instant::now();
    let loaded = explorer::prepare(&db, 0, dir, "oracle", &progress).expect("build");
    let build_s = t.elapsed().as_secs_f64();
    let size = std::fs::metadata(&loaded.base.path).map(|m| m.len()).unwrap_or(0);
    println!("records {n}");
    println!("games indexed {}", loaded.games());
    println!("entries (game, position) {}", progress.total.load(std::sync::atomic::Ordering::Relaxed));
    println!("distinct positions {}", progress.positions.load(std::sync::atomic::Ordering::Relaxed));
    println!("positions kept {}", loaded.base.header.keys);
    println!("index file {:.1} MB", size as f64 / 1e6);
    println!("cold build {build_s:.1} s, peak RSS {} MB", peak_rss_mb());

    // Sample positions: a random game, then a random position on its main line.
    let mut rng = Rng(seed | 1);
    let mut sample: HashMap<u64, Board> = HashMap::new();
    let mut tries = 0;
    while sample.len() < want && tries < want * 100 {
        tries += 1;
        let id = (rng.next() % u64::from(n)) as u32 + 1;
        let Some(m) = main_line(&db, id, true) else { continue };
        if m.positions.is_empty() {
            continue;
        }
        let p = &m.positions[(rng.next() % m.positions.len() as u64) as usize];
        sample.entry(p.0).or_insert_with(|| p.3.clone());
    }
    println!("positions sampled {}", sample.len());

    // Brute force over every game: each thread takes every `threads`-th batch
    // of records, read in two large reads, and walks each game's tree.
    let keys: HashSet<u64> = sample.keys().copied().collect();
    let brute: Mutex<HashMap<u64, Brute>> = Mutex::new(HashMap::new());
    let threads = std::thread::available_parallelism().map_or(4, |c| c.get()).min(16) as u32;
    let t = Instant::now();
    std::thread::scope(|s| {
        for w in 0..threads {
            let (db, keys, brute) = (&db, &keys, &brute);
            s.spawn(move || {
                let mut local: HashMap<u64, Brute> = HashMap::new();
                const BATCH: u32 = 4096;
                let mut first = 1 + w * BATCH;
                while first <= n {
                    let batch = db.batch(first, first.saturating_add(BATCH - 1)).expect("batch");
                    for id in batch.ids() {
                        let Ok(r) = batch.record(id) else { continue };
                        if r.kind() != RecordKind::Game || r.is_deleted() {
                            continue;
                        }
                        let Some(m) = batch.moves_of(&r).ok().and_then(|d| walk_main(&d, false)) else { continue };
                        let o = outcome(&r);
                        for &(key, mv, ply, _) in &m.positions {
                            if keys.contains(&key) {
                                let b =
                                    local.entry(key).or_insert_with(|| Brute { min_ply: u8::MAX, ..Brute::default() });
                                b.counts.add(o);
                                b.min_ply = b.min_ply.min(ply);
                                if let Some(mv) = mv {
                                    *b.moves.entry(pack_move(mv)).or_default() += 1;
                                }
                            }
                        }
                    }
                    match first.checked_add(threads * BATCH) {
                        Some(next) => first = next,
                        None => break,
                    }
                }
                let mut all = brute.lock().unwrap();
                for (k, v) in local {
                    let e = all.entry(k).or_insert_with(|| Brute { min_ply: u8::MAX, ..Brute::default() });
                    e.counts.merge(&v.counts);
                    e.min_ply = e.min_ply.min(v.min_ply);
                    for (m, c) in v.moves {
                        *e.moves.entry(m).or_default() += c;
                    }
                }
            });
        }
    });
    println!("brute-force scan {:.1} s", t.elapsed().as_secs_f64());

    let brute = brute.into_inner().unwrap();
    let (mut equal, mut pruned, mut differ) = (0, 0, 0);
    for key in &keys {
        let b = brute.get(key).cloned().unwrap_or_default();
        let got = loaded.lookup(*key).expect("lookup");
        match got {
            None if b.counts.games == 1 && b.min_ply > PRUNE_PLY => pruned += 1,
            Some(s)
                if s.counts == b.counts
                    && s.moves.iter().map(|m| (m.0, m.1.games)).collect::<HashMap<_, _>>() == b.moves =>
            {
                equal += 1
            }
            _ => differ += 1,
        }
    }
    println!("oracle: {equal} equal, {pruned} pruned as expected, {differ} different");

    // Warm latency of the explorer's answer: lookup and rendering.
    let boards: Vec<&Board> = sample.values().collect();
    for b in &boards {
        let _ = render(&db, b, loaded.lookup(b.hash()).expect("lookup"), &loaded);
    }
    let mut times: Vec<f64> = boards
        .iter()
        .map(|b| {
            let t = Instant::now();
            let _ = render(&db, b, loaded.lookup(b.hash()).expect("lookup"), &loaded);
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| times[((times.len() as f64 - 1.0) * q) as usize];
    println!("warm answer: p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms", at(0.5), at(0.95), at(1.0));
}
