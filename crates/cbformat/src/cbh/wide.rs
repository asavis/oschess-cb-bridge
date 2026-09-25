//! The 64-bit offsets of `.cbj`, for a `.cbg` or `.cba` over 4 GiB.
//!
//! A `.cbh` record holds 32-bit offsets. The extended record of each game in
//! `.cbj` holds the same two offsets as 64-bit integers: the annotations at
//! 0x0c (from version 3) and the moves at 0x1e (from version 6). Records are
//! big-endian after a little-endian 32-byte header of version, record size
//! and record count.

use std::path::PathBuf;

use crate::file::DbFile;
use crate::{Error, Result};

const HEADER: u64 = 32;
/// The shortest record that holds both offsets.
const MIN_RECORD: u64 = 0x1e + 8;

pub(super) struct Wide {
    file: DbFile,
    record: u64,
    count: u32,
}

impl Wide {
    pub(super) fn open(path: PathBuf) -> Result<Self> {
        let bad = |what: &str| Error::Format(format!(".cbj: {what}; a .cbg or .cba over 4 GiB needs its offsets"));
        let file = match DbFile::open(path) {
            Ok(f) => f,
            Err(Error::Io(_, e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(bad("missing")),
            Err(e) => return Err(e),
        };
        if file.len()? < HEADER {
            return Err(bad("shorter than its header"));
        }
        let h = file.read(0, HEADER as usize)?;
        let int = |o: usize| i32::from_le_bytes(h[o..o + 4].try_into().unwrap_or_default());
        let (record, count) = (int(4), int(8));
        if i64::from(record) < MIN_RECORD as i64 || record > 4096 {
            return Err(bad(&format!("record size {record} holds no 64-bit offsets")));
        }
        Ok(Wide { file, record: record as u64, count: count.max(0) as u32 })
    }

    /// The `.cbg` and `.cba` offsets of game `id`, checked against the low 32
    /// bits `.cbh` holds, `short` of them. A game past the records the file
    /// holds keeps the `.cbh` offsets, as ChessBase gives it default values.
    pub(super) fn offsets(&self, id: u32, short: (u32, u32)) -> Result<(u64, u64)> {
        if id == 0 || id > self.count {
            return Ok((u64::from(short.0), u64::from(short.1)));
        }
        let at = HEADER + self.record * u64::from(id - 1);
        let r = self.file.read(at, MIN_RECORD as usize)?;
        let long = |o: usize| i64::from_be_bytes(r[o..o + 8].try_into().unwrap_or_default());
        let (moves, annotations) = (long(0x1e), long(0x0c).max(0));
        let agrees = |wide: i64, short: u32| wide >= 0 && wide as u64 & 0xffff_ffff == u64::from(short);
        if !agrees(moves, short.0) || !agrees(annotations, short.1) {
            return Err(Error::Format(format!("game {id}: the offsets of .cbj and .cbh disagree")));
        }
        Ok((moves as u64, annotations as u64))
    }
}
