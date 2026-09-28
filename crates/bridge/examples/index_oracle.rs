//! Builds the position index of a database and checks the explorer's answers
//! against a brute-force count, printing numbers only (acceptance 1–3 of #24,
//! and of #146 at any ply):
//!
//! ```text
//! cargo run --release -p bridge --example index_oracle -- <db.2cbh> <index dir> [positions] [seed] [--keep]
//! ```
//!
//! 1. Builds the index cold, timing it and reading the process's peak memory.
//! 2. Samples positions: a seeded choice of a ply from 0 to [`SAMPLE_PLIES`],
//!    then of a game whose main line reaches it, 2,000 positions unless told.
//! 3. Counts every sampled position over the whole database by walking each
//!    game's move tree with `replay::walk`, an implementation independent of
//!    the index's own reader: each game once, at the first ply its main line
//!    reaches the position, however deep, with the move it played from there.
//!    Compares the games, results and moves of the explorer's answer, its
//!    tree and its deep section together, and its notable games.
//! 4. Times the explorer's answer for each sampled position, warm.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use bridge::explorer::format::{Counts, MAX_PLY, TOP_GAMES, pack_move};
use bridge::explorer::runs::Progress;
use bridge::explorer::source::{MAX_MOVE_RECORD, average_elo, outcome};
use bridge::explorer::stream::MAX_PLIES;
use bridge::explorer::{self, Loaded, render};
use bridge::search::memory::Cancel;
use cbformat::game::RecordKind;
use cbformat::replay::{self, TreeVisitor};
use cbformat::v2::{Database, MoveData, Record};
use cbformat::view::Base;
use chesscore::{Board, Move};

/// The deepest ply sampled.
const SAMPLE_PLIES: u32 = 120;

/// A game's main line as the index reads it: to its end, a null move, damage,
/// or its [`MAX_PLIES`]th ply, where its last position counts with no move
/// from it.
struct Main<'a> {
    /// The key of the main-line position reached, and its ply.
    here: u64,
    ply: u32,
    /// A main-line move announced and not yet played: it counts once it is,
    /// as an illegal move is announced before it is found to be one.
    pending: Option<Move>,
    done: bool,
    /// The positions looked for, each found once, at its first visit: key,
    /// the move played from there, ply. With none, the line's boards are kept.
    keys: Option<&'a HashSet<u64>>,
    found: Vec<(u64, Option<Move>, u32)>,
    /// The line's boards from its start to ply [`SAMPLE_PLIES`], when no key
    /// is looked for.
    boards: Vec<Board>,
}

impl<'a> Main<'a> {
    fn new(start: &Board, keys: Option<&'a HashSet<u64>>) -> Main<'a> {
        let boards = if keys.is_none() { vec![start.clone()] } else { Vec::new() };
        Main { here: start.hash(), ply: 0, pending: None, done: false, keys, found: Vec::new(), boards }
    }

    /// The position reached, with `mv` played from it.
    fn visit(&mut self, mv: Option<Move>) {
        let Some(keys) = self.keys else { return };
        if keys.contains(&self.here) && !self.found.iter().any(|f| f.0 == self.here) {
            self.found.push((self.here, mv, self.ply));
        }
    }

    /// The line ends where it is, with no move from its last position.
    fn end(&mut self) {
        if !self.done {
            self.visit(None);
            self.done = true;
        }
    }
}

impl TreeVisitor for Main<'_> {
    fn play(&mut self, _before: &Board, mv: Option<Move>, main_line: bool) {
        if !main_line || self.done {
            return;
        }
        match mv.filter(|_| (self.ply as usize) < MAX_PLIES) {
            Some(mv) => self.pending = Some(mv),
            None => self.end(),
        }
    }

    fn played(&mut self, after: &Board) {
        if let Some(mv) = self.pending.take() {
            self.visit(Some(mv));
            self.ply += 1;
            self.here = after.hash();
            if self.keys.is_none() && self.ply <= SAMPLE_PLIES {
                self.boards.push(after.clone());
            }
        }
    }

    /// The main line comes first: after its end, nothing is main line.
    fn resume(&mut self) {
        self.end();
    }

    fn stopped(&self) -> bool {
        self.done
    }
}

