//! FEN and Shredder-FEN.

use std::fmt;
use std::str::FromStr;

use crate::board::{Board, CastleSide};
use crate::setup::{BoardBuilder, SetupError};
use crate::types::{Color, ParseError, Piece, Square};

/// A FEN that does not parse, or parses to an invalid position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenError {
    Syntax(ParseError),
    Setup(SetupError),
}

impl fmt::Display for FenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FenError::Syntax(e) => write!(f, "bad FEN: {e}"),
            FenError::Setup(e) => write!(f, "invalid position: {e}"),
        }
    }
}

impl std::error::Error for FenError {}

fn syntax(msg: impl Into<String>) -> FenError {
    FenError::Syntax(ParseError(msg.into()))
}

impl Board {
    /// Parses FEN. Castling may be written `KQkq` (the outermost rook on that
    /// side of the king) or with rook files `A`-`H` / `a`-`h` (Shredder-FEN).
    /// The halfmove clock and move number may be left out. A position whose
    /// castling needs Chess960 rules is marked as Chess960.
    pub fn from_fen(fen: &str) -> Result<Board, FenError> {
        let fields: Vec<&str> = fen.split_whitespace().collect();
        if !(4..=6).contains(&fields.len()) {
            return Err(syntax(format!("{} fields", fields.len())));
        }
        let mut b = BoardBuilder::empty();
        let ranks: Vec<&str> = fields[0].split('/').collect();
        if ranks.len() != 8 {
            return Err(syntax("placement needs 8 ranks"));
        }
        for (i, rank_text) in ranks.iter().enumerate() {
            let rank = 7 - i as u8;
            let mut file = 0u8;
            for c in rank_text.chars() {
                if let Some(d) = c.to_digit(10) {
                    if !(1..=8).contains(&d) {
                        return Err(syntax(format!("bad digit {c}")));
                    }
                    file += d as u8;
                } else {
                    let piece = Piece::from_fen_char(c).ok_or_else(|| syntax(format!("bad piece {c}")))?;
                    if file >= 8 {
                        return Err(syntax("rank too long"));
                    }
                    b.set(Square::new(file, rank), Some(piece));
                    file += 1;
                }
                if file > 8 {
                    return Err(syntax("rank too long"));
                }
            }
            if file != 8 {
                return Err(syntax("rank too short"));
            }
        }
        b.side_to_move = match fields[1] {
            "w" => Color::White,
            "b" => Color::Black,
            s => return Err(syntax(format!("bad side {s}"))),
        };
        let mut shredder = false;
        if fields[2] != "-" {
            for c in fields[2].chars() {
                let color = if c.is_ascii_uppercase() { Color::White } else { Color::Black };
                let back = color.back_rank();
                let king = (0..8)
                    .find(|&f| b.squares[Square::new(f, back).index()] == Some((Piece::King, color)))
                    .ok_or(FenError::Setup(SetupError::Castling))?;
                let rooks: Vec<u8> =
                    (0..8).filter(|&f| b.squares[Square::new(f, back).index()] == Some((Piece::Rook, color))).collect();
                let (side, file) = match c.to_ascii_uppercase() {
                    'K' => (CastleSide::Short, rooks.iter().copied().filter(|&f| f > king).max()),
                    'Q' => (CastleSide::Long, rooks.iter().copied().filter(|&f| f < king).min()),
                    l @ 'A'..='H' => {
                        shredder = true;
                        let f = l as u8 - b'A';
                        (if f > king { CastleSide::Short } else { CastleSide::Long }, Some(f))
                    }
                    _ => return Err(syntax(format!("bad castling {c}"))),
                };
                let file = file.ok_or(FenError::Setup(SetupError::Castling))?;
                b.castling[color.index()][side as usize] = Some(file);
            }
        }
        b.en_passant_file = match fields[3] {
            "-" => None,
            s => Some(s.parse::<Square>().map_err(FenError::Syntax)?.file()),
        };
        let number = |i: usize, default: u16| match fields.get(i) {
            None => Ok(default),
            Some(s) => s.parse::<u16>().map_err(|_| syntax(format!("bad number {s}"))),
        };
        b.halfmove_clock = number(4, 0)?;
        b.fullmove_number = number(5, 1)?;
        b.chess960 = shredder || needs_chess960(&b);
        b.build().map_err(FenError::Setup)
    }

    /// FEN, with castling written `KQkq`.
    pub fn fen(&self) -> String {
        self.to_string()
    }

    /// Shredder-FEN: castling written as the rook files.
    pub fn shredder_fen(&self) -> String {
        let mut s = String::with_capacity(90);
        write_fen(self, &mut s, true).expect("writing to a String cannot fail");
        s
    }
}

/// Castling rights that standard chess cannot express: a king off the e-file
/// or a rook off the corner.
fn needs_chess960(b: &BoardBuilder) -> bool {
    Color::ALL.iter().any(|&color| {
        let back = color.back_rank();
        let rights = b.castling[color.index()];
        let king_on_e = b.squares[Square::new(4, back).index()] == Some((Piece::King, color));
        (rights[0].is_some() || rights[1].is_some()) && !king_on_e
            || rights[CastleSide::Short as usize].is_some_and(|f| f != 7)
            || rights[CastleSide::Long as usize].is_some_and(|f| f != 0)
    })
}

fn write_fen(b: &Board, f: &mut impl fmt::Write, shredder: bool) -> fmt::Result {
    for rank in (0..8).rev() {
        let mut empty = 0;
        for file in 0..8 {
            match b.piece_at(Square::new(file, rank)) {
                Some((p, c)) => {
                    if empty > 0 {
                        write!(f, "{empty}")?;
                        empty = 0;
                    }
                    let l = p.letter();
                    f.write_char(if c == Color::White { l } else { l.to_ascii_lowercase() })?;
                }
                None => empty += 1,
            }
        }
        if empty > 0 {
            write!(f, "{empty}")?;
        }
        if rank > 0 {
            f.write_char('/')?;
        }
    }
    f.write_str(if b.side_to_move() == Color::White { " w " } else { " b " })?;
    let mut any = false;
    for color in Color::ALL {
        for side in CastleSide::ALL {
            if let Some(file) = b.castling_rook(color, side) {
                let c = if shredder {
                    (b'a' + file) as char
                } else if side == CastleSide::Short {
                    'k'
                } else {
                    'q'
                };
                f.write_char(if color == Color::White { c.to_ascii_uppercase() } else { c })?;
                any = true;
            }
        }
    }
    if !any {
        f.write_char('-')?;
    }
    // The file of any double step, as `cozy-chess` and most FEN writers do,
    // whether or not a capture is possible.
    match b.en_passant_file() {
        Some(file) => {
            let rank = if b.side_to_move() == Color::White { 5 } else { 2 };
            write!(f, " {}", Square::new(file, rank))?;
        }
        None => f.write_str(" -")?,
    }
    write!(f, " {} {}", b.halfmove_clock(), b.fullmove_number())
}

impl fmt::Display for Board {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_fen(self, f, f.alternate())
    }
}

impl FromStr for Board {
    type Err = FenError;
    fn from_str(s: &str) -> Result<Board, FenError> {
        Board::from_fen(s)
    }
}
