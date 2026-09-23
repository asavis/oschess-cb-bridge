//! The framing of a record in `.2cbg` and `.2cba`: magic, sizes, checksum,
//! tag, content, spare area and trailing length.

use super::bytes::{be_u64, le_i32, le_i64, le_u16};
use crate::{Error, Result};

const RECORD_MAGIC: [u8; 8] = [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
/// Size of a move record's frame before its content.
pub(super) const FRAME_HEADER: usize = 0x1a;
/// Largest content or spare area accepted in one move record. The largest in
/// a Mega Database is about 1.2 MB, a guiding text.
const MAX_FRAME_PART: usize = 64 << 20;

/// The content and spare sizes of a frame, from its first bytes.
pub(super) fn frame_sizes(frame: &[u8], offset: i64) -> Result<(usize, usize)> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    if frame.len() < FRAME_HEADER {
        return Err(bad("offset out of range"));
    }
    if frame[..8] != RECORD_MAGIC {
        return Err(bad("bad magic"));
    }
    let a = usize::try_from(le_i32(frame, 8)).map_err(|_| bad("negative size"))?;
    let b = usize::try_from(le_i32(frame, 12)).map_err(|_| bad("negative size"))?;
    if a > MAX_FRAME_PART || b > MAX_FRAME_PART {
        return Err(bad("record larger than 64 MiB"));
    }
    Ok((a, b))
}

/// Checks one framed record from `.2cbg` or `.2cba` held from its first byte
/// in `frame`: magic, sizes, trailing length and checksum. Returns its tag and
/// content.
pub(super) fn parse_frame(frame: &[u8], offset: i64, verify_checksum: bool) -> Result<(u16, &[u8])> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    let (a, b) = frame_sizes(frame, offset)?;
    if FRAME_HEADER + a + b + 8 > frame.len() {
        return Err(bad("runs past end of file"));
    }
    let tail = le_i64(frame, FRAME_HEADER + a + b);
    if tail != (a + b + 34) as i64 {
        return Err(bad("trailing length mismatch"));
    }
    let content = &frame[FRAME_HEADER..FRAME_HEADER + a];
    if verify_checksum && be_u64(frame, 0x10) != checksum(content) {
        return Err(bad("checksum mismatch"));
    }
    Ok((le_u16(frame, 0x18), content))
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
