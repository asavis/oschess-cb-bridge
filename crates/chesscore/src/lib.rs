//! A small chess core: positions, legal moves, FEN, Chess960 and the Polyglot
//! position key.
//!
//! It is built for replaying stored games. [`Board::play_checked`] checks and
//! plays one move in place, with no move generation and no copy of the board;
//! legal move generation exists for mate detection, notation and validation.
//! Castling is the king taking its own rook, which covers Chess960.

pub mod attacks;
mod board;
pub mod chess960;
mod fen;
mod movegen;
mod setup;
mod types;
mod zobrist;

pub use board::{Board, CastleSide, IllegalMove};
pub use fen::FenError;
pub use setup::{BoardBuilder, SetupError};
pub use types::{Bitboard, Color, Move, ParseError, Piece, Square, Squares, squares};
