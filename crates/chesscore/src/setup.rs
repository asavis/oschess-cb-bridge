//! Building a position from parts, with the checks that keep it valid.

use std::fmt;

use crate::board::{Board, CastleSide};
use crate::types::{Color, Piece, Square};

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
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SetupError::KingCount => "each side needs exactly one king",
            SetupError::PawnOnBackRank => "a pawn stands on the first or last rank",
            SetupError::OpponentInCheck => "the side not to move is in check",
            SetupError::Castling => "a castling right has no king or rook in place",
            SetupError::EnPassant => "the en passant file has no pawn that just made a double step",
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
    /// passed and left empty.
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
        self.squares[Square::new(file, to).index()] == Some((Piece::Pawn, mover))
            && self.squares[Square::new(file, passed).index()].is_none()
            && self.squares[Square::new(file, from).index()].is_none()
    }

    pub fn build(&self) -> Result<Board, SetupError> {
        let mut b = Board::empty();
        for (i, piece) in self.squares.iter().enumerate() {
            if let Some((p, c)) = *piece {
                let sq = Square::from_index(i as u8).expect("64 squares");
                if p == Piece::Pawn && (sq.rank() == 0 || sq.rank() == 7) {
                    return Err(SetupError::PawnOnBackRank);
                }
                b.put(sq, p, c);
            }
        }
        for color in Color::ALL {
            if b.colored(Piece::King, color).count_ones() != 1 {
                return Err(SetupError::KingCount);
            }
        }
        b.set_side(self.side_to_move);
        if b.is_attacked(b.king(!self.side_to_move), self.side_to_move, b.occupied()) {
            return Err(SetupError::OpponentInCheck);
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
        Ok(b)
    }
}
