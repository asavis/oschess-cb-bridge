//! `.cba` records: the annotations of one classic game.
//!
//! A record is a 14-byte head (the game id, a fixed `01 00 0e 0e`, the number
//! of annotations plus one, and the record's size) and the annotations back to
//! back. Each annotation carries its position, its type and its own size, so
//! a type whose layout is unknown is skipped rather than ending the record.
//! Integers are big-endian.
//!
//! Positions count the moves in stored order (depth first, the main line
//! first at every position), not in the PGN order 2CBH uses; the PGN writer
//! places them by that order.

use super::bytes::{be_u16, be_u24, be_u32};
use crate::movetable::{Sq, from_cb_square};
use crate::v2::{Annotation, Arrow, Block, GAME_POSITION, GameAnnotations, Square, language};
use crate::{Error, Result};

/// Size of a record's head.
pub(super) const HEAD: usize = 14;
/// Size of an annotation's own head: position, type and size.
const ITEM_HEAD: usize = 6;
/// The fixed bytes at 0x03 of every record.
const MARK: [u8; 4] = [1, 0, 0x0e, 0x0e];

/// The record's size from its head, which must be `HEAD` bytes.
pub(super) fn record_size(head: &[u8]) -> usize {
    be_u32(head, 0x0a) as usize
}

/// Decodes the record of game `id`. Damage (a head that does not match the
/// game, a size or count that disagrees with the contents, an annotation that
/// runs past the record, a position below −1, a square out of range, a text
/// without its language) is an error. Whether each position names a move of
/// the game is checked against the game by [`GameAnnotations::check_positions`].
pub fn parse(record: &[u8], id: u32) -> Result<GameAnnotations> {
    let bad = |at: usize, what: &str| Error::Format(format!("classic annotations of game {id} at byte {at}: {what}"));
    if record.len() < HEAD {
        return Err(bad(0, "shorter than its head"));
    }
    if be_u24(record, 0) != id {
        return Err(bad(0, "the head names another game"));
    }
    if record[3..7] != MARK {
        return Err(bad(3, "unexpected head bytes"));
    }
    if record_size(record) != record.len() {
        return Err(bad(0x0a, "size disagrees with the record"));
    }
    let mut out = GameAnnotations::default();
    let mut count = 0u32;
    let mut i = HEAD;
    while i < record.len() {
        if record.len() - i < ITEM_HEAD {
            return Err(bad(i, "annotation head runs past the record"));
        }
        let position = int24(be_u24(record, i));
        let type_code = record[i + 3];
        let size = be_u16(record, i + 4) as usize;
        if size < ITEM_HEAD || size > record.len() - i {
            return Err(bad(i, "annotation size out of range"));
        }
        if position < GAME_POSITION {
            return Err(bad(i, &format!("position {position}")));
        }
        let data = &record[i + ITEM_HEAD..i + size];
        let a = annotation(type_code, data).map_err(|what| bad(i + ITEM_HEAD, what))?;
        match out.blocks.last_mut() {
            Some(b) if b.position == position => b.annotations.push(a),
            _ => out.blocks.push(Block { position, annotations: vec![a] }),
        }
        count += 1;
        i += size;
    }
    if be_u24(record, 7) != count + 1 {
        return Err(bad(7, "annotation count disagrees with the record"));
    }
    Ok(out)
}

/// A signed 24-bit integer.
fn int24(v: u32) -> i32 {
    ((v << 8) as i32) >> 8
}

fn annotation(t: u8, d: &[u8]) -> std::result::Result<Annotation, &'static str> {
    Ok(match t {
        0x02 | 0x82 => {
            let [_, nation, text @ ..] = d else { return Err("text without its language") };
            Annotation::Text { before: t == 0x82, language: language_of(*nation), text: decode_text(text) }
        }
        0x03 => {
            if d.is_empty() || d.len() > 3 {
                return Err("symbols of unexpected length");
            }
            let at = |k: usize| d.get(k).copied().unwrap_or(0);
            Annotation::Symbols { on_move: at(0), on_position: at(1), prefix: at(2) }
        }
        0x04 => {
            if !d.len().is_multiple_of(2) {
                return Err("coloured squares of odd length");
            }
            let mut v = Vec::with_capacity(d.len() / 2);
            for p in d.as_chunks::<2>().0 {
                v.push(Square { colour: p[0], square: square(p[1])? });
            }
            Annotation::Squares(v)
        }
        0x05 => {
            if !d.len().is_multiple_of(3) {
                return Err("arrows not in triples");
            }
            let mut v = Vec::with_capacity(d.len() / 3);
            for p in d.as_chunks::<3>().0 {
                v.push(Arrow { colour: p[0], from: square(p[1])?, to: square(p[2])? });
            }
            Annotation::Arrows(v)
        }
        // Every other type has its size, so it is skipped whatever its layout.
        _ => Annotation::Other(u16::from(t)),
    })
}

/// Squares here are numbered from 1, file by file.
fn square(n: u8) -> std::result::Result<Sq, &'static str> {
    match n {
        1..=64 => Ok(from_cb_square(n - 1)),
        _ => Err("square out of range"),
    }
}

/// The 2CBH language number for a text's nation code: the seven languages
/// ChessBase writes, Polish and Greek by their nations, and 0 as any
/// language. Other nations keep a number of their own above those, so that
/// they never pass for a preferred language.
pub fn language_of(nation: u8) -> u16 {
    match nation {
        0 => language::ANY,
        42 => language::ENGLISH,
        53 => language::GERMAN,
        49 => language::FRENCH,
        43 => language::SPANISH,
        70 => language::ITALIAN,
        103 => language::DUTCH,
        117 => language::PORTUGUESE,
        116 => language::POLISH,
        55 => language::GREEK,
        n => 0x100 + u16::from(n),
    }
}

/// UTF-8 when the bytes are valid UTF-8, else Windows-1252, as in 2CBH.
fn decode_text(b: &[u8]) -> String {
    crate::v2::annotations_text(b)
}
