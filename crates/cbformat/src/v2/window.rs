//! Move records read into a buffer the caller owns, so that a caller scanning
//! many games bounds and accounts for all of its memory: nothing here
//! allocates, and annotations are never read.

use std::borrow::Cow;

use super::frame::{FRAME_HEADER, frame_sizes, parse_frame};
use super::{Database, MoveData, Record};
use crate::{Error, Result};

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
    /// database. `buf` grows within its capacity only; the span is `None` when
    /// it does not fit there or the offsets name no span.
    pub fn read_move_window(
        &self,
        records: &[Record],
        next: Option<&Record>,
        buf: &mut Vec<u8>,
    ) -> Result<Option<MoveWindow>> {
        let offsets = records.iter().filter_map(|r| u64::try_from(r.moves_offset()).ok()).filter(|&o| o >= 12);
        let (Some(at), Some(last)) = (offsets.clone().min(), offsets.max()) else { return Ok(None) };
        let file_len = self.moves.len()?;
        let end = next.and_then(|n| u64::try_from(n.moves_offset()).ok()).filter(|&o| o >= 12).unwrap_or(file_len);
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
        let offset = record.moves_offset();
        let bytes = buf.get(..window.len)?;
        let rel = u64::try_from(offset).ok()?.checked_sub(window.at).and_then(|r| usize::try_from(r).ok())?;
        if rel.saturating_add(FRAME_HEADER) > bytes.len() {
            return None;
        }
        let (a, b) = frame_sizes(&bytes[rel..], offset).ok()?;
        if rel + FRAME_HEADER + a + b + 8 > bytes.len() {
            return None;
        }
        if a > limit || b > limit {
            let what = format!("move record of {} bytes, over the {limit}-byte limit", a.max(b));
            return Some(Err(Error::Format(format!("record at {offset:#x}: {what}"))));
        }
        Some(
            parse_frame(&bytes[rel..], offset, true)
                .map(|(tag, content)| MoveData { tag, content: Cow::Borrowed(content) }),
        )
    }

    /// Reads the move record of `record` into `buf`, within its capacity: a
    /// record whose content or spare area exceeds `limit` bytes, or whose frame
    /// does not fit `buf`, is refused before it is read.
    pub fn read_moves_into<'a>(&self, record: &Record, limit: usize, buf: &'a mut Vec<u8>) -> Result<MoveData<'a>> {
        let offset = record.moves_offset();
        let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
        let at = u64::try_from(offset).map_err(|_| bad("negative offset"))?;
        let file_len = self.moves.len()?;
        if at.checked_add(FRAME_HEADER as u64).is_none_or(|end| end > file_len) {
            return Err(bad("offset out of range"));
        }
        let mut head = [0u8; FRAME_HEADER];
        self.moves.read_into(at, &mut head)?;
        let (a, b) = frame_sizes(&head, offset)?;
        let whole = FRAME_HEADER + a + b + 8;
        if a > limit || b > limit || whole > buf.capacity() {
            return Err(bad(&format!("move record of {} bytes, over the {limit}-byte limit", a.max(b))));
        }
        if at + whole as u64 > file_len {
            return Err(bad("runs past end of file"));
        }
        buf.clear();
        buf.resize(whole, 0);
        self.moves.read_into(at, buf)?;
        let (tag, content) = parse_frame(buf, offset, true)?;
        Ok(MoveData { tag, content: Cow::Borrowed(content) })
    }
}
