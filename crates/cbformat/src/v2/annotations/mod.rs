//! `.2cba` records: the annotations of one game.
//!
//! A record is a run of position blocks, each a position, a count and that many
//! annotations, ended by the marker `7fffffff`. An annotation is a type and its
//! data, with **no length field**: every type met must be understood to find the
//! next one. A type whose layout is unknown therefore ends the decoding; what was
//! decoded before it is kept and the record is marked incomplete.

use crate::game::annotations_text as decode_text;
use crate::game::{Annotation, Arrow, Block, GAME_POSITION, GameAnnotations, Square, Unknown};
use crate::{Error, Result};

mod layout;
#[cfg(test)]
mod tests;

use layout::annotation;

/// The tag of an annotation record's frame.
pub const ANNOTATION_TAG: u16 = 0x2000;
/// The end of a record's position blocks.
const END_MARKER: i32 = 0x7fff_ffff;

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
