//! `info` and `verify` for classic (`.cbh`) databases, with the same output
//! as for 2CBH ones, annotations included.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cbformat::cbh::{self, Batch, Database};
use cbformat::replay::TreeVisitor;
use cbformat::v2::{RecordKind, Start};
use chesscore::{Board, Move, Piece};

use super::{AnyResult, RUN, Stats, report, run_ids, run_workers, threads};

pub(crate) fn info(path: &str) -> AnyResult<bool> {
    let db = Database::open(path)?;
    println!("records        {}", db.record_count());
    println!("format version {}", db.format_version());
    let counts = db.entities().counts();
    for (name, n) in ["players", "tournaments", "annotators", "sources"].iter().zip(counts) {
        println!("{name:<14} {n}");
    }
    Ok(true)
}

/// Counts what `verify` reports about the moves, from the position before each.
struct Counter<'s>(&'s mut Stats);

impl TreeVisitor for Counter<'_> {
    fn play(&mut self, before: &Board, mv: Option<Move>, _main_line: bool) {
        let Some(mv) = mv else {
            self.0.null_moves += 1;
            return;
        };
        let moving = before.piece_at(mv.from);
        let target = before.piece_at(mv.to);
        let pawn = matches!(moving, Some((Piece::Pawn, _)));
        if pawn && mv.from.file() != mv.to.file() && target.is_none() {
            self.0.en_passant += 1;
        }
        if let (Some(p), Some((captured, c))) = (mv.promotion, target)
            && moving.is_some_and(|(_, m)| m != c)
        {
            self.0.promo_captures += 1;
            if captured != p {
                self.0.promo_captures_distinct += 1;
            }
        }
    }
}

fn verify_record(batch: &Batch<'_>, id: u32, s: &mut Stats, failures: &Mutex<Vec<(u32, String)>>) {
    let fail = |s: &mut Stats, msg: String| {
        s.failures += 1;
        let mut f = failures.lock().unwrap_or_else(|e| e.into_inner());
        if f.len() < 50 {
            f.push((id, msg));
        }
    };
    let r = match batch.record(id) {
        Ok(r) => r,
        Err(e) => return fail(s, e.to_string()),
    };
    if r.is_deleted() {
        s.deleted += 1;
    }
    match r.kind() {
        RecordKind::Game => s.games += 1,
        RecordKind::Text => {
            s.texts += 1;
            return;
        }
        RecordKind::Analysis | RecordKind::Unknown(_) => {
            s.unknown_kind += 1;
            return;
        }
    }
    let data = match batch.moves_of(&r) {
        Ok(d) => d,
        Err(e) => return fail(s, e.to_string()),
    };
    let game = match data.moves() {
        Ok(g) => g,
        Err(e) => return fail(s, e.to_string()),
    };
    if game.is_chess960() {
        s.chess960 += 1;
    }
    if matches!(game.start(), Ok(Start::Setup(_))) {
        s.setups += 1;
    }
    let plies = match cbh::walk(&game, &mut Counter(s)) {
        Ok(t) => {
            s.main_plies += u64::from(t.main_line_plies);
            s.total_plies += u64::from(t.total_plies);
            t.total_plies
        }
        Err(e) => return fail(s, e.to_string()),
    };
    // Every annotation type has its size, so a classic record is never left
    // incomplete: it decodes, or it is damaged.
    match batch.annotations_of(&r) {
        Ok(Some(a)) if !a.is_empty() => {
            s.annotated += 1;
            if let Err(e) = a.check_positions(plies) {
                fail(s, format!("annotations: {e}"));
            }
        }
        Ok(_) => {}
        Err(e) => fail(s, format!("annotations: {e}")),
    }
}

pub(crate) fn verify(path: &str, limit: Option<u32>) -> AnyResult<bool> {
    let db = Database::open(path)?;
    let n = limit.map_or(db.record_count(), |l| l.min(db.record_count()));
    let failures: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
    let started = Instant::now();
    let next_run = AtomicU64::new(0);
    let total = Mutex::new(Stats::default());
    let runs = u64::from(n).div_ceil(u64::from(RUN));
    if runs > 0 {
        run_workers(threads().min(runs as usize), &|| {
            let mut s = Stats::default();
            while let Some(ids) = run_ids(next_run.fetch_add(1, Ordering::Relaxed), n) {
                let Ok(batch) = db.batch(*ids.start(), *ids.end()).or_else(|_| db.batch(1, 0)) else { continue };
                for id in ids {
                    verify_record(&batch, id, &mut s, &failures);
                }
            }
            total.lock().unwrap_or_else(|e| e.into_inner()).add(&s);
        });
    }
    let stats = total.into_inner().unwrap_or_else(|e| e.into_inner());
    Ok(report(n, started, &stats, failures, Some(db.has_annotations())))
}
