//! Move records read into a buffer the caller owns, as the 2CBH reader's
//! window reads them: a caller scanning many games bounds and accounts for
//! all of its memory. Nothing here allocates, and annotations are never read.

use std::borrow::Cow;

use super::bytes::be_u24;
use super::{Database, MIN_FILE_HEADER, MoveData, Record};
use crate::{Error, Result};

/// The `.cbg` bytes a buffer holds: from `at`, `len` of them.
#[derive(Clone, Copy, Debug)]
pub struct MoveWindow {
    at: u64,
    len: usize,
}

impl Database {
    /// Reads into `buf` the `.cbg` bytes from the first to past the last move
    /// record of `records`, which are consecutive; `next` is the record after
    /// them, whose move record ends the span, or `None` at the end of the
    /// database. `buf` grows within its capacity only; the span is `None` when
    /// it does not fit there, when the offsets name no span, or when the
    /// database is too large for its headers' offsets (see `.cbj`).
    pub fn read_move_window(
        &self,
        records: &[Record],
        next: Option<&Record>,
        buf: &mut Vec<u8>,
    ) -> Result<Option<MoveWindow>> {
        if self.wide.is_some() {
            return Ok(None);
        }
        let offsets = records.iter().map(|r| u64::from(r.moves_offset())).filter(|&o| o >= MIN_FILE_HEADER);
        let (Some(at), Some(last)) = (offsets.clone().min(), offsets.max()) else { return Ok(None) };
        let file_len = self.moves.len()?;
        let end = next.map(|n| u64::from(n.moves_offset())).filter(|&o| o >= MIN_FILE_HEADER).unwrap_or(file_len);
        let end = end.max(last).min(file_len);
        let at = at.min(file_len);
        let Some(len) =
            end.checked_sub(at).and_then(|l| usize::try_from(l).ok()).filter(|&l| l > 0 && l <= buf.capacity())
        else {
            return Ok(None);
        };
        buf.clear();
        buf.resize(len, 0);
        self.moves.read_into(at, buf)?;
        Ok(Some(MoveWindow { at, len }))
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
        let bytes = buf.get(..window.len)?;
        let rel = offset.checked_sub(window.at).and_then(|r| usize::try_from(r).ok())?;
        if rel.saturating_add(4) > bytes.len() {
            return None;
        }
        let size = be_u24(bytes, rel + 1) as usize;
        if size < 4 {
            let what = format!("move record at {offset:#x}: size {size} is smaller than the record's head");
            return Some(Err(Error::Format(what)));
        }
        let whole = bytes.get(rel..rel + size)?;
        if size > limit {
            let what = format!("move record at {offset:#x}: {size} bytes, over the {limit}-byte limit");
            return Some(Err(Error::Format(what)));
        }
        Some(Ok(MoveData { bytes: Cow::Borrowed(whole) }))
    }

    /// Reads the move record of `record` into `buf`, within its capacity: a
    /// record larger than `limit` bytes or than `buf` holds is refused before
    /// it is read.
    pub fn read_moves_into<'a>(&self, record: &Record, limit: usize, buf: &'a mut Vec<u8>) -> Result<MoveData<'a>> {
        let (at, size) = self.move_extent(record)?;
        if size > limit || size > buf.capacity() {
            return Err(Error::Format(format!(
                "move record at {at:#x}: {size} bytes, over the {}-byte limit",
                limit.min(buf.capacity())
            )));
        }
        buf.clear();
        buf.resize(size, 0);
        self.moves.read_into(at, buf)?;
        Ok(MoveData { bytes: Cow::Borrowed(buf) })
    }
}
