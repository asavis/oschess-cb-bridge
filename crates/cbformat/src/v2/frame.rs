//! The framing of a record in `.2cbg` and `.2cba`: magic, sizes, checksum,
//! tag, content, spare area and trailing length; and the two ways a frame is
//! read, from bytes already read ([`frame_at`]) or from its file
//! ([`read_frame_into`]).

use crate::bytes::{self, Fields};
use crate::file::DbFile;
use crate::recordfile::over_limit;
use crate::{Error, Result};

const RECORD_MAGIC: [u8; 8] = [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
/// Size of a move record's frame before its content.
const FRAME_HEADER: usize = 0x1a;
/// Largest content or spare area accepted in one move record. The largest in
/// a Mega Database is about 1.2 MB, a guiding text.
pub(super) const MAX_FRAME_PART: usize = 64 << 20;

/// The content and spare sizes of a frame, from its header.
fn frame_sizes(head: &[u8; FRAME_HEADER], offset: i64) -> Result<(usize, usize)> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    if head.field::<0, 8>() != &RECORD_MAGIC {
        return Err(bad("bad magic"));
    }
    let a = usize::try_from(head.le_i32::<8>()).map_err(|_| bad("negative size"))?;
    let b = usize::try_from(head.le_i32::<12>()).map_err(|_| bad("negative size"))?;
    if a > MAX_FRAME_PART || b > MAX_FRAME_PART {
        return Err(bad("record larger than 64 MiB"));
    }
    Ok((a, b))
}

/// Checks one framed record from `.2cbg` or `.2cba` held from its first byte
/// in `frame`: magic, sizes, trailing length and checksum. Returns its tag and
/// content.
fn parse_frame(frame: &[u8], offset: i64, verify_checksum: bool) -> Result<(u16, &[u8])> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    let head = frame.first_chunk().ok_or_else(|| bad("offset out of range"))?;
    let (a, b) = frame_sizes(head, offset)?;
    let (Some(content), Some(tail)) =
        (frame.get(FRAME_HEADER..FRAME_HEADER + a), bytes::array(frame, FRAME_HEADER + a + b))
    else {
        return Err(bad("runs past end of file"));
    };
    if i64::from_le_bytes(*tail) != (a + b + 34) as i64 {
        return Err(bad("trailing length mismatch"));
    }
    if verify_checksum && head.be_u64::<0x10>() != checksum(content) {
        return Err(bad("checksum mismatch"));
    }
    Ok((head.le_u16::<0x18>(), content))
}

/// The tag and content of the frame at `offset` of a file, from `bytes`,
/// which hold the file from `at`; `None` when the frame does not lie wholly
/// inside them, and is then read on its own. A frame whose content or spare
/// area is over `limit` bytes is refused, as [`read_frame_into`] refuses it,
/// however `bytes` hold it; `kind` names the record in that error.
pub(super) fn frame_at<'a>(
    bytes: &'a [u8],
    at: u64,
    offset: i64,
    limit: usize,
    kind: &str,
) -> Option<Result<(u16, &'a [u8])>> {
    // A position that does not fit in `usize` (on a 32-bit target) lies
    // outside the bytes.
    let rel = u64::try_from(offset).ok()?.checked_sub(at).and_then(|rel| usize::try_from(rel).ok())?;
    let frame = bytes.get(rel..)?;
    let (a, b) = frame_sizes(frame.first_chunk()?, offset).ok()?;
    if FRAME_HEADER + a + b + 8 > frame.len() {
        return None;
    }
    if a > limit || b > limit {
        let what = format!("{kind} of {}", over_limit(a.max(b), limit));
        return Some(Err(Error::Format(format!("record at {offset:#x}: {what}"))));
    }
    Some(parse_frame(frame, offset, true))
}

/// Reads the frame at `offset` of `file` into `buf` and checks it, in two
/// reads: the frame header, then the whole frame. A frame whose content or
/// spare area is over `limit` bytes, or which is longer than `room` bytes, is
/// refused before it is read; `kind` names the record in that error.
pub(super) fn read_frame_into<'a>(
    file: &DbFile,
    offset: i64,
    limit: usize,
    room: usize,
    kind: &str,
    buf: &'a mut Vec<u8>,
) -> Result<(u16, &'a [u8])> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    let at = u64::try_from(offset).map_err(|_| bad("negative offset"))?;
    let file_len = file.size()?;
    if at.checked_add(FRAME_HEADER as u64).is_none_or(|end| end > file_len) {
        return Err(bad("offset out of range"));
    }
    let mut head = [0u8; FRAME_HEADER];
    file.read_into(at, &mut head)?;
    let (a, b) = frame_sizes(&head, offset)?;
    let whole = FRAME_HEADER + a + b + 8;
    if a > limit || b > limit {
        return Err(bad(&format!("{kind} of {}", over_limit(a.max(b), limit))));
    }
    if whole > room {
        return Err(bad(&format!("{kind} of {}", over_limit(whole, room))));
    }
    if at + whole as u64 > file_len {
        return Err(bad("runs past end of file"));
    }
    buf.clear();
    buf.resize(whole, 0);
    file.read_into(at, buf)?;
    parse_frame(buf, offset, true)
}

/// The record checksum: byte *i* of the value is the sum, modulo 256, of the
/// *i*-th of eight equal runs of the content; a trailing remainder is ignored.
pub fn checksum(content: &[u8]) -> u64 {
    let m = content.len() / 8;
    if m == 0 {
        let mut buf = [0u8; 8];
        buf[..content.len()].copy_from_slice(content);
        return u64::from_le_bytes(buf);
    }
    let mut v = 0u64;
    for i in 0..8 {
        let s = content[i * m..(i + 1) * m].iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
        v |= (s as u64) << (8 * i);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_runs() {
        assert_eq!(checksum(&[1, 2, 3]), 0x030201);
        let content: Vec<u8> = (0..17).collect();
        // m = 2: runs (0,1) (2,3) ... (14,15); byte 16 ignored
        let expect: u64 = (0..8).map(|i| ((2 * i + 2 * i + 1) as u64) << (8 * i)).sum();
        assert_eq!(checksum(&content), expect);
    }
}
