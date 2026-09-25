//! Where a game starts: the standard position, a Chess960 one, or a set-up
//! position, as both formats describe it.

use crate::movetable::{Color, Piece, Sq};

/// Where a game starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Standard,
    Chess960(u16),
    Setup(Setup),
}

/// A set-up start position, decoded from the start-position section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    pub chess960: bool,
    pub move_number: u16,
    pub side_to_move: Color,
    /// Castling rights from the high byte of the second word. Bits: 1 white
    /// O-O-O, 2 white O-O, 4 black O-O-O, 8 black O-O.
    pub castling: u8,
    /// The file (0-7) of the rook each right castles with, when the record
    /// names it, in the order of the `castling` bits: white O-O-O, white O-O,
    /// black O-O-O, black O-O. A right without one castles with the outermost
    /// rook on its wing in Chess960, and with the corner rook otherwise.
    pub castling_rooks: [Option<u8>; 4],
    /// The file (0-7) of the square each side's king must stand on to keep
    /// its castling rights, when the record names it: white, black.
    pub castling_kings: [Option<u8>; 2],
    /// En passant file 0-7 (`a`-`h`), when the third word names one.
    pub en_passant_file: Option<u8>,
    /// The third word itself. Zero in every set-up position examined; read as
    /// an en passant file 1-8 when it holds one, which is **unconfirmed**.
    pub en_passant_raw: u16,
    pub pieces: Vec<(Sq, Color, Piece)>,
}
