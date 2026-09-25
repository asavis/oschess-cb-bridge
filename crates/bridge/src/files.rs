//! Small files replaced whole (#62): the pairing token, `bridge.toml` and the
//! app's preferences. Each is written to a temporary file beside it, flushed
//! to the disk, and renamed over the old one, so a reader, and the next start
//! after a crash or a power cut, finds the old file or the new one, never a
//! part of either.

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
    let temporary = temporary(path);
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// A name beside `path` that no other write in this process uses at the same
/// time: `<name>.<process>.<count>.tmp`.
fn temporary(path: &Path) -> PathBuf {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{}.{}.tmp", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
    path.with_file_name(name)
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
}
