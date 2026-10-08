//! `cbtool databases`: the databases ChessBase's database window lists.

use std::path::Path;

use cbformat::codepage::CodePage;
use cbformat::dbitems;
use cbformat::view::{Base, Format};

/// Lists the databases of a ChessBase documents folder with their state on
/// this computer, reading paths and titles that are not UTF-8 in `page`. The
/// stored paths are not printed.
pub fn databases(dir: &str, page: CodePage) -> Result<bool, Box<dyn std::error::Error>> {
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
    let Some(list) = dbitems::read(dir, page)? else {
        println!("no {} in {}", dbitems::FILE_NAME, dir.display());
        return Ok(true);
    };
    println!("{:>3}  {:<6} {:<11} {:>10} {:>10}  name", "#", "format", "state", "listed", "records");
    for (i, e) in list.window_order().into_iter().enumerate() {
        let path = dbitems::local_path(dir, &e.path);
        // Every file the database would be opened through is checked from its
        // metadata first, the main file first; a database is opened only when
        // its main file is here, and every file of it that is here is a
        // regular file kept on this computer. A file of another format is
        // checked alone.
        let files: Vec<State> = match e.format {
            Some(format) => format.files(&path).iter().map(|(file, _)| state_of(file)).collect(),
            None => vec![state_of(&path)],
        };
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

/// A database's state from the states of its files, the main file first, as
/// the bridge decides it (`bridge::catalog`, `generation_of` and
/// `Entry::open_files`): missing when the main file is, unreadable when any
/// file that is there is not a regular file, otherwise cloud-only when any
/// file that is there is. A missing companion, even a required one, leaves
/// the database present: opening it then fails, and the records column says
/// `unreadable`, as the bridge reports the database.
///
/// The one difference is [`State::MaybeCloudOnly`]: the bridge takes every
/// file as local where Windows attributes do not say otherwise, while here a
/// placeholder seen through WSL is guessed from its blocks.
fn combine(files: &[State]) -> State {
    let present = || files.iter().copied().filter(|&s| s != State::Missing);
    if files.first().is_none_or(|&s| s == State::Missing) {
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
        let db = |h, g, l, a| combine(&[h, g, l, a]);
        assert_eq!(db(P, P, P, P), P);
        assert_eq!(db(P, P, P, X), P); // no annotation file: fine
        // Without its main file there is no database, whatever else is here.
        assert_eq!(db(X, P, P, P), X);
        assert_eq!(db(X, C, P, P), X);
        assert_eq!(db(X, X, X, X), X);
        assert_eq!(combine(&[]), X);
        // A missing companion leaves the database to be opened, and fail.
        assert_eq!(db(P, X, P, P), P);
        // A resident header with an offline companion is not opened (Windows attributes).
        assert_eq!(db(P, P, C, P), C);
        assert_eq!(db(P, P, P, C), C);
        // The same through the zero-block heuristic elsewhere.
        assert_eq!(db(P, M, P, P), M);
        assert_eq!(db(M, P, C, P), C);
        assert_eq!(db(C, X, P, P), C);
        // Anything but a regular file makes the database unreadable, even offline.
        use State::NotAFile as N;
        assert_eq!(db(P, N, P, P), N);
        assert_eq!(db(P, P, C, N), N);
        assert_eq!(db(N, X, P, P), N);
        assert_eq!(db(X, N, P, P), X);
    }

    #[test]
    fn unallocated_files_may_be_placeholders() {
        assert_eq!(unallocated(1677, 0), State::MaybeCloudOnly);
        assert_eq!(unallocated(1677, 8), State::Present);
        assert_eq!(unallocated(0, 0), State::Present);
    }
}
