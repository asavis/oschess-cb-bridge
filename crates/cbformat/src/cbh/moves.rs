//! A game's `.cbg` record: its encoding mode, start position and move stream.

use chesscore::{Board, Color as CColor, Piece as CPiece, Square};

use super::bytes::{be_u16, be_u24};
use crate::game::{Setup, Start};
use crate::movetable::{Color, Piece, Sq, from_cb_square};
use crate::{Error, Result};

/// Size of the explicit start position that bit 6 of the flags announces.
const START_SIZE: usize = 28;
/// Size of the Chess960 squares and start number after the start position.
const CHESS960_SIZE: usize = 8;

/// The move record of one game, split into its parts.
#[derive(Clone, Copy)]
pub struct GameMoves<'a> {
    flags: u8,
    start: Option<&'a [u8]>,
    chess960: Option<&'a [u8]>,
    stream: &'a [u8],
}

/// A board as the start position stores it: the pieces by ChessBase square
/// (`a1` = 0, `a2` = 1, … file by file).
pub(super) type CbBoard = [Option<(CColor, CPiece)>; 64];

impl<'a> GameMoves<'a> {
    /// Splits a whole `.cbg` record, its 4-byte head included.
    pub fn parse(record: &'a [u8]) -> Result<Self> {
        let bad = |what: String| Error::Format(format!("move record: {what}"));
        if record.len() < 4 {
            return Err(bad(format!("{} bytes", record.len())));
        }
        let size = be_u24(record, 1) as usize;
        if size != record.len() {
            return Err(bad(format!("size field {size} for a {}-byte record", record.len())));
        }
        let flags = record[0];
        let mut at = 4;
        let mut take = |n: usize, what: &str| {
            let part = record.get(at..at + n).ok_or_else(|| bad(format!("{what} runs past the record")))?;
            at += n;
            Ok::<_, Error>(part)
        };
        let start = if flags & 0x40 != 0 { Some(take(START_SIZE, "start position")?) } else { None };
        let mode = flags & 0x3f;
        let chess960 = if mode == 10 || mode == 11 {
            if start.is_none() {
                return Err(bad("Chess960 game without a start position".into()));
            }
            Some(take(CHESS960_SIZE, "Chess960 squares")?)
        } else {
            None
        };
        Ok(GameMoves { flags, start, chess960, stream: &record[at..] })
    }

    /// The encoding mode, the low 6 bits of the flags.
    pub fn mode(&self) -> u8 {
        self.flags & 0x3f
    }

    pub fn is_chess960(&self) -> bool {
        self.chess960.is_some()
    }

    pub(super) fn stream(&self) -> &'a [u8] {
        self.stream
    }

    /// Where the game starts. A Chess960 game whose explicit position is one
    /// of the 960 start positions, untouched, is reported as that position, as
    /// 2CBH stores it; any other explicit position is a set-up, whose castling
    /// rights use the rooks the Chess960 squares name.
    pub fn start(&self) -> Result<Start> {
        let Some(s) = self.start else { return Ok(Start::Standard) };
        let board = decode_board(&s[4..])?;
        let side_to_move = if s[1] & 0x10 != 0 { Color::Black } else { Color::White };
        let ep = s[1] & 0x0f;
        let castling = s[2] & 0x0f;
        let move_number = u16::from(s[3]).max(1);
        if let Some(extra) = self.chess960 {
            let n = be_u16(extra, 6);
            let untouched = side_to_move == Color::White && castling == 0x0f && ep == 0 && move_number == 1;
            if untouched && Board::chess960(n).is_some_and(|b| same_placement(&b, &board)) {
                return Ok(Start::Chess960(n));
            }
        }
        let mut pieces: Vec<(Sq, Color, Piece)> = Vec::new();
        for (cb, p) in board.iter().enumerate() {
            if let Some((c, p)) = p {
                pieces.push((from_cb_square(cb as u8), color(*c), piece(*p)));
            }
        }
        Ok(Start::Setup(Setup {
            chess960: self.chess960.is_some(),
            move_number,
            side_to_move,
            castling,
            castling_rooks: self.chess960.map_or([None; 4], |e| named_squares(e).1),
            castling_kings: self.chess960.map_or([None; 2], |e| named_squares(e).0),
            en_passant_file: (1..=8).contains(&ep).then(|| ep - 1),
            en_passant_raw: u16::from(ep),
            pieces,
        }))
    }
}

