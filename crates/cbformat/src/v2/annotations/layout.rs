//! The layout of each annotation type: how many bytes it takes, and what is
//! kept of it.

use super::{Annotation, Arrow, Reader, Square, decode_text};
use crate::Result;
use crate::movetable::{Sq, from_cb_square};

/// One annotation of type `t`, or `None` when its layout is unknown.
pub(super) fn annotation(r: &mut Reader<'_>, t: u16) -> Result<Option<Annotation>> {
    Ok(Some(match t {
        0x02 | 0x82 => {
            r.skip(2)?; // always 0
            let language = r.u16()?;
            let n = r.len()?;
            Annotation::Text { before: t == 0x82, language, text: decode_text(r.take(n)?) }
        }
        0x03 => {
            let b = r.take(3)?;
            Annotation::Symbols { on_move: b[0], on_position: b[1], prefix: b[2] }
        }
        0x04 => {
            let n = r.len()?;
            let items = r.take(n)?;
            if items.len() % 2 != 0 {
                return Err(r.bad("coloured squares of odd length"));
            }
            let mut v = Vec::with_capacity(items.len() / 2);
            for p in items.as_chunks::<2>().0 {
                v.push(Square { colour: p[0], square: square(r, p[1])? });
            }
            Annotation::Squares(v)
        }
        0x05 => {
            let n = r.len()?;
            let items = r.take(n)?;
            if items.len() % 3 != 0 {
                return Err(r.bad("arrows not in triples"));
            }
            let mut v = Vec::with_capacity(items.len() / 3);
            for p in items.as_chunks::<3>().0 {
                v.push(Arrow { colour: p[0], from: square(r, p[1])?, to: square(r, p[2])? });
            }
            Annotation::Arrows(v)
        }
        // Piece path: a length and that many bytes.
        0x15 => {
            let n = r.len()?;
            r.skip(n)?;
            Annotation::Other(t)
        }
        // Time spent, unknown 08, clocks, medals, variation colour, video
        // stream time; 27 is ours (docs/format-notes.md).
        0x07 | 0x08 | 0x16 | 0x17 | 0x22 | 0x23 | 0x25 => {
            r.skip(4)?;
            Annotation::Other(t)
        }
        0x27 => {
            r.skip(2)?;
            Annotation::Other(t)
        }
        // Pawn structure, critical position.
        0x14 | 0x18 => {
            r.skip(1)?;
            Annotation::Other(t)
        }
        0x21 => {
            r.skip(6)?;
            Annotation::Other(t)
        }
        0x24 => {
            r.skip(38)?;
            Annotation::Other(t)
        }
        0x26 => {
            r.expect_one()?;
            let n = r.len()?;
            r.skip(n)?;
            Annotation::Other(t)
        }
        0x09 => {
            if !training(r)? {
                return Ok(None);
            }
            Annotation::Other(t)
        }
        0x13 => {
            if !quotation(r)? {
                return Ok(None);
            }
            Annotation::Other(t)
        }
        // Video: 01 00, a language, a length and that many bytes.
        0x20 => {
            r.skip(4)?;
            let n = r.len()?;
            r.skip(n)?;
            Annotation::Other(t)
        }
        0x1c => {
            r.expect_one()?;
            for _ in 0..2 {
                let n = r.len()?;
                r.skip(n)?;
            }
            Annotation::Other(t)
        }
        _ => return Ok(None),
    }))
}

/// A training question: a header whose third byte is the variant, time and
/// points, four lists (the question and three responses), then the solutions.
/// Variant 1 solutions are two squares, two unknown bytes and a list; variant 2
/// solutions are points and two lists, answer and reply (docs/format-notes.md).
/// `false` for another variant.
fn training(r: &mut Reader<'_>) -> Result<bool> {
    let variant = r.take(6)?[2];
    if !matches!(variant, 1 | 2) {
        return Ok(false);
    }
    r.skip(6)?; // time allowed, points
    for _ in 0..4 {
        training_list(r)?;
    }
    let solutions = r.u8()?;
    for _ in 0..solutions {
        if variant == 1 {
            r.skip(4)?; // squares, two unknown bytes
            training_list(r)?;
        } else {
            r.skip(1)?; // points
            training_list(r)?;
            training_list(r)?;
        }
    }
    Ok(true)
}

fn training_list(r: &mut Reader<'_>) -> Result<()> {
    let items = r.u16()?;
    for _ in 0..items {
        r.skip(2)?;
        let n = r.len()?;
        r.skip(n)?;
    }
    Ok(())
}

/// A game quotation: header strings, fixed blocks, rating lists, the start
/// position when it is not the standard one, and moves. `false` for a start
/// marker of unknown meaning.
fn quotation(r: &mut Reader<'_>) -> Result<bool> {
    r.expect_one()?;
    r.skip(2 + 2 + 4 + 1)?; // mode, unknown, int 1, zero
    for _ in 0..6 {
        let n = r.u8()? as usize; // counts the terminating zero
        r.skip(n)?;
    }
    r.skip(35 + 44)?;
    for _ in 0..2 {
        r.skip(5)?; // 01 00 01 00 00
        let n = r.len()?;
        r.skip(n)?;
    }
    r.skip(26)?;
    match r.u8()? {
        1 => {}
        // A set-up start: 64 squares file by file, then 11 bytes of side to
        // move, move number and the like (docs/format-notes.md).
        0 => r.skip(64 + 11)?,
        _ => return Ok(false),
    }
    r.skip(2)?;
    let moves = r.len()?;
    r.skip(moves.checked_mul(5).ok_or_else(|| r.bad("quotation move count"))?)?;
    r.skip(4)?;
    Ok(true)
}

/// A square numbered from 1, file by file (`a1` 1, `a2` 2, `b1` 9).
fn square(r: &Reader<'_>, v: u8) -> Result<Sq> {
    match v {
        1..=64 => Ok(from_cb_square(v - 1)),
        _ => Err(r.bad("square out of range")),
    }
}
