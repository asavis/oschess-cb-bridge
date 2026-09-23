//! Why a move word cannot be played where it stands.

use std::fmt;

use chesscore::{Color as CColor, IllegalMove, Move, Piece as CPiece, Square};

use crate::movetable::Captured;

/// Why a move word cannot be played where it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveError {
    /// The word is neither a move word nor the null move.
    NotAMoveWord(u16),
    /// The null move, given to [`to_move`].
    NullMove,
    /// A null move while the side to move is in check.
    NullMoveInCheck,
    /// A castling word for the side not to move.
    CastlingOutOfTurn { word: u16 },
    /// A castling word without the castling right.
    NoCastlingRight { word: u16 },
    /// A move word of `color` when the other side is to move.
    OutOfTurn { word: u16, color: CColor },
    /// The word's piece is not on its origin.
    NoPiece { word: u16, color: CColor, piece: CPiece, from: Square },
    /// The destination holds a piece of the mover's colour.
    OntoOwnPiece { word: u16, color: CColor, from: Square, to: Square },
    /// The destination does not hold what the word says it captures.
    WrongCapture { word: u16, from: Square, to: Square, named: Captured, found: Option<(CPiece, CColor)> },
    /// An en passant capture where none is available.
    NoEnPassant { word: u16, from: Square, to: Square },
    /// The word agrees with the position but the move is illegal.
    Illegal { word: u16, mv: Move, why: IllegalMove },
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MoveError::NotAMoveWord(word) => write!(f, "{word:#06x} is not a move word"),
            MoveError::NullMove => f.write_str("null move"),
            MoveError::NullMoveInCheck => f.write_str("null move while in check"),
            MoveError::CastlingOutOfTurn { word } => write!(f, "{word:#06x}: castling for the side not to move"),
            MoveError::NoCastlingRight { word } => write!(f, "{word:#06x}: castling without the right"),
            MoveError::OutOfTurn { word, color } => write!(f, "{word:#06x}: {color:?} move with {:?} to move", !color),
            MoveError::NoPiece { word, color, piece, from } => {
                write!(f, "{word:#06x}: no {color:?} {piece:?} on {from}")
            }
            MoveError::OntoOwnPiece { word, color, from, to } => {
                write!(f, "{word:#06x}: {from}{to} lands on a {color:?} piece")
            }
            MoveError::WrongCapture { word, from, to, named, found } => {
                write!(f, "{word:#06x}: {from}{to} names capture {named:?}, square holds {found:?}")
            }
            MoveError::NoEnPassant { word, from, to } => write!(f, "{word:#06x}: en passant {from}{to} not available"),
            MoveError::Illegal { word, mv, why } => write!(f, "{word:#06x}: {mv}: {why}"),
        }
    }
}

impl std::error::Error for MoveError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_unchanged() {
        let (e4, e5, d6) = (Square::new(4, 3), Square::new(4, 4), Square::new(3, 5));
        let cases = [
            (MoveError::NotAMoveWord(0xc02d), "0xc02d is not a move word"),
            (MoveError::NullMove, "null move"),
            (MoveError::NullMoveInCheck, "null move while in check"),
            (MoveError::CastlingOutOfTurn { word: 0xb12a }, "0xb12a: castling for the side not to move"),
            (MoveError::NoCastlingRight { word: 0xb12a }, "0xb12a: castling without the right"),
            (MoveError::OutOfTurn { word: 0x0001, color: CColor::White }, "0x0001: White move with Black to move"),
            (
                MoveError::NoPiece { word: 0x0001, color: CColor::Black, piece: CPiece::Knight, from: e4 },
                "0x0001: no Black Knight on e4",
            ),
            (
                MoveError::OntoOwnPiece { word: 0x0001, color: CColor::White, from: e4, to: e5 },
                "0x0001: e4e5 lands on a White piece",
            ),
            (
                MoveError::WrongCapture {
                    word: 0x0001,
                    from: e4,
                    to: e5,
                    named: Captured::Knight,
                    found: Some((CPiece::Pawn, CColor::Black)),
                },
                "0x0001: e4e5 names capture Knight, square holds Some((Pawn, Black))",
            ),
            (MoveError::NoEnPassant { word: 0xad67, from: e5, to: d6 }, "0xad67: en passant e5d6 not available"),
            (
                MoveError::Illegal { word: 0x0001, mv: Move::new(e4, e5, None), why: IllegalMove::LeavesKingInCheck },
                &format!("0x0001: e4e5: {}", IllegalMove::LeavesKingInCheck),
            ),
        ];
        for (e, text) in cases {
            assert_eq!(e.to_string(), text);
        }
    }
}
