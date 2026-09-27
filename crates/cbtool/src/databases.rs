//! `cbtool databases`: the databases ChessBase's database window lists.

use std::path::Path;

use cbformat::dbitems;
use cbformat::view::{Base, Format};

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
    for (i, e) in list.window_order().into_iter().enumerate() {
        let path = dbitems::local_path(dir, &e.path);
        // Every file the database would be opened through is checked from its
        // metadata first; a database is opened only when all of them are here.
        // A file of another format is checked alone.
        let files = e.format.map_or_else(|| vec![(path.clone(), true)], |format| format.files(&path));
        let files: Vec<(State, bool)> = files.iter().map(|(file, required)| (state_of(file), *required)).collect();
        let state = combine(&files);
        let records = match (e.format, state) {
            // A PGN file is opened through an index built from it, never here.
            (Some(Format::TwoCbh | Format::Cbh), State::Present) => match Base::open(&path) {
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

fn format_name(f: Option<Format>) -> &'static str {
    match f {
        Some(Format::TwoCbh) => "2cbh",
        Some(Format::Cbh) => "cbh",
        Some(Format::Pgn) => "pgn",
        None => "other",
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
    /// Something other than a regular file (a directory, a pipe, a device):
    /// opening it could block or read something that is not a database.
    NotAFile,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Present => "present",
            State::Missing => "missing",
            State::CloudOnly => "cloud-only",
            State::MaybeCloudOnly => "cloud-only?",
            State::NotAFile => "unreadable",
        }
    }
}

/// A database's state from the states of its files: missing when a required
/// file is missing, unreadable when any file that is there is not a regular
/// file, otherwise cloud-only when any file that is there is.
fn combine(files: &[(State, bool)]) -> State {
    let present = || files.iter().filter(|(s, _)| *s != State::Missing).map(|(s, _)| *s);
    if files.iter().any(|&(s, required)| required && s == State::Missing) {
        State::Missing
    } else if present().any(|s| s == State::NotAFile) {
        State::NotAFile
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
        Ok(m) if !m.is_file() => State::NotAFile,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Anything but a regular file makes the database unreadable, even offline.
        use State::NotAFile as N;
        assert_eq!(db(P, N, P, P), N);
        assert_eq!(db(P, P, C, N), N);
        assert_eq!(db(N, X, P, P), X);
    }

    #[test]
    fn unallocated_files_may_be_placeholders() {
        assert_eq!(unallocated(1677, 0), State::MaybeCloudOnly);
        assert_eq!(unallocated(1677, 8), State::Present);
        assert_eq!(unallocated(0, 0), State::Present);
    }
}
