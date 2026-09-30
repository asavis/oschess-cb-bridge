//! The 64-bit offsets of `.cbj`, for a `.cbg` or `.cba` over 4 GiB.
//!
//! A `.cbh` record holds 32-bit offsets. The extended record of each game in
//! `.cbj` holds the same two offsets as 64-bit integers: the annotations at
//! 0x0c (from version 3) and the moves at 0x1e (from version 6). Records are
//! big-endian after a little-endian 32-byte header of version, record size
//! and record count.

use std::path::PathBuf;

use crate::bytes::Fields;
use crate::file::DbFile;
use crate::{Error, Result};

const HEADER: usize = 32;
/// The shortest record that holds both offsets.
const MIN_RECORD: usize = 0x1e + 8;

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
        if file.size()? < HEADER as u64 {
            return Err(bad("shorter than its header"));
        }
        let mut h = [0u8; HEADER];
        file.read_into(0, &mut h)?;
        let (record, count) = (h.le_i32::<4>(), h.le_i32::<8>());
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
        let at = HEADER as u64 + self.record * u64::from(id - 1);
        let mut r = [0u8; MIN_RECORD];
        self.file.read_into(at, &mut r)?;
        let (moves, annotations) = (r.be_i64::<0x1e>(), r.be_i64::<0x0c>().max(0));
        let agrees = |wide: i64, short: u32| wide >= 0 && wide as u64 & 0xffff_ffff == u64::from(short);
        if !agrees(moves, short.0) || !agrees(annotations, short.1) {
            return Err(Error::Format(format!("game {id}: the offsets of .cbj and .cbh disagree")));
        }
        Ok((moves as u64, annotations as u64))
    }
}
