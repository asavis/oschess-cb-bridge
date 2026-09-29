//! Move records read into a buffer the caller owns, so that a caller scanning
//! many games bounds and accounts for all of its memory: nothing here
//! allocates, and annotations are never read.

use std::borrow::Cow;

use super::frame::{frame_at, read_frame_into};
use super::{Database, FILE_HEADER, MoveData, Record, position};
use crate::Result;
use crate::recordfile::span;

/// The `.2cbg` bytes a buffer holds: from `at`, `len` of them.
#[derive(Clone, Copy, Debug)]
pub struct MoveWindow {
    at: u64,
    len: usize,
}

impl Database {
    /// Reads into `buf` the `.2cbg` bytes from the first to past the last
    /// move record of `records`, which are consecutive; `next` is the record
    /// after them, whose move record ends the span, or `None` at the end of the
    /// database. A `next` whose offset names no record ends the span where the
    /// last move record starts, and that one is read on its own. `buf` grows
    /// within its capacity only; the span is `None` when it does not fit there
    /// or the offsets name no span.
    pub fn read_move_window(
        &self,
        records: &[Record],
        next: Option<&Record>,
        buf: &mut Vec<u8>,
    ) -> Result<Option<MoveWindow>> {
        let offset = |r: &Record| position(r.moves_offset());
        let Some(range) = span(records.iter().map(offset), next.map(offset), self.moves.len()?, FILE_HEADER) else {
            return Ok(None);
        };
        let Some(len) = usize::try_from(range.end - range.start).ok().filter(|&l| l <= buf.capacity()) else {
            return Ok(None);
        };
        buf.clear();
        buf.resize(len, 0);
        self.moves.read_into(range.start, buf)?;
        Ok(Some(MoveWindow { at: range.start, len }))
    }

    /// The move record of `record` from `window`, which `buf` holds, or `None`
    /// when the record does not lie wholly inside it. A record whose content
    /// or spare area exceeds `limit` bytes is refused, as
    /// [`Database::read_moves_into`] refuses it, however the window holds it.
    pub fn moves_in<'a>(
        &self,
        window: MoveWindow,
        buf: &'a [u8],
        record: &Record,
        limit: usize,
    ) -> Option<Result<MoveData<'a>>> {
        let found = frame_at(buf.get(..window.len)?, window.at, record.moves_offset(), limit, "move record")?;
        Some(found.map(|(tag, content)| MoveData { tag, content: Cow::Borrowed(content) }))
    }

    /// Reads the move record of `record` into `buf`, within its capacity: a
    /// record whose content or spare area exceeds `limit` bytes, or whose frame
    /// does not fit `buf`, is refused before it is read.
    pub fn read_moves_into<'a>(&self, record: &Record, limit: usize, buf: &'a mut Vec<u8>) -> Result<MoveData<'a>> {
        let room = buf.capacity();
        let (tag, content) = read_frame_into(&self.moves, record.moves_offset(), limit, room, "move record", buf)?;
        Ok(MoveData { tag, content: Cow::Borrowed(content) })
    }
}