/// The main line of a game's move record, walked with `replay::walk`, or
/// `None` when the index leaves the game out.
fn walk_main<'a>(data: &MoveData<'_>, keys: Option<&'a HashSet<u64>>) -> Option<Main<'a>> {
    let moves = data.moves().ok()?;
    if moves.is_chess960() {
        return None;
    }
    let start = replay::start_board(&moves.start().ok()?).ok()?;
    if start.is_chess960() {
        return None;
    }
    let mut m = Main::new(&start, keys);
    // Damage ends the line where it is: the positions before it are kept.
    let _ = replay::walk(&moves, &mut m);
    m.end();
    Some(m)
}

/// Whether the index holds `r`'s game by its kind: a game, not deleted.
fn is_game(r: &Record) -> bool {
    r.kind() == RecordKind::Game && !r.is_deleted()
}

/// The boards of game `id`'s main line to ply [`SAMPLE_PLIES`], or `None`
/// when the index leaves the game out.
fn boards_of(db: &Database, id: u32) -> Option<Vec<Board>> {
    let r = db.record(id).ok().filter(is_game)?;
    let data = db.moves_of_within(&r, MAX_MOVE_RECORD).ok()?;
    Some(walk_main(&data, None)?.boards)
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

/// A position's games counted by brute force: their counts, the moves played
/// from their first visits with theirs, and the best of them by rating, then
/// number, best first.
#[derive(Default, Clone)]
struct Brute {
    counts: Counts,
    moves: HashMap<u16, Counts>,
    top: Vec<(u16, u32)>,
}

impl Brute {
    fn add(&mut self, counts: &Counts, mv: Option<Move>, best: (u16, u32)) {
        self.counts.merge(counts);
        if let Some(mv) = mv {
            self.moves.entry(pack_move(mv)).or_default().merge(counts);
        }
        self.top.push(best);
        self.top.sort_unstable_by(|a, b| b.cmp(a));
        self.top.truncate(TOP_GAMES);
    }

    fn merge(&mut self, other: Brute) {
        self.counts.merge(&other.counts);
        for (mv, c) in other.moves {
            self.moves.entry(mv).or_default().merge(&c);
        }
        self.top.extend(other.top);
        self.top.sort_unstable_by(|a, b| b.cmp(a));
        self.top.truncate(TOP_GAMES);
    }
}

fn peak_rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

/// Every game over the whole database that reaches one of `keys`, walked
/// with `replay::walk` on up to 16 threads, each taking every `threads`-th
/// batch of records. A game over the move record limit is left out as the
/// index leaves it out: the move stream names the games it left out, and each
/// is read again against the limit.
fn brute_force(db: &Database, loaded: &Loaded, keys: &HashSet<u64>) -> (HashMap<u64, Brute>, u64) {
    let n = db.record_count();
    let brute: Mutex<(HashMap<u64, Brute>, u64)> = Mutex::new((HashMap::new(), 0));
    let threads = std::thread::available_parallelism().map_or(4, |c| c.get()).min(16) as u32;
    std::thread::scope(|s| {
        for w in 0..threads {
            let brute = &brute;
            s.spawn(move || {
                let mut local: HashMap<u64, Brute> = HashMap::new();
                let mut over = 0u64;
                const BATCH: u32 = 4096;
                let mut first = 1 + w * BATCH;
                while first <= n {
                    let batch = db.batch(first, first.saturating_add(BATCH - 1)).expect("batch");
                    for id in batch.ids() {
                        let Ok(r) = batch.record(id) else { continue };
                        if !is_game(&r) {
                            continue;
                        }
                        let Some(m) = batch.moves_of(&r).ok().and_then(|d| walk_main(&d, Some(keys))) else {
                            continue;
                        };
                        let left_out = loaded.stream.entry(id).is_ok_and(|e| !e.indexed());
                        if left_out && db.moves_of_within(&r, MAX_MOVE_RECORD).is_err() {
                            over += 1;
                            continue;
                        }
                        let mut counts = Counts::default();
                        counts.add(outcome(&r));
                        for &(key, mv, _) in &m.found {
                            local.entry(key).or_default().add(&counts, mv, (average_elo(&r), id));
                        }
                    }
                    match first.checked_add(threads * BATCH) {
                        Some(next) => first = next,
                        None => break,
                    }
                }
                let mut all = brute.lock().unwrap();
                for (k, v) in local {
                    all.0.entry(k).or_default().merge(v);
                }
                all.1 += over;
            });
        }
    });
    brute.into_inner().unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (path, dir) = (&args[1], std::path::Path::new(&args[2]));
    let want: usize = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(2000);
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
    println!(
        "games left out (move record unreadable or over 2 MiB) {}",
        progress.skipped.load(std::sync::atomic::Ordering::Relaxed)
    );
    println!("entries (game, position) {}", progress.total.load(std::sync::atomic::Ordering::Relaxed));
    println!("distinct positions {}", progress.positions.load(std::sync::atomic::Ordering::Relaxed));
    println!("positions kept {}", loaded.base.header.keys);
    println!("index file {:.1} MB", size as f64 / 1e6);
    println!("cold build {build_s:.1} s, peak RSS {} MB", peak_rss_mb());

    // Sample positions: a random ply, then a random game whose main line
    // reaches it, so that every ply is sampled as often.
    let mut rng = Rng(seed | 1);
    let mut sample: HashMap<u64, (Board, u32)> = HashMap::new();
    let mut tries = 0;
    while sample.len() < want && tries < want * 100 {
        tries += 1;
        let ply = (rng.next() % (u64::from(SAMPLE_PLIES) + 1)) as u32;
        let found = (0..1000).find_map(|_| {
            let id = (rng.next() % u64::from(n)) as u32 + 1;
            boards_of(&db, id)?.get(ply as usize).cloned()
        });
        if let Some(board) = found {
            sample.entry(board.hash()).or_insert((board, ply));
        }
    }
    println!("positions sampled {} at plies 0-{SAMPLE_PLIES}", sample.len());

    let keys: HashSet<u64> = sample.keys().copied().collect();
    let t = Instant::now();
    let (brute, over) = brute_force(&db, &loaded, &keys);
    println!("brute-force scan {:.1} s, {over} games over the move record limit left out", t.elapsed().as_secs_f64());

    // The explorer's answer, tree and deep section together, against it.
    let bands = [(0, u32::from(MAX_PLY)), (u32::from(MAX_PLY) + 1, SAMPLE_PLIES)];
    let mut per_band = [(0u64, 0u64); 2];
    let (mut equal, mut differ, mut top_differ, mut held, mut beyond) = (0, 0, 0, 0, 0);
    for (key, (board, ply)) in &sample {
        let b = brute.get(key).cloned().unwrap_or_default();
        let tree = loaded.lookup(*key).expect("lookup");
        let got = explorer::stats(&loaded, board, &Cancel::never()).expect("answer").unwrap_or_default();
        if let Some(tree) = &tree {
            held += 1;
            if got.counts.games > tree.counts.games {
                beyond += 1;
            }
        }
        let moves: HashMap<u16, Counts> = got.moves.iter().copied().collect();
        let same = got.counts == b.counts && moves == b.moves;
        let band = bands.iter().position(|&(lo, hi)| (lo..=hi).contains(ply)).unwrap_or(1);
        per_band[band].0 += 1;
        if same {
            equal += 1;
        } else {
            differ += 1;
            per_band[band].1 += 1;
        }
        if got.top != b.top.iter().map(|t| t.1).collect::<Vec<_>>() {
            top_differ += 1;
        }
    }
    println!("oracle: {equal} equal, {differ} different in games, results or moves");
    for ((lo, hi), (sampled, different)) in bands.iter().zip(per_band) {
        println!("  sampled at plies {lo}-{hi}: {sampled}, {different} different");
    }
    println!("  held by the tree {held}, {beyond} of them with games that reach them only beyond ply {MAX_PLY}");
    println!("notable games: {top_differ} different");

    // Warm latency of the explorer's answer: tree, deep section and rendering.
    let db = Base::TwoCbh(db);
    let answer = |b: &Board| {
        let stats = explorer::stats(&loaded, b, &Cancel::never()).expect("answer");
        render(&db, b, stats, &loaded)
    };
    let boards: Vec<&Board> = sample.values().map(|s| &s.0).collect();
    for b in &boards {
        let _ = answer(b);
    }
    let mut times: Vec<f64> = boards
        .iter()
        .map(|b| {
            let t = Instant::now();
            let _ = answer(b);
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| times[((times.len() as f64 - 1.0) * q) as usize];
    println!("warm answer: p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms", at(0.5), at(0.95), at(1.0));
}
