//! The annotations of a game, as both formats' readers decode them: blocks of
//! annotations by position, each a comment, symbols, squares, arrows or a type
//! the reading form of the PGN leaves out. [`crate::v2`] decodes `.2cba`
//! records into them, and [`crate::cbh`] classic `.cba` records.

use crate::movetable::Sq;
use crate::{Error, Result};

mod quote;
pub mod timing;

pub use quote::{Quotation, QuotedPlayer};

/// The position of annotations that belong to the game as a whole.
pub const GAME_POSITION: i32 = -1;

/// The annotations of one game, in stored order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameAnnotations {
    pub blocks: Vec<Block>,
    /// Set when a type of unknown layout ended the decoding: nothing after it
    /// could be located.
    pub stopped_at: Option<Unknown>,
    /// The record's bytes after the type code of [`Self::stopped_at`], which
    /// could not be decoded; empty when decoding reached the end.
    pub undecoded: Vec<u8>,
}

/// The annotations attached to one position. Their order is meaningful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// −1 for the game, else a move counted from 0 in PGN order.
    pub position: i32,
    pub annotations: Vec<Annotation>,
}

/// A type of unknown layout, and where it was met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unknown {
    pub position: i32,
    pub type_code: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Annotation {
    /// A comment, shown before the move (`before`) or after it.
    Text {
        before: bool,
        language: u16,
        text: String,
    },
    /// Up to three NAGs: on the move, on the position, and a prefix such as
    /// "better is". Zero means none.
    Symbols {
        on_move: u8,
        on_position: u8,
        prefix: u8,
    },
    Squares(Vec<Square>),
    Arrows(Vec<Arrow>),
    /// A type of known layout that the reading form of the PGN leaves out:
    /// its type code and its data, the bytes after the type as stored.
    Other {
        code: u16,
        data: Vec<u8>,
    },
}

/// A coloured square. Colours are ChessBase's numbers (2 green, 3 yellow,
/// 4 red); others occur and are kept as they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Square {
    pub colour: u8,
    pub square: Sq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrow {
    pub colour: u8,
    pub from: Sq,
    pub to: Sq,
}

/// ChessBase's language numbers in a text annotation.
pub mod language {
    pub const ENGLISH: u16 = 0;
    pub const GERMAN: u16 = 1;
    pub const FRENCH: u16 = 2;
    pub const SPANISH: u16 = 3;
    pub const ITALIAN: u16 = 4;
    pub const DUTCH: u16 = 5;
    pub const PORTUGUESE: u16 = 6;
    /// A text meant for every language.
    pub const ANY: u16 = 7;
    pub const POLISH: u16 = 12;
    pub const GREEK: u16 = 18;

    /// The number for an ISO 639-1 code, when ChessBase has one.
    pub fn from_iso(code: &str) -> Option<u16> {
        Some(match code.to_ascii_lowercase().as_str() {
            "en" => ENGLISH,
            "de" => GERMAN,
            "fr" => FRENCH,
            "es" => SPANISH,
            "it" => ITALIAN,
            "nl" => DUTCH,
            "pt" => PORTUGUESE,
            "pl" => POLISH,
            "el" => GREEK,
            _ => return None,
        })
    }
}

impl GameAnnotations {
    /// Checks every position, the one where decoding stopped included,
    /// against the game's `moves` moves (all lines counted, as
    /// [`crate::replay::TreeStats::total_plies`]), and says how many
    /// annotations lie past the last of them. The PGN writes those after the
    /// main line's last move (asavis/oschess-cb-bridge#38). A game without
    /// moves has no move to take them, so there any position but the game's
    /// (−1) is an error: the annotation would otherwise vanish without a trace.
    pub fn check_positions(&self, moves: u32) -> Result<usize> {
        let positions = self.blocks.iter().map(|b| b.position).chain(self.stopped_at.map(|u| u.position));
        if let Some(p) = positions.filter(|&p| p >= 0).max().filter(|_| moves == 0) {
            return Err(Error::Format(format!("annotations at position {p}, in a game without moves")));
        }
        Ok(self.blocks.iter().filter(|b| i64::from(b.position) >= i64::from(moves)).map(|b| b.annotations.len()).sum())
    }

    /// No annotation at all, and nothing left undecoded.
    pub fn is_empty(&self) -> bool {
        self.stopped_at.is_none() && self.blocks.iter().all(|b| b.annotations.is_empty())
    }
}

/// UTF-8 when the bytes are valid UTF-8, else Windows-1252: nothing in the
/// record says which.
pub(crate) fn decode_text(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => b.iter().map(|&c| cp1252(c)).collect(),
    }
}

fn cp1252(c: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match c {
        0x80..=0x9f => HIGH[(c - 0x80) as usize],
        _ => c as char,
    }
}
