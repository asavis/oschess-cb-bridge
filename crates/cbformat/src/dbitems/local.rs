//! A path stored in the list as a path on this computer.

use std::path::{Path, PathBuf};

/// `stored`, a path from the list of the ChessBase documents folder `dir`, as
/// a path on this computer. ChessBase stores absolute Windows paths. On other
/// systems (WSL) a drive path `X:\…` maps to `/mnt/x/…`. A relative path is
/// taken relative to the documents folder.
pub fn local_path(dir: &Path, stored: &str) -> PathBuf {
    local_path_on(dir, stored, cfg!(windows))
}

fn local_path_on(dir: &Path, stored: &str, windows: bool) -> PathBuf {
    let b = stored.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    if windows {
        let p = PathBuf::from(stored);
        return if p.is_absolute() || drive { p } else { dir.join(p) };
    }
    if drive {
        let rest = stored[3..].replace('\\', "/");
        return PathBuf::from(format!("/mnt/{}/{rest}", (b[0] as char).to_ascii_lowercase()));
    }
    if stored.starts_with('/') {
        return PathBuf::from(stored);
    }
    dir.join(stored.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_paths_on_this_computer() {
        let dir = Path::new("/docs");
        assert_eq!(
            local_path_on(dir, r"C:\Users\u\Documents\A b.2cbh", false),
            PathBuf::from("/mnt/c/Users/u/Documents/A b.2cbh")
        );
        assert_eq!(local_path_on(dir, r"MyWork\A.cbh", false), PathBuf::from("/docs/MyWork/A.cbh"));
        assert_eq!(local_path_on(dir, "/tmp/x.2cbh", false), PathBuf::from("/tmp/x.2cbh"));
        assert_eq!(local_path_on(dir, r"C:\A.2cbh", true), PathBuf::from(r"C:\A.2cbh"));
    }
}
