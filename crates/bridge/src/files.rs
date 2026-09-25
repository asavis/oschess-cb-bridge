//! Small files replaced whole (#62): the pairing token, `bridge.toml` and the
//! app's preferences. Each is written to a temporary file beside it, flushed
//! to the disk, and renamed over the old one, so a reader, and the next start
//! after a crash or a power cut, finds the old file or the new one, never a
//! part of either.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replaces the file at `path` with `bytes`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write(path, bytes, false)
}

/// [`write_atomic`] for a file only its owner may read. `%APPDATA%` is
/// private to the user on Windows; elsewhere the file is mode 0600 from its
/// creation on.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write(path, bytes, true)
}

fn write(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let (temporary, mut file) = create_temporary(path, private)?;
    let written = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    // Only the file this call created is removed.
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Temporary names tried before a write gives up.
const TEMPORARY_ATTEMPTS: u32 = 64;

/// Counts the temporary names this process has used.
static COUNT: AtomicU64 = AtomicU64::new(0);

/// Creates a temporary file beside `path`, `<name>.<process>.<count>.tmp`. A
/// name already taken, as by a file left when a process of the same id was
/// stopped before its rename, is passed over, never removed (#62).
fn create_temporary(path: &Path, private: bool) -> io::Result<(PathBuf, File)> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut attempts = 0;
    loop {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(format!(".{}.{}.tmp", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let candidate = path.with_file_name(name);
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts + 1 < TEMPORARY_ATTEMPTS => attempts += 1,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_replaced_whole_and_no_temporary_file_stays() {
        let dir = std::env::temp_dir().join(format!("bridge-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second, longer").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second, longer");
        write_private_atomic(&path, b"third").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"third");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["settings.json"]);
        // A write that cannot finish leaves the old file and no temporary one.
        let folder = dir.join("a folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(write_atomic(&folder, b"x").is_err());
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["a folder", "settings.json"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Temporary files left by a process with this id that was stopped before
    /// its rename are passed over and kept: the write takes the next name.
    #[test]
    fn a_left_temporary_file_is_passed_over_and_kept() {
        let dir = std::env::temp_dir().join(format!("bridge-files-left-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        let next = COUNT.load(Ordering::Relaxed);
        let left: Vec<PathBuf> =
            (next..next + 3).map(|n| dir.join(format!("token.{}.{n}.tmp", std::process::id()))).collect();
        for file in &left {
            std::fs::write(file, b"left").unwrap();
        }
        write_private_atomic(&path, b"new token").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new token");
        for file in &left {
            assert_eq!(std::fs::read(file).unwrap(), b"left", "{} is kept", file.display());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
