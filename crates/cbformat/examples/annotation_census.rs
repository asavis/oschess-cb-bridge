//! Counts the annotations of a database by type, without printing any of their
//! contents: types, languages, colours, types of unknown layout, damaged
//! records, and positions that name no move.
//!
//! It also tests the square numbering of arrows. For arrows on games without
//! variations, it counts how often the arrow's origin holds a piece after the
//! annotated move, reading the squares file by file and, as a control, rank by
//! rank.
//!
//! `cargo run --release -p cbformat --example annotation_census -- <database> [threads]`

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use cbformat::replay::{self, TreeVisitor};
use cbformat::v2::{Annotation, Database, GAME_POSITION, RecordKind};
use chesscore::{Board, Move, Square};

#[derive(Default)]
struct Census {
    records: u64,
    games: u64,
    annotated: u64,
    incomplete: BTreeMap<u16, u64>,
    incomplete_ids: Vec<(u16, u32)>,
    damaged: u64,
    damage_examples: Vec<String>,
    unreadable_moves: u64,
    types: BTreeMap<u16, u64>,
    languages: BTreeMap<u16, u64>,
    square_colours: BTreeMap<u8, u64>,
    arrow_colours: BTreeMap<u8, u64>,
    nags: [BTreeMap<u8, u64>; 3],
    orphans: u64,
    orphan_games: u64,
    game_level_symbols: u64,
    arrows_tested: u64,
    origin_occupied_file_major: u64,
    origin_occupied_rank_major: u64,
}

impl Census {
    fn add(&mut self, o: Census) {
        self.records += o.records;
        self.games += o.games;
        self.annotated += o.annotated;
        self.damaged += o.damaged;
        self.incomplete_ids.extend(o.incomplete_ids);
        self.unreadable_moves += o.unreadable_moves;
        self.orphans += o.orphans;
        self.orphan_games += o.orphan_games;
        self.game_level_symbols += o.game_level_symbols;
        self.arrows_tested += o.arrows_tested;
        self.origin_occupied_file_major += o.origin_occupied_file_major;
        self.origin_occupied_rank_major += o.origin_occupied_rank_major;
        for e in o.damage_examples {
            if self.damage_examples.len() < 10 {
                self.damage_examples.push(e);
            }
        }
        merge(&mut self.incomplete, o.incomplete);
        merge(&mut self.types, o.types);
        merge(&mut self.languages, o.languages);
        merge(&mut self.square_colours, o.square_colours);
        merge(&mut self.arrow_colours, o.arrow_colours);
        for (a, b) in self.nags.iter_mut().zip(o.nags) {
            merge(a, b);
        }
    }
}

fn merge<K: Ord>(a: &mut BTreeMap<K, u64>, b: BTreeMap<K, u64>) {
    for (k, v) in b {
        *a.entry(k).or_default() += v;
    }
}

/// The board after each move of a game's main line.
#[derive(Default)]
struct Boards(Vec<Board>);

impl TreeVisitor for Boards {
    fn play(&mut self, _before: &Board, _mv: Option<Move>, _main_line: bool) {}
    fn played(&mut self, after: &Board) {
        self.0.push(after.clone());
    }
}

