//! Legal move generation.
//!
//! Candidates are generated pseudo-legally and each is confirmed by playing it
//! on a copy of the board. Replaying a game never generates moves; this serves
//! mate detection, SAN disambiguation, input validation and perft, none of
//! which is on a hot path.

use crate::attacks;
use crate::board::{Board, CastleSide};
use crate::types::{Color, Move, Piece, Square, squares};

const PROMOTIONS: [Piece; 4] = [Piece::Queen, Piece::Rook, Piece::Bishop, Piece::Knight];

impl Board {
    /// Calls `f` with each legal move until it returns `true`; returns whether
    /// it did.
    pub fn any_legal_move(&self, mut f: impl FnMut(Move) -> bool) -> bool {
        self.pseudo_legal_moves(&mut |mv| self.is_legal(mv) && f(mv))
    }

    pub fn legal_moves(&self) -> Vec<Move> {
        let mut v = Vec::with_capacity(48);
        self.any_legal_move(|mv| {
            v.push(mv);
            false
        });
        v
    }

    pub fn has_legal_move(&self) -> bool {
        self.any_legal_move(|_| true)
    }

    /// Checkmate: in check with no legal move.
    pub fn is_checkmate(&self) -> bool {
        self.in_check() && !self.has_legal_move()
    }

    /// Leaf count of the legal move tree to `depth`.
    pub fn perft(&self, depth: u32) -> u64 {
        if depth == 0 {
            return 1;
        }
        let moves = self.legal_moves();
        if depth == 1 {
            return moves.len() as u64;
        }
        moves
            .into_iter()
            .map(|mv| {
                let mut b = self.clone();
                b.play_unchecked(mv);
                b.perft(depth - 1)
            })
            .sum()
    }

    /// Every pseudo-legal move, until `f` returns `true`.
    fn pseudo_legal_moves(&self, f: &mut impl FnMut(Move) -> bool) -> bool {
        let us = self.side_to_move();
        let own = self.colors(us);
        let enemy = self.colors(!us);
        let occupied = self.occupied();
        let mut emit = |from: Square, targets: u64| squares(targets).any(|to| f(Move::new(from, to, None)));

        for from in squares(self.colored(Piece::Knight, us)) {
            if emit(from, attacks::knight(from) & !own) {
                return true;
            }
        }
        for from in squares(self.colored(Piece::Bishop, us)) {
            if emit(from, attacks::bishop(from, occupied) & !own) {
                return true;
            }
        }
        for from in squares(self.colored(Piece::Rook, us)) {
            if emit(from, attacks::rook(from, occupied) & !own) {
                return true;
            }
        }
        for from in squares(self.colored(Piece::Queen, us)) {
            if emit(from, attacks::queen(from, occupied) & !own) {
                return true;
            }
        }
        let king = self.king(us);
        if emit(king, attacks::king(king) & !own) {
            return true;
        }
        for side in CastleSide::ALL {
            if let Some(file) = self.castling_rook(us, side)
                && f(Move::new(king, Square::new(file, us.back_rank()), None))
            {
                return true;
            }
        }

        let (step, start, last): (i8, u8, u8) = match us {
            Color::White => (1, 1, 7),
            Color::Black => (-1, 6, 0),
        };
        let ep = self.en_passant();
        for from in squares(self.colored(Piece::Pawn, us)) {
            let mut targets = attacks::pawn(us, from) & enemy;
            if let Some(ep) = ep {
                targets |= attacks::pawn(us, from) & ep.bit();
            }
            let one = Square::new(from.file(), (from.rank() as i8 + step) as u8);
            if occupied & one.bit() == 0 {
                targets |= one.bit();
                let two = Square::new(from.file(), (from.rank() as i8 + 2 * step) as u8);
                if from.rank() == start && occupied & two.bit() == 0 {
                    targets |= two.bit();
                }
            }
            for to in squares(targets) {
                if to.rank() == last {
                    if PROMOTIONS.iter().any(|&p| f(Move::new(from, to, Some(p)))) {
                        return true;
                    }
                } else if f(Move::new(from, to, None)) {
                    return true;
                }
            }
        }
        false
    }
}
