//! Colours, pieces, squares, bitboards and moves.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Color {
    White,
    Black,
}

impl Color {
    pub const ALL: [Color; 2] = [Color::White, Color::Black];

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The rank a side's pieces start on, 0 or 7.
    #[inline]
    pub const fn back_rank(self) -> u8 {
        match self {
            Color::White => 0,
            Color::Black => 7,
        }
    }
}

impl std::ops::Not for Color {
    type Output = Color;
    #[inline]
    fn not(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Piece {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}

impl Piece {
    pub const ALL: [Piece; 6] = [Piece::Pawn, Piece::Knight, Piece::Bishop, Piece::Rook, Piece::Queen, Piece::King];

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    pub const fn from_index(i: usize) -> Option<Piece> {
        match i {
            0 => Some(Piece::Pawn),
            1 => Some(Piece::Knight),
            2 => Some(Piece::Bishop),
            3 => Some(Piece::Rook),
            4 => Some(Piece::Queen),
            5 => Some(Piece::King),
            _ => None,
        }
    }

    /// The upper-case letter of the piece, as SAN and a white FEN piece use it.
    pub const fn letter(self) -> char {
        match self {
            Piece::Pawn => 'P',
            Piece::Knight => 'N',
            Piece::Bishop => 'B',
            Piece::Rook => 'R',
            Piece::Queen => 'Q',
            Piece::King => 'K',
        }
    }

    /// A FEN piece letter: upper case white, lower case black.
    pub fn from_fen_char(c: char) -> Option<(Piece, Color)> {
        let color = if c.is_ascii_uppercase() { Color::White } else { Color::Black };
        let piece = match c.to_ascii_uppercase() {
            'P' => Piece::Pawn,
            'N' => Piece::Knight,
            'B' => Piece::Bishop,
            'R' => Piece::Rook,
            'Q' => Piece::Queen,
            'K' => Piece::King,
            _ => return None,
        };
        Some((piece, color))
    }
}

/// A square, `a1` = 0, `b1` = 1, … `h8` = 63.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Square(u8);

impl Square {
    /// The square on `file` (0-7, `a`-`h`) and `rank` (0-7). Both are taken
    /// modulo 8.
    #[inline]
    pub const fn new(file: u8, rank: u8) -> Square {
        Square((rank & 7) << 3 | (file & 7))
    }

    #[inline]
    pub const fn from_index(i: u8) -> Option<Square> {
        if i < 64 { Some(Square(i)) } else { None }
    }

    /// For indices already known to be below 64, such as a bit of a bitboard.
    #[inline]
    pub(crate) const fn at(i: u32) -> Square {
        Square((i & 63) as u8)
    }

    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    #[inline]
    pub const fn file(self) -> u8 {
        self.0 & 7
    }

    #[inline]
    pub const fn rank(self) -> u8 {
        self.0 >> 3
    }

    #[inline]
    pub const fn bit(self) -> Bitboard {
        1 << self.0
    }
}

impl fmt::Display for Square {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", (b'a' + self.file()) as char, (b'1' + self.rank()) as char)
    }
}

impl fmt::Debug for Square {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Square {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Square, ParseError> {
        match s.as_bytes() {
            &[f @ b'a'..=b'h', r @ b'1'..=b'8'] => Ok(Square::new(f - b'a', r - b'1')),
            _ => Err(ParseError(format!("bad square {}", excerpt(s)))),
        }
    }
}

/// A set of squares, one bit per square.
pub type Bitboard = u64;

/// The squares of a bitboard in ascending order.
#[derive(Clone, Copy)]
pub struct Squares(Bitboard);

impl Iterator for Squares {
    type Item = Square;
    #[inline]
    fn next(&mut self) -> Option<Square> {
        if self.0 == 0 {
            return None;
        }
        let sq = Square::at(self.0.trailing_zeros());
        self.0 &= self.0 - 1;
        Some(sq)
    }
}

#[inline]
pub fn squares(b: Bitboard) -> Squares {
    Squares(b)
}

/// A move. Castling is the king moving onto its own rook, which describes
/// standard and Chess960 castling alike (the UCI_Chess960 convention).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Move {
    pub from: Square,
    pub to: Square,
    pub promotion: Option<Piece>,
}

impl Move {
    pub const fn new(from: Square, to: Square, promotion: Option<Piece>) -> Move {
        Move { from, to, promotion }
    }
}

/// UCI in the king-takes-rook form for castling.
impl fmt::Display for Move {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.from, self.to)?;
        if let Some(p) = self.promotion {
            write!(f, "{}", p.letter().to_ascii_lowercase())?;
        }
        Ok(())
    }
}

impl FromStr for Move {
    type Err = ParseError;
    /// UCI, `e2e4` or `e7e8q`; castling as the king taking its own rook.
    fn from_str(s: &str) -> Result<Move, ParseError> {
        if !(4..=5).contains(&s.len()) || !s.is_ascii() {
            return Err(ParseError(format!("bad move {}", excerpt(s))));
        }
        let from = s[0..2].parse()?;
        let to = s[2..4].parse()?;
        let promotion = match s.as_bytes().get(4) {
            None => None,
            Some(b'n') => Some(Piece::Knight),
            Some(b'b') => Some(Piece::Bishop),
            Some(b'r') => Some(Piece::Rook),
            Some(b'q') => Some(Piece::Queen),
            Some(_) => return Err(ParseError(format!("bad promotion in {}", excerpt(s)))),
        };
        Ok(Move { from, to, promotion })
    }
}

/// At most the first 16 characters of `s`, quoted, for an error message: a
/// message must not grow with a malformed input.
pub(crate) fn excerpt(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(16).collect();
    if chars.next().is_some() { format!("{head:?}…") } else { format!("{head:?}") }
}

/// A square, move or FEN that could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squares_and_moves_round_trip() {
        for i in 0..64u8 {
            let sq = Square::from_index(i).unwrap();
            assert_eq!(sq.to_string().parse::<Square>().unwrap(), sq);
            assert_eq!(Square::new(sq.file(), sq.rank()), sq);
        }
        assert_eq!("e4".parse::<Square>().unwrap().index(), 28);
        assert!("i1".parse::<Square>().is_err());
        assert!("a9".parse::<Square>().is_err());
        for s in ["e2e4", "e7e8q", "a2a1n", "e1h1"] {
            assert_eq!(s.parse::<Move>().unwrap().to_string(), s);
        }
        assert!("e7e8k".parse::<Move>().is_err());
        assert!("e2e".parse::<Move>().is_err());
    }

    #[test]
    fn bitboard_iteration_is_ascending() {
        let b = Square::new(0, 0).bit() | Square::new(7, 7).bit() | Square::new(4, 3).bit();
        let v: Vec<String> = squares(b).map(|s| s.to_string()).collect();
        assert_eq!(v, ["a1", "e4", "h8"]);
    }
}
