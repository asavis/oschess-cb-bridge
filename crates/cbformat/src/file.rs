//! Database files read at positions, never mapped.

use std::fs::File;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Mutex;

use crate::{Error, Result};

/// Most handles a file keeps for its readers; a reader beyond them opens one
/// for its read and closes it after.
#[cfg(windows)]
const SPARE_HANDLES: usize = 64;

/// One file of a database, read at positions.
pub(crate) struct DbFile {
    file: File,
    /// Boxed, so that a database of many files stays small with the handles
    /// below.
    path: Box<Path>,
    /// Windows reads through one handle one read at a time, so readers at the
    /// same time each take a handle of their own, opened again from `file`,
    /// and leave it here for the next read.
    #[cfg(windows)]
    spare: Box<Mutex<Vec<File>>>,
}

impl DbFile {
    pub(crate) fn open(path: PathBuf) -> Result<DbFile> {
        let file = File::open(&path).map_err(|e| Error::Io(path.clone(), e))?;
        Ok(DbFile {
            file,
            path: path.into_boxed_path(),
            #[cfg(windows)]
            spare: Box::default(),
        })
    }

    pub(crate) fn len(&self) -> Result<u64> {
        self.file.metadata().map(|m| m.len()).map_err(|e| Error::Io(self.path.to_path_buf(), e))
    }

    /// Fills `buf` from `offset`; a short file is an error.
    pub(crate) fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.with_handle(|file| read_exact_at(file, buf, offset)).map_err(|e| Error::Io(self.path.to_path_buf(), e))
    }

    /// Calls `read` with a handle of the file that no other reader uses now:
    /// a spare one, or one opened again from the file's own; the file's own
    /// when it cannot be opened again.
    #[cfg(windows)]
    fn with_handle<T>(&self, read: impl FnOnce(&File) -> std::io::Result<T>) -> std::io::Result<T> {
        let spare = self.spare.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let Some(handle) = spare.or_else(|| reopen(&self.file).ok()) else { return read(&self.file) };
        let result = read(&handle);
        let mut spare = self.spare.lock().unwrap_or_else(|e| e.into_inner());
        if spare.len() < SPARE_HANDLES {
            spare.push(handle);
        }
        result
    }

    /// Calls `read` with the file's handle, which readers share: reads at
    /// positions on it run at the same time.
    #[cfg(not(windows))]
    fn with_handle<T>(&self, read: impl FnOnce(&File) -> std::io::Result<T>) -> std::io::Result<T> {
        read(&self.file)
    }

    /// `len` bytes from `offset`. Callers bound `len` first.
    pub(crate) fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];
        self.read_into(offset, &mut buf)?;
        Ok(buf)
    }
}

/// The paths `stem` takes with each of `extensions` appended.
pub(crate) fn with_extensions<'a>(stem: &Path, extensions: impl IntoIterator<Item = &'a &'a str>) -> Vec<PathBuf> {
    extensions
        .into_iter()
        .map(|ext| {
            let mut s = stem.as_os_str().to_owned();
            s.push(ext);
            PathBuf::from(s)
        })
        .collect()
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// A new handle, for reading, of the file `file` has open: the same file even
/// when its path has since come to name another.
#[cfg(windows)]
fn reopen(file: &File) -> std::io::Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReOpenFile(original: RawHandle, access: u32, share: u32, flags: u32) -> RawHandle;
    }
    const GENERIC_READ: u32 = 0x8000_0000;
    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, as `File::open`
    // shares a file.
    const SHARE_ALL: u32 = 0x7;
    // SAFETY: `file` holds its handle open for the whole call, and no flags
    // are asked for, so the new handle reads synchronously as `file`'s does.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), GENERIC_READ, SHARE_ALL, 0) };
    if handle as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the handle was just opened, and the file made from it is its
    // only owner.
    Ok(unsafe { File::from_raw_handle(handle) })
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("cbformat-file-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Readers at the same time read what the file holds, each through a
    /// handle of its own, and leave the handles for the next reads.
    #[test]
    fn readers_at_the_same_time_read_through_handles_of_their_own() {
        let bytes: Vec<u8> = (0..1u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        let path = temp("readers", &bytes);
        let f = DbFile::open(path.clone()).unwrap();
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let (f, bytes) = (&f, &bytes);
                s.spawn(move || {
                    for i in 0..200u64 {
                        let at = (t * 131_071 + i * 4_099) % (bytes.len() as u64 - 4096);
                        let mut buf = [0u8; 4096];
                        f.read_into(at, &mut buf).unwrap();
                        assert_eq!(&buf[..], &bytes[at as usize..at as usize + 4096]);
                    }
                });
            }
        });
        let spare = f.spare.lock().unwrap().len();
        assert!((1..=8).contains(&spare), "{spare} spare handles");
        drop(f);
        std::fs::remove_file(&path).unwrap();
    }

    /// A file renamed while it is open, with another written at its path, is
    /// still the one read.
    #[test]
    fn a_handle_opened_again_reads_the_open_file_not_its_path() {
        let path = temp("renamed", b"first");
        let moved = path.with_extension("moved");
        let f = DbFile::open(path.clone()).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"other").unwrap();
        let mut buf = [0u8; 5];
        f.read_into(0, &mut buf).unwrap();
        assert_eq!(&buf, b"first");
        assert_eq!(f.spare.lock().unwrap().len(), 1, "the read took a handle of its own");
        drop(f);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&moved).unwrap();
    }
}
