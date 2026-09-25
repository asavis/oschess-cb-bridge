//! A tree walk ends as soon as its visitor has what it needs (asavis/oschess-cb-bridge#81):
//! the rest of the tree is neither replayed nor checked.

use cbformat::fixture::{bytes, quiet};
use cbformat::fixture_cbh::{Tok, encode, move_record};
use cbformat::movetable::{self, Color, Piece};
use cbformat::replay::{self, TreeVisitor};
use cbformat::{cbh, v2};
use chesscore::{Board, Move};

/// Counts the moves announced, and has what it needs after `stop_after`.
struct Counter {
    plays: u32,
    stop_after: u32,
}

impl TreeVisitor for Counter {
    fn play(&mut self, _before: &Board, _mv: Option<Move>, _main_line: bool) {
        self.plays += 1;
    }

    fn stopped(&self) -> bool {
        self.plays >= self.stop_after
    }
}

#[test]
fn a_2cbh_walk_ends_when_the_visitor_stops() {
    // 1.e4 e5, then a word no position allows and no final end of line.
    let e4 = quiet(Color::White, Piece::Pawn, "e2", "e4");
    let e5 = quiet(Color::Black, Piece::Pawn, "e7", "e5");
    let content = bytes(&[movetable::MOVES, e4, e5, e4]);
    let moves = v2::GameMoves::parse(1, &content).unwrap();
    assert!(replay::walk(&moves, &mut Counter { plays: 0, stop_after: u32::MAX }).is_err());
    let mut counter = Counter { plays: 0, stop_after: 1 };
    assert!(replay::walk(&moves, &mut counter).is_ok(), "the damage after the stop is not read");
    assert_eq!(counter.plays, 1);
}

#[test]
fn a_classic_walk_ends_when_the_visitor_stops() {
    // 1.e4 e5 without the final end of line.
    let stream = encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::Mv("e7e5")], 0, false);
    let record = move_record(0, None, None, &stream);
    let moves = cbh::GameMoves::parse(&record).unwrap();
    assert!(cbh::walk(&moves, &mut Counter { plays: 0, stop_after: u32::MAX }).is_err());
    let mut counter = Counter { plays: 0, stop_after: 1 };
    assert!(cbh::walk(&moves, &mut counter).is_ok(), "the unterminated rest is not read");
    assert_eq!(counter.plays, 1);
}
