//! Database files read at positions, never mapped.

use std::fs::File;
use std::path::PathBuf;

use crate::{Error, Result};

/// One file of a database, read at positions.
pub(super) struct DbFile {
    file: File,
    path: PathBuf,
}

impl DbFile {
    pub(super) fn open(path: PathBuf) -> Result<DbFile> {
        let file = File::open(&path).map_err(|e| Error::Io(path.clone(), e))?;
        Ok(DbFile { file, path })
    }

    pub(super) fn len(&self) -> Result<u64> {
        self.file.metadata().map(|m| m.len()).map_err(|e| Error::Io(self.path.clone(), e))
    }

    /// Fills `buf` from `offset`; a short file is an error.
    pub(super) fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        read_exact_at(&self.file, buf, offset).map_err(|e| Error::Io(self.path.clone(), e))
    }

    /// `len` bytes from `offset`. Callers bound `len` first.
    pub(super) fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];
        self.read_into(offset, &mut buf)?;
        Ok(buf)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
