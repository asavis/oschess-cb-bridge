//! What `info` and `verify` do differently for classic (`.cbh`) databases:
//! their entity tables, and counting the moves from the positions before them.
//! The run, the record's reading and the report are the 2CBH ones
//! ([`crate::verify`], through `view`, #66).

use cbformat::cbh::{self, Batch, Database, Record};
use cbformat::game::GameAnnotations;
use cbformat::replay::{TreeStats, TreeVisitor};
use chesscore::{Board, Move, Piece};

use crate::LIMITS;
use crate::verify::{Records, Stats};

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

/// A classic database counts its moves from the position before each: its
/// move records store no captured piece. It has no analyses ([`Record::kind`]).
impl Records for Batch<'_> {
    type Record = Record;

    fn record(&self, id: u32) -> cbformat::Result<Record> {
        Batch::record(self, id)
    }

    fn moves(&self, r: &Record, s: &mut Stats) -> cbformat::Result<TreeStats> {
        let data = self.moves_of_within(r, LIMITS.game_bytes)?;
        let game = data.moves()?;
        s.count_start(game.is_chess960(), game.start());
        cbh::walk(&game, &mut Counter(s))
    }

    fn annotations(&self, r: &Record) -> cbformat::Result<Option<GameAnnotations>> {
        self.annotations_of_within(r, LIMITS.game_bytes)
    }
}
