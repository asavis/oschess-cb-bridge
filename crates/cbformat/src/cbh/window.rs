//! Move records read into a buffer the caller owns, as the 2CBH reader's
//! window reads them: a caller scanning many games bounds and accounts for
//! all of its memory. Nothing here allocates, and annotations are never read.

use std::borrow::Cow;

use super::{Database, MIN_FILE_HEADER, MoveData, Record};
use crate::bytes::{self, Fields};
pub use crate::recordfile::MoveWindow;
use crate::recordfile::{over_limit, read_move_window};
use crate::{Error, Result};

impl Database {
    /// Reads into `buf` the `.cbg` bytes from the first to past the last move
    /// record of `records`, which are consecutive; `next` is the record after
    /// them, whose move record ends the span, or `None` at the end of the
    /// database. A `next` whose offset names no record ends the span where the
    /// last move record starts, and that one is read on its own. `buf` grows
    /// within its capacity only; the span is `None` when it does not fit
    /// there, when the offsets name no span, or when the database is too large
    /// for its headers' offsets (see `.cbj`).
    pub fn read_move_window(
        &self,
        records: &[Record],
        next: Option<&Record>,
        buf: &mut Vec<u8>,
    ) -> Result<Option<MoveWindow>> {
        if self.wide.is_some() {
            return Ok(None);
        }
        let offset = |r: &Record| u64::from(r.moves_offset());
        read_move_window(&self.moves, records.iter().map(offset), next.map(offset), MIN_FILE_HEADER, buf)
    }

    /// The move record of `record` from `window`, which `buf` holds, or `None`
    /// when the record does not lie wholly inside it. A record larger than
    /// `limit` bytes is refused, as [`Database::read_moves_into`] refuses it,
    /// however the window holds it.
    pub fn moves_in<'a>(
        &self,
        window: MoveWindow,
        buf: &'a [u8],
        record: &Record,
        limit: usize,
    ) -> Option<Result<MoveData<'a>>> {
        if self.wide.is_some() {
            return None;
        }
        let offset = u64::from(record.moves_offset());
        let held = buf.get(..window.len)?;
        let rel = offset.checked_sub(window.at).and_then(|r| usize::try_from(r).ok())?;
        let size = bytes::array::<4>(held, rel)?.be_u24::<1>() as usize;
        if size < 4 {
            let what = format!("move record at {offset:#x}: size {size} is smaller than the record's head");
            return Some(Err(Error::Format(what)));
        }
        let whole = held.get(rel..rel + size)?;
        if size > limit {
            return Some(Err(Error::Format(format!("move record at {offset:#x}: {}", over_limit(size, limit)))));
        }
        Some(Ok(MoveData { bytes: Cow::Borrowed(whole) }))
    }

    /// Reads the move record of `record` into `buf`, within its capacity: a
    /// record larger than `limit` bytes or than `buf` holds is refused before
    /// it is read.
    pub fn read_moves_into<'a>(&self, record: &Record, limit: usize, buf: &'a mut Vec<u8>) -> Result<MoveData<'a>> {
        let (at, size) = self.move_extent(record)?;
        if size > limit || size > buf.capacity() {
            let bound = limit.min(buf.capacity());
            return Err(Error::Format(format!("move record at {at:#x}: {}", over_limit(size, bound))));
        }
        buf.clear();
        buf.resize(size, 0);
        self.moves.read_into(at, buf)?;
        Ok(MoveData { bytes: Cow::Borrowed(buf) })
    }
}
