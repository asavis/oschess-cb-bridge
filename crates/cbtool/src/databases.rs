//! `cbtool databases`: the databases ChessBase's database window lists.

use std::path::{Path, PathBuf};

use cbformat::dbitems::{self, Format};
use cbformat::v2::Database;

/// Lists the databases of a ChessBase documents folder with their state on
/// this computer. The stored paths are not printed.
pub fn databases(dir: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let dir = Path::new(dir);
    let located = dbitems::locate(dir)?;
    if !located.conflict_copies.is_empty() {
        println!(
            "ignored: {} OneDrive conflict cop{} of {}",
            located.conflict_copies.len(),
            plural(located.conflict_copies.len()),
            dbitems::FILE_NAME
        );
    }
    let Some(list) = dbitems::read(dir)? else {
        println!("no {} in {}", dbitems::FILE_NAME, dir.display());
        return Ok(true);
    };
    println!("{:>3}  {:<6} {:<11} {:>10} {:>10}  name", "#", "format", "state", "listed", "records");
    for (i, e) in list.entries.iter().enumerate() {
        let path = local_path(dir, &e.path, cfg!(windows));
        let state = state_of(&path);
        let records = match (e.format, state) {
            (Format::Cbh2, State::Present) => match Database::open(&path) {
                Ok(db) => db.record_count().to_string(),
                Err(_) => "unreadable".into(),
            },
            _ => "-".into(),
        };
        println!(
            "{:>3}  {:<6} {:<11} {:>10} {:>10}  {}",
            i + 1,
            format_name(e.format),
            state.name(),
            e.games(),
            records,
            e.name
        );
    }
    Ok(true)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

fn format_name(f: Format) -> &'static str {
    match f {
        Format::Cbh2 => "2cbh",
        Format::Cbh => "cbh",
        Format::Pgn => "pgn",
        Format::Other => "other",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Present,
    Missing,
    /// A cloud-only OneDrive placeholder: not on disk, and reading it would download it.
    #[cfg_attr(not(windows), allow(dead_code))]
    CloudOnly,
    /// No blocks allocated for a non-empty file, as a cloud-only placeholder shows
    /// through WSL. A heuristic: no placeholder has been available to confirm it.
    #[cfg_attr(not(unix), allow(dead_code))]
    MaybeCloudOnly,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Present => "present",
            State::Missing => "missing",
            State::CloudOnly => "cloud-only",
            State::MaybeCloudOnly => "cloud-only?",
        }
    }
}

/// The state of a database's header file, read from its metadata only, so a
/// placeholder is never downloaded.
fn state_of(path: &Path) -> State {
    match std::fs::metadata(path) {
        Err(_) => State::Missing,
        Ok(m) => placeholder_state(&m),
    }
}

#[cfg(windows)]
fn placeholder_state(m: &std::fs::Metadata) -> State {
    use std::os::windows::fs::MetadataExt;
    if dbitems::is_cloud_only(m.file_attributes()) { State::CloudOnly } else { State::Present }
}

#[cfg(unix)]
fn placeholder_state(m: &std::fs::Metadata) -> State {
    use std::os::unix::fs::MetadataExt;
    unallocated(m.len(), m.blocks())
}

#[cfg(not(any(windows, unix)))]
fn placeholder_state(_: &std::fs::Metadata) -> State {
    State::Present
}

/// A non-empty file with no allocated blocks.
#[cfg_attr(not(unix), allow(dead_code))]
fn unallocated(len: u64, blocks: u64) -> State {
    if len > 0 && blocks == 0 { State::MaybeCloudOnly } else { State::Present }
}

/// A stored path as a path on this computer. ChessBase stores absolute Windows
/// paths. Elsewhere (WSL) a drive path `X:\…` maps to `/mnt/x/…`. A relative
/// path is taken relative to the documents folder.
fn local_path(dir: &Path, stored: &str, windows: bool) -> PathBuf {
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
            local_path(dir, r"C:\Users\u\Documents\A b.2cbh", false),
            PathBuf::from("/mnt/c/Users/u/Documents/A b.2cbh")
        );
        assert_eq!(local_path(dir, r"MyWork\A.cbh", false), PathBuf::from("/docs/MyWork/A.cbh"));
        assert_eq!(local_path(dir, "/tmp/x.2cbh", false), PathBuf::from("/tmp/x.2cbh"));
        assert_eq!(local_path(dir, r"C:\A.2cbh", true), PathBuf::from(r"C:\A.2cbh"));
    }

    #[test]
    fn unallocated_files_may_be_placeholders() {
        assert_eq!(unallocated(1677, 0), State::MaybeCloudOnly);
        assert_eq!(unallocated(1677, 8), State::Present);
        assert_eq!(unallocated(0, 0), State::Present);
    }
}
