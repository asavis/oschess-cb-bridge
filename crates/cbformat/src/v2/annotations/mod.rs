//! `.2cba` records: the annotations of one game.
//!
//! A record is a run of position blocks, each a position, a count and that many
//! annotations, ended by the marker `7fffffff`. An annotation is a type and its
//! data, with **no length field**: every type met must be understood to find the
//! next one. A type whose layout is unknown therefore ends the decoding; what was
//! decoded before it is kept and the record is marked incomplete.

use crate::movetable::Sq;
use crate::{Error, Result};

mod layout;
mod quote;
#[cfg(test)]
mod tests;

use layout::annotation;
pub use quote::{Quotation, QuotedPlayer};

/// The tag of an annotation record's frame.
pub const ANNOTATION_TAG: u16 = 0x2000;
/// The end of a record's position blocks.
const END_MARKER: i32 = 0x7fff_ffff;
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
    /// Decodes a record's content. Damage (a length or count past the end, a
    /// missing end marker, bytes after it, a position below −1, a square out
    /// of range) is an error; a type of unknown layout ends decoding and sets
    /// [`Self::stopped_at`]. Whether each position names a move of the game
    /// is checked against the game by [`Self::check_positions`].
    pub fn parse(content: &[u8]) -> Result<Self> {
        let mut r = Reader { b: content, i: 0 };
        let mut out = GameAnnotations::default();
        loop {
            let position = r.i32()?;
            if position == END_MARKER {
                if r.i != content.len() {
                    return Err(r.bad("bytes after the end marker"));
                }
                return Ok(out);
            }
            if position < GAME_POSITION {
                return Err(r.bad(&format!("position {position}")));
            }
            let count = r.i32()?;
            // Each annotation is at least its 2-byte type.
            if count < 0 || count as usize > r.left() / 2 {
                return Err(r.bad("annotation count out of range"));
            }
            let mut annotations = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let type_code = r.u16()?;
                let start = r.i;
                match annotation(&mut r, type_code)? {
                    Some(mut a) => {
                        if let Annotation::Other { data, .. } = &mut a {
                            *data = content[start..r.i].to_vec();
                        }
                        annotations.push(a);
                    }
                    None => {
                        out.blocks.push(Block { position, annotations });
                        out.stopped_at = Some(Unknown { position, type_code });
                        out.undecoded = content[start..].to_vec();
                        return Ok(out);
                    }
                }
            }
            out.blocks.push(Block { position, annotations });
        }
    }

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

pub(super) struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn bad(&self, what: &str) -> Error {
        Error::Format(format!("annotations at byte {}: {what}", self.i))
    }
    pub(super) fn left(&self) -> usize {
        self.b.len() - self.i
    }
    pub(super) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.left() {
            return Err(self.bad("runs past the end of the record"));
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    pub(super) fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    pub(super) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(super) fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub(super) fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// A non-negative `int` length that fits in what is left.
    pub(super) fn len(&mut self) -> Result<usize> {
        let n = self.i32()?;
        usize::try_from(n).ok().filter(|&n| n <= self.left()).ok_or_else(|| self.bad("length out of range"))
    }
    pub(super) fn expect_one(&mut self) -> Result<()> {
        match self.u8()? {
            1 => Ok(()),
            _ => Err(self.bad("expected 01")),
        }
    }
}
