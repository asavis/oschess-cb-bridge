//! `.cba` records: the annotations of one classic game.
//!
//! A record is a 14-byte head (the game id, a fixed `01 00 0e 0e`, the number
//! of annotations plus one, and the record's size) and the annotations back to
//! back. Each annotation carries its position, its type and its own size, so
//! a type whose layout is unknown is skipped rather than ending the record.
//! Integers are big-endian.
//!
//! Positions count the moves in stored order (depth first, the main line
//! first at every position), not in the PGN order 2CBH uses; the annotations
//! say so by their [`Source`], which the PGN writer places them by.

use super::text::evidence;
use crate::bytes::{self, Fields};
use crate::codepage::{CodePage, Evidence};
use crate::game::{Annotation, Arrow, Block, GAME_POSITION, GameAnnotations, Source, Square, language};
use crate::movetable::{Sq, from_cb_square};
use crate::{Error, Result};

/// Size of a record's head.
pub(super) const HEAD: usize = 14;
/// Size of an annotation's own head: position, type and size.
const ITEM_HEAD: usize = 6;
/// The fixed bytes at 0x03 of every record.
const MARK: [u8; 4] = [1, 0, 0x0e, 0x0e];

/// The record's size from its head.
pub(super) fn record_size(head: &[u8; HEAD]) -> usize {
    head.be_u32::<0x0a>() as usize
}

/// Decodes the record of game `id`. Damage (a head that does not match the
/// game, a size or count that disagrees with the contents, an annotation that
/// runs past the record, a position below −1, a square out of range, a text
/// without its language) is an error. Whether each position names a move of
/// the game is checked against the game by [`GameAnnotations::check_positions`].
/// Texts are read as on a computer whose code page is Windows-1252
/// ([`parse_in`]).
pub fn parse(record: &[u8], id: u32) -> Result<GameAnnotations> {
    parse_in(record, id, CodePage::WESTERN, CodePage::WESTERN)
}

/// [`parse`] on a computer whose ANSI code page is `page`, which texts that
/// are not UTF-8 are read by (`super::text::single_byte`). A text whose own
/// words show no page is read in the one the game's other texts show
/// together, else in `fallback`.
pub fn parse_in(record: &[u8], id: u32, page: CodePage, fallback: CodePage) -> Result<GameAnnotations> {
    let bad = |at: usize, what: &str| Error::Format(format!("classic annotations of game {id} at byte {at}: {what}"));
    let Some(head) = record.first_chunk::<HEAD>() else { return Err(bad(0, "shorter than its head")) };
    if head.be_u24::<0>() != id {
        return Err(bad(0, "the head names another game"));
    }
    if head.field::<3, 4>() != &MARK {
        return Err(bad(3, "unexpected head bytes"));
    }
    if record_size(head) != record.len() {
        return Err(bad(0x0a, "size disagrees with the record"));
    }
    let mut out = GameAnnotations { source: Source::Classic, ..GameAnnotations::default() };
    let mut count = 0u32;
    // Texts that are not UTF-8, decoded once the whole record has been read:
    // where each lies in `out` and its bytes.
    let mut single_byte: Vec<((usize, usize), &[u8])> = Vec::new();
    let mut i = HEAD;
    while i < record.len() {
        let Some(item) = bytes::array::<ITEM_HEAD>(record, i) else {
            return Err(bad(i, "annotation head runs past the record"));
        };
        let position = int24(item.be_u24::<0>());
        let type_code = item[3];
        let size = item.be_u16::<4>() as usize;
        if size < ITEM_HEAD || size > record.len() - i {
            return Err(bad(i, "annotation size out of range"));
        }
        if position < GAME_POSITION {
            return Err(bad(i, &format!("position {position}")));
        }
        let data = &record[i + ITEM_HEAD..i + size];
        let (a, pending) = annotation(type_code, data).map_err(|what| bad(i + ITEM_HEAD, what))?;
        match out.blocks.last_mut() {
            Some(b) if b.position == position => b.annotations.push(a),
            _ => out.blocks.push(Block { position, annotations: vec![a] }),
        }
        if let Some(text) = pending {
            let block = out.blocks.len() - 1;
            single_byte.push(((block, out.blocks[block].annotations.len() - 1), text));
        }
        count += 1;
        i += size;
    }
    if head.be_u24::<7>() != count + 1 {
        return Err(bad(7, "annotation count disagrees with the record"));
    }
    let mut game = Evidence::default();
    for (_, text) in &single_byte {
        game.add(evidence(text));
    }
    let fallback = game.page().unwrap_or(fallback);
    for ((block, at), bytes) in single_byte {
        if let Annotation::Text { text, .. } = &mut out.blocks[block].annotations[at] {
            *text = super::text::single_byte(bytes, page, fallback);
        }
    }
    Ok(out)
}

/// A signed 24-bit integer.
fn int24(v: u32) -> i32 {
    ((v << 8) as i32) >> 8
}

/// An annotation of type `t` with data `d`, and for a text that is not
/// UTF-8 its bytes, which [`parse_in`] decodes once it has seen the record's
/// other texts: the text stays empty until then.
fn annotation(t: u8, d: &[u8]) -> std::result::Result<(Annotation, Option<&[u8]>), &'static str> {
    let a = match t {
        0x02 | 0x82 => {
            let [_, nation, text @ ..] = d else { return Err("text without its language") };
            let (decoded, pending) = match std::str::from_utf8(text) {
                Ok(s) => (s.to_owned(), None),
                Err(_) => (String::new(), Some(text)),
            };
            let a = Annotation::Text { before: t == 0x82, language: language_of(*nation), text: decoded };
            return Ok((a, pending));
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
        _ => Annotation::Other { code: u16::from(t), data: d.to_vec() },
    };
    Ok((a, None))
}

/// Squares here are numbered from 1, file by file.
fn square(n: u8) -> std::result::Result<Sq, &'static str> {
    match n {
        1..=64 => Ok(from_cb_square(n - 1).index() as Sq),
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