/// Square `v` (numbered from 1) read file by file and, as a control, rank by rank.
fn orders(v: u8) -> (Square, Square) {
    let i = v - 1;
    (Square::new(i / 8, i % 8), Square::new(i % 8, i / 8))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = Database::open(&args[1]).expect("open");
    let threads: usize = args.get(2).and_then(|t| t.parse().ok()).unwrap_or(4);
    let n = db.record_count();
    let next = AtomicU64::new(1);
    let total = Mutex::new(Census::default());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut c = Census::default();
                loop {
                    let first = next.fetch_add(4096, Ordering::Relaxed);
                    if first > u64::from(n) {
                        break;
                    }
                    let last = (first + 4095).min(u64::from(n)) as u32;
                    let Ok(batch) = db.batch(first as u32, last) else {
                        c.damaged += u64::from(last) - first + 1;
                        continue;
                    };
                    for id in first as u32..=last {
                        c.records += 1;
                        let Ok(r) = batch.record(id) else { continue };
                        if !matches!(r.kind(), RecordKind::Game | RecordKind::Analysis) {
                            continue;
                        }
                        c.games += 1;
                        let ann = match batch.annotations_of(&r) {
                            Ok(Some(a)) => a,
                            Ok(None) => continue,
                            Err(e) => {
                                c.damaged += 1;
                                if c.damage_examples.len() < 10 {
                                    c.damage_examples.push(format!("record {id}: {e}"));
                                }
                                continue;
                            }
                        };
                        if ann.is_empty() {
                            continue;
                        }
                        c.annotated += 1;
                        if let Some(u) = ann.stopped_at {
                            *c.incomplete.entry(u.type_code).or_default() += 1;
                            c.incomplete_ids.push((u.type_code, id));
                        }
                        let moves = batch.moves_of(&r).and_then(|d| {
                            let m = d.moves()?;
                            let mut boards = Boards::default();
                            let stats = replay::walk(&m, &mut boards)?;
                            Ok((stats, boards.0))
                        });
                        let Ok((stats, boards)) = moves else {
                            c.unreadable_moves += 1;
                            continue;
                        };
                        let mut orphan = false;
                        for b in &ann.blocks {
                            if b.position < GAME_POSITION || b.position >= stats.total_plies as i32 {
                                orphan = true;
                                c.orphans += b.annotations.len() as u64;
                            }
                            for a in &b.annotations {
                                let code = match a {
                                    Annotation::Text { before, language, .. } => {
                                        *c.languages.entry(*language).or_default() += 1;
                                        if *before { 0x82 } else { 0x02 }
                                    }
                                    Annotation::Symbols { on_move, on_position, prefix } => {
                                        // PGN has no place for a NAG before the first move.
                                        c.game_level_symbols += (b.position == GAME_POSITION) as u64;
                                        for (i, v) in [on_move, on_position, prefix].into_iter().enumerate() {
                                            *c.nags[i].entry(*v).or_default() += 1;
                                        }
                                        0x03
                                    }
                                    Annotation::Squares(v) => {
                                        for s in v {
                                            *c.square_colours.entry(s.colour).or_default() += 1;
                                        }
                                        0x04
                                    }
                                    Annotation::Arrows(v) => {
                                        for a in v {
                                            *c.arrow_colours.entry(a.colour).or_default() += 1;
                                        }
                                        // Games without variations: position p is
                                        // the p-th move of the main line.
                                        if stats.lines == 1
                                            && b.position >= 0
                                            && let Some(board) = boards.get(b.position as usize)
                                        {
                                            for a in v {
                                                // Back to numbered-from-1, file by file.
                                                let cb = (a.from % 8) * 8 + a.from / 8 + 1;
                                                let (fm, rm) = orders(cb);
                                                c.arrows_tested += 1;
                                                c.origin_occupied_file_major += board.piece_at(fm).is_some() as u64;
                                                c.origin_occupied_rank_major += board.piece_at(rm).is_some() as u64;
                                            }
                                        }
                                        0x05
                                    }
                                    Annotation::Other { code, .. } => *code,
                                };
                                *c.types.entry(code).or_default() += 1;
                            }
                        }
                        c.orphan_games += orphan as u64;
                    }
                }
                total.lock().unwrap().add(c);
            });
        }
    });
    let c = total.into_inner().unwrap();
    println!("records                {}", c.records);
    println!("games and analyses     {}", c.games);
    println!("with annotations       {}", c.annotated);
    println!("damaged records        {}", c.damaged);
    for e in &c.damage_examples {
        println!("  {e}");
    }
    println!("unreadable move trees  {}", c.unreadable_moves);
    println!("incomplete (by type)   {:?}", hex(&c.incomplete));
    let mut ids = c.incomplete_ids.clone();
    ids.sort();
    for t in c.incomplete.keys() {
        let first: Vec<u32> = ids.iter().filter(|(k, _)| k == t).map(|&(_, id)| id).take(8).collect();
        println!("  {t:#04x} in records {first:?}");
    }
    println!("annotations by type    {:?}", hex(&c.types));
    println!("text languages         {:?}", c.languages);
    println!("square colours         {:?}", c.square_colours);
    println!("arrow colours          {:?}", c.arrow_colours);
    println!("NAGs on move           {:?}", c.nags[0]);
    println!("NAGs on position       {:?}", c.nags[1]);
    println!("NAG prefixes           {:?}", c.nags[2]);
    println!("orphan annotations     {} in {} games", c.orphans, c.orphan_games);
    println!("symbols on the game    {}", c.game_level_symbols);
    println!(
        "arrow origins occupied {} tested: file by file {}, rank by rank {}",
        c.arrows_tested, c.origin_occupied_file_major, c.origin_occupied_rank_major
    );
}

fn hex(m: &BTreeMap<u16, u64>) -> BTreeMap<String, u64> {
    m.iter().map(|(k, v)| (format!("{k:#04x}"), *v)).collect()
}
