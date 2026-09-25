//! What `info` and `verify` do differently for classic (`.cbh`) databases:
//! their entity tables, and counting the moves from the positions before them.
//! The run and the report are the 2CBH ones, through `view` (#66).

use std::sync::Mutex;

use cbformat::cbh::{self, Batch, Database};
use cbformat::game::{RecordKind, Start};
use cbformat::replay::TreeVisitor;
use chesscore::{Board, Move, Piece};

use super::Stats;

/// The lines of `info` after the record count.
pub(crate) fn info(db: &Database) {
    println!("format version {}", db.format_version());
    let counts = db.entities().counts();
    for (name, n) in ["players", "tournaments", "annotators", "sources"].iter().zip(counts) {
        println!("{name:<14} {n}");
    }
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

pub(crate) fn verify_record(batch: &Batch<'_>, id: u32, s: &mut Stats, failures: &Mutex<Vec<(u32, String)>>) {
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
            if let Err(e) = a.check_positions(plies).map(|n| s.count_past_end(n)) {
                fail(s, format!("annotations: {e}"));
            }
        }
        Ok(_) => {}
        Err(e) => fail(s, format!("annotations: {e}")),
    }
}