/// The files of the king and rook squares the Chess960 bytes name: the
/// kings (white, black), and the castling rooks in the order of the castling
/// bits (white O-O-O, white O-O, black O-O-O, black O-O). They are the start
/// squares, which a side's king and rook must still stand on to castle. One
/// that is not a square on its side's back rank names nothing: a right then
/// needs no particular king square and uses the outermost rook.
fn named_squares(extra: &[u8]) -> ([Option<u8>; 2], [Option<u8>; 4]) {
    // Bytes 0-1: the kings; 2-5: white king's side, white queen's side, black
    // king's side, black queen's side.
    let file = |i: usize, back_rank: u8| (extra[i] < 64 && extra[i] % 8 == back_rank).then_some(extra[i] / 8);
    ([file(0, 0), file(1, 7)], [file(3, 0), file(2, 0), file(5, 7), file(4, 7)])
}

/// Decodes the 192-bit board stream: per square, a 0 bit for an empty square,
/// or a 1 bit, a colour bit and 3 bits of piece.
pub(super) fn decode_board(bits: &[u8]) -> Result<CbBoard> {
    let bad = |what: &str| Error::Format(format!("start position: {what}"));
    let mut board: CbBoard = [None; 64];
    let mut pos = 0usize;
    let mut bit = || {
        let byte = *bits.get(pos / 8).ok_or_else(|| bad("board runs past its 24 bytes"))?;
        let b = byte >> (7 - pos % 8) & 1;
        pos += 1;
        Ok::<_, Error>(b)
    };
    for square in &mut board {
        if bit()? == 0 {
            continue;
        }
        let c = if bit()? == 0 { CColor::White } else { CColor::Black };
        let code = (bit()? << 2) | (bit()? << 1) | bit()?;
        let p = match code {
            1 => CPiece::King,
            2 => CPiece::Queen,
            3 => CPiece::Knight,
            4 => CPiece::Bishop,
            5 => CPiece::Rook,
            6 => CPiece::Pawn,
            _ => return Err(bad(&format!("piece code {code}"))),
        };
        *square = Some((c, p));
    }
    Ok(board)
}

/// The ChessBase square (file-major) as a board square.
pub(super) fn cb_square(cb: u8) -> Square {
    Square::new(cb / 8, cb % 8)
}

fn same_placement(b: &Board, cb: &CbBoard) -> bool {
    (0..64u8).all(|s| b.piece_at(cb_square(s)).map(|(p, c)| (c, p)) == cb[s as usize])
}

fn color(c: CColor) -> Color {
    match c {
        CColor::White => Color::White,
        CColor::Black => Color::Black,
    }
}

fn piece(p: CPiece) -> Piece {
    match p {
        CPiece::King => Piece::King,
        CPiece::Queen => Piece::Queen,
        CPiece::Knight => Piece::Knight,
        CPiece::Bishop => Piece::Bishop,
        CPiece::Rook => Piece::Rook,
        CPiece::Pawn => Piece::Pawn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_stream() {
        // The example of the format description: a white pawn on a2, a black
        // rook on b1 and a white knight on b4.
        let mut bits = [0u8; 24];
        bits[..3].copy_from_slice(&[0x58, 0x0e, 0x93]);
        let b = decode_board(&bits).unwrap();
        assert_eq!(b[1], Some((CColor::White, CPiece::Pawn)));
        assert_eq!(b[8], Some((CColor::Black, CPiece::Rook)));
        assert_eq!(b[11], Some((CColor::White, CPiece::Knight)));
        assert_eq!(b.iter().flatten().count(), 3);
        assert_eq!(cb_square(11).to_string(), "b4");
    }

    #[test]
    fn a_full_board_that_overruns_is_refused() {
        // 64 occupied squares need 320 bits; the stream has 192.
        assert!(decode_board(&[0xff; 24]).is_err());
        assert!(decode_board(&[0x84; 24]).is_err()); // piece code 0
    }

    #[test]
    fn record_parts() {
        let rec = [0x00, 0x00, 0x00, 0x06, 0xaa, 0xbb];
        let g = GameMoves::parse(&rec).unwrap();
        assert_eq!((g.mode(), g.stream()), (0, &rec[4..]));
        assert_eq!(g.start().unwrap(), Start::Standard);
        assert!(GameMoves::parse(&[0x00, 0x00, 0x00, 0x07, 0xaa, 0xbb]).is_err());
        assert!(GameMoves::parse(&[0x40, 0x00, 0x00, 0x06, 0xaa, 0xbb]).is_err());
        assert!(GameMoves::parse(&[0x0a, 0x00, 0x00, 0x04]).is_err());
    }
}
