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
        // Every file the database would be opened through is checked from its
        // metadata first; a database is opened only when all of them are here.
        let files: Vec<(State, bool)> =
            database_files(&path, e.format).iter().map(|(file, required)| (state_of(file), *required)).collect();
        let state = combine(&files);
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
    CloudOnly,
    /// No blocks allocated for a non-empty file, as a cloud-only placeholder shows
    /// through WSL. A heuristic: no placeholder has been available to confirm it.
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

/// The files a database is read through, each with whether it is required:
/// for 2CBH the ones `Database::open` opens, plus the annotations when present.
/// Other formats are not opened; their main file alone is checked.
fn database_files(path: &Path, format: Format) -> Vec<(PathBuf, bool)> {
    match format {
        Format::Cbh2 => {
            let stem = path.with_extension("");
            [(".2cbh", true), (".2cbg", true), (".2lid", true), (".2cba", false)]
                .iter()
                .map(|&(ext, required)| {
                    let mut p = stem.clone().into_os_string();
                    p.push(ext);
                    (PathBuf::from(p), required)
                })
                .collect()
        }
        _ => vec![(path.to_owned(), true)],
    }
}

/// A database's state from the states of its files: missing when a required
/// file is missing, otherwise cloud-only when any file that is there is.
fn combine(files: &[(State, bool)]) -> State {
    let present = || files.iter().filter(|(s, _)| *s != State::Missing).map(|(s, _)| *s);
    if files.iter().any(|&(s, required)| required && s == State::Missing) {
        State::Missing
    } else if present().any(|s| s == State::CloudOnly) {
        State::CloudOnly
    } else if present().any(|s| s == State::MaybeCloudOnly) {
        State::MaybeCloudOnly
    } else {
        State::Present
    }
}

/// The state of one file, read from its metadata only, so a placeholder is
/// never downloaded.
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
    fn a_database_is_as_available_as_its_least_available_file() {
        use State::{CloudOnly as C, MaybeCloudOnly as M, Missing as X, Present as P};
        let db = |h, g, l, a| combine(&[(h, true), (g, true), (l, true), (a, false)]);
        assert_eq!(db(P, P, P, P), P);
        assert_eq!(db(P, P, P, X), P); // no annotation file: fine
        assert_eq!(db(P, X, P, P), X);
        // A resident header with an offline companion is not opened (Windows attributes).
        assert_eq!(db(P, P, C, P), C);
        assert_eq!(db(P, P, P, C), C);
        // The same through the zero-block heuristic elsewhere.
        assert_eq!(db(P, M, P, P), M);
        assert_eq!(db(M, P, C, P), C);
        assert_eq!(db(C, X, P, P), X);
    }

    #[test]
    fn companion_files_of_a_2cbh_database() {
        let files = database_files(Path::new("/d/Big Base.2cbh"), Format::Cbh2);
        let names: Vec<(String, bool)> =
            files.iter().map(|(p, r)| (p.file_name().unwrap().to_string_lossy().into_owned(), *r)).collect();
        assert_eq!(
            names,
            [
                ("Big Base.2cbh".to_string(), true),
                ("Big Base.2cbg".to_string(), true),
                ("Big Base.2lid".to_string(), true),
                ("Big Base.2cba".to_string(), false)
            ]
        );
        assert_eq!(database_files(Path::new("/d/a.pgn"), Format::Pgn), [(PathBuf::from("/d/a.pgn"), true)]);
    }

    #[test]
    fn unallocated_files_may_be_placeholders() {
        assert_eq!(unallocated(1677, 0), State::MaybeCloudOnly);
        assert_eq!(unallocated(1677, 8), State::Present);
        assert_eq!(unallocated(0, 0), State::Present);
    }
}
