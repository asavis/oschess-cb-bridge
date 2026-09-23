//! Building a position from parts, with the checks that keep it valid.

use std::fmt;

use crate::attacks;
use crate::board::{Board, CastleSide};
use crate::types::{Bitboard, Color, Piece, Square, squares};

const RANK_1: Bitboard = 0xff;
const RANK_8: Bitboard = 0xff << 56;

/// The parts of a position, checked by [`BoardBuilder::build`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardBuilder {
    pub squares: [Option<(Piece, Color)>; 64],
    pub side_to_move: Color,
    /// The file of the castling rook, by colour and [`CastleSide`].
    pub castling: [[Option<u8>; 2]; 2],
    /// The file of the pawn that has just made a double step.
    pub en_passant_file: Option<u8>,
    pub halfmove_clock: u16,
    pub fullmove_number: u16,
    pub chess960: bool,
}

/// Why a set of parts is not a valid position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// A side has no king or more than one.
    KingCount,
    /// A pawn stands on the first or last rank.
    PawnOnBackRank,
    /// The side not to move is in check.
    OpponentInCheck,
    /// A castling right without its king and rook in place.
    Castling,
    /// An en passant file with no pawn that can have just made a double step.
    EnPassant,
    /// A side has more than sixteen pieces or more than eight pawns.
    TooManyPieces,
    /// The side to move is checked by more than two pieces.
    ImpossibleCheck,
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SetupError::KingCount => "each side needs exactly one king",
            SetupError::PawnOnBackRank => "a pawn stands on the first or last rank",
            SetupError::OpponentInCheck => "the side not to move is in check",
            SetupError::Castling => "a castling right has no king or rook in place",
            SetupError::EnPassant => "the en passant file has no pawn that just made a double step",
            SetupError::TooManyPieces => "a side has more than sixteen pieces or more than eight pawns",
            SetupError::ImpossibleCheck => "the side to move is checked by more than two pieces",
        })
    }
}

impl std::error::Error for SetupError {}

impl Default for BoardBuilder {
    fn default() -> Self {
        BoardBuilder::empty()
    }
}

impl BoardBuilder {
    /// No pieces, white to move, move 1.
    pub fn empty() -> BoardBuilder {
        BoardBuilder {
            squares: [None; 64],
            side_to_move: Color::White,
            castling: [[None; 2]; 2],
            en_passant_file: None,
            halfmove_clock: 0,
            fullmove_number: 1,
            chess960: false,
        }
    }

    pub fn set(&mut self, sq: Square, piece: Option<(Piece, Color)>) {
        self.squares[sq.index()] = piece;
    }

    /// Whether a castling right has its king and rook where it needs them.
    pub fn castling_is_valid(&self, color: Color, side: CastleSide) -> bool {
        let Some(rook) = self.castling[color.index()][side as usize] else { return true };
        let back = color.back_rank();
        let king = (0..8).find(|&f| self.squares[Square::new(f, back).index()] == Some((Piece::King, color)));
        let Some(king) = king else { return false };
        rook < 8
            && self.squares[Square::new(rook, back).index()] == Some((Piece::Rook, color))
            && match side {
                CastleSide::Short => rook > king,
                CastleSide::Long => rook < king,
            }
    }

    /// Whether the en passant file names a pawn of the side not to move that
    /// can have just made a double step: the pawn in place, the squares it
    /// passed and left empty, and every check on the side to move given by
    /// that pawn or opened through the square it left.
    pub fn en_passant_is_valid(&self) -> bool {
        let Some(file) = self.en_passant_file else { return true };
        if file >= 8 {
            return false;
        }
        let mover = !self.side_to_move;
        let (from, passed, to) = match mover {
            Color::White => (1, 2, 3),
            Color::Black => (6, 5, 4),
        };
        let (source, pawn) = (Square::new(file, from), Square::new(file, to));
        if self.squares[pawn.index()] != Some((Piece::Pawn, mover))
            || self.squares[Square::new(file, passed).index()].is_some()
            || self.squares[source.index()].is_some()
        {
            return false;
        }
        let b = self.placement();
        let king = self.squares.iter().position(|&p| p == Some((Piece::King, self.side_to_move)));
        let Some(king) = king.and_then(|k| Square::from_index(k as u8)) else { return false };
        let checkers = b.attackers_to(king, b.occupied()) & b.colors(mover);
        squares(checkers).all(|c| c == pawn || attacks::between(c, king) & source.bit() != 0)
    }

    /// The pieces alone on a board, the side to move set.
    fn placement(&self) -> Board {
        let mut b = Board::empty();
        for (i, piece) in self.squares.iter().enumerate() {
            if let (Some((p, c)), Some(sq)) = (*piece, Square::from_index(i as u8)) {
                b.put(sq, p, c);
            }
        }
        b.set_side(self.side_to_move);
        b
    }

    pub fn build(&self) -> Result<Board, SetupError> {
        let mut b = self.placement();
        if b.pieces(Piece::Pawn) & (RANK_1 | RANK_8) != 0 {
            return Err(SetupError::PawnOnBackRank);
        }
        for color in Color::ALL {
            if b.colored(Piece::King, color).count_ones() != 1 {
                return Err(SetupError::KingCount);
            }
            if b.colors(color).count_ones() > 16 || b.colored(Piece::Pawn, color).count_ones() > 8 {
                return Err(SetupError::TooManyPieces);
            }
        }
        if b.is_attacked(b.king(!self.side_to_move), self.side_to_move, b.occupied()) {
            return Err(SetupError::OpponentInCheck);
        }
        let us = self.side_to_move;
        if (b.attackers_to(b.king(us), b.occupied()) & b.colors(!us)).count_ones() > 2 {
            return Err(SetupError::ImpossibleCheck);
        }
        for color in Color::ALL {
            for side in CastleSide::ALL {
                if !self.castling_is_valid(color, side) {
                    return Err(SetupError::Castling);
                }
                b.set_castling(color, side, self.castling[color.index()][side as usize]);
            }
        }
        if !self.en_passant_is_valid() {
            return Err(SetupError::EnPassant);
        }
        b.set_ep_file(self.en_passant_file);
        b.set_clocks(self.halfmove_clock, self.fullmove_number);
        b.set_chess960(self.chess960);
        b.refresh_checkers();
        Ok(b)
    }
}
