//! A game's move record: its start and its move tree as words.

use super::bytes::le_u16;
use crate::movetable::{self, Color, Piece, Sq};
use crate::{Error, Result};

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

/// The move record of one game: its variant tag, start and move-tree words.
#[derive(Clone, Copy)]
pub struct GameMoves<'a> {
    pub tag: u16,
    start: &'a [u8],
    tree: &'a [u8],
}

/// One token of the move tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// A move word, including the null move; decode with [`movetable::decode`].
    Move(u16),
    /// The move just given has a further alternative, stored later.
    Alternative,
    /// End of the current line.
    EndOfLine,
}

impl<'a> GameMoves<'a> {
    /// Splits a move record's content into its start section and move tree.
    pub fn parse(tag: u16, content: &'a [u8]) -> Result<Self> {
        if !content.len().is_multiple_of(2) {
            return Err(Error::Format("odd move stream length".into()));
        }
        let word = |i: usize| le_u16(content, 2 * i);
        let n = content.len() / 2;
        if n == 0 {
            return Err(Error::Format("empty move stream".into()));
        }
        let mut i = 0;
        let start_from = 0;
        if word(0) == movetable::START_POSITION {
            i = 1;
            while i < n && word(i) != movetable::MOVES {
                i += 1;
            }
        }
        if i >= n || word(i) != movetable::MOVES {
            return Err(Error::Format("no move section".into()));
        }
        Ok(GameMoves { tag, start: &content[start_from..2 * i], tree: &content[2 * (i + 1)..] })
    }

    /// The variant from the record tag: 1 normal chess, 2 Chess960.
    pub fn is_chess960(&self) -> bool {
        self.tag & 0xff == 2
    }

    pub fn start(&self) -> Result<Start> {
        if self.start.is_empty() {
            return Ok(Start::Standard);
        }
        let w: Vec<u16> = self.start[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        if self.is_chess960() && w.len() == 1 {
            return Ok(Start::Chess960(w[0]));
        }
        let (chess960, s) =
            if self.is_chess960() && w.first() == Some(&1000) { (true, &w[1..]) } else { (self.is_chess960(), &w[..]) };
        if s.len() < 3 {
            return Err(Error::Format("short set-up section".into()));
        }
        let mut pieces = Vec::with_capacity(s.len() - 3);
        for &p in &s[3..] {
            let (color, piece, sq) =
                movetable::decode_piece_word(p).ok_or_else(|| Error::Format(format!("bad piece word {p:#06x}")))?;
            pieces.push((sq, color, piece));
        }
        Ok(Start::Setup(Setup {
            chess960,
            move_number: s[0],
            side_to_move: if s[1] & 0xff == 0 { Color::White } else { Color::Black },
            castling: (s[1] >> 8) as u8,
            castling_rooks: [None; 4],
            castling_kings: [None; 2],
            en_passant_file: if (1..=8).contains(&s[2]) { Some(s[2] as u8 - 1) } else { None },
            en_passant_raw: s[2],
            pieces,
        }))
    }

    /// The move tree in stored order.
    pub fn tokens(&self) -> impl Iterator<Item = Token> + 'a {
        self.tree.as_chunks::<2>().0.iter().map(|c| match u16::from_le_bytes(*c) {
            movetable::ALTERNATIVE => Token::Alternative,
            movetable::END_OF_LINE => Token::EndOfLine,
            w => Token::Move(w),
        })
    }

    /// The main line: the move words up to the first end of line.
    pub fn main_line(&self) -> impl Iterator<Item = u16> + 'a {
        self.tokens()
            .take_while(|t| *t != Token::EndOfLine)
            .filter_map(|t| if let Token::Move(w) = t { Some(w) } else { None })
    }
}
