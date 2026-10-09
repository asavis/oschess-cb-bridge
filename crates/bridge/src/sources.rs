//! Where the list of databases comes from: ChessBase's database window
//! (`DBItems.cbini`), then `bridge.toml`, then the command line.

use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fmt::Display;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cbformat::dbitems;

use crate::catalog::{Format, Hash};
use crate::config;

/// The places the list is read from.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    /// ChessBase's documents folder, holding `DBItems.cbini`.
    pub chessbase: Option<PathBuf>,
    /// `bridge.toml`, as the bridge follows it, for the engine too (#175);
    /// its `databases` are read again when it changes.
    pub config: Option<Arc<config::Watched>>,
    /// Databases named on the command line.
    pub fixed: Vec<PathBuf>,
}

/// A database of the list: its path on this computer and the name shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub path: PathBuf,
    pub name: String,
}

impl Listed {
    /// A database listed by path alone, named after its file.
    pub fn at(path: PathBuf) -> Listed {
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Listed { path, name }
    }
}

impl Sources {
    /// The databases ChessBase's window lists, in the order the window shows
    /// them where that is decoded and in the file's order otherwise
    /// (`DbList::window_order`), named as the window names them. Paths and
    /// titles that are not UTF-8 are read in the computer's ANSI code page, as
    /// ChessBase reads them. `Ok(empty)` when there is no such list. The
    /// error, for the log, names no path.
    pub fn window(&self) -> Result<Vec<Listed>, String> {
        let Some(dir) = &self.chessbase else { return Ok(Vec::new()) };
        match std::fs::metadata(dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(Vec::new()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("ChessBase's documents folder: {e}")),
        }
        let located = dbitems::locate(dir).map_err(|e| crate::log::error(&e))?;
        if !located.conflict_copies.is_empty() {
            crate::log!(
                "ignoring {} sync-conflict cop{} of {}",
                located.conflict_copies.len(),
                if located.conflict_copies.len() == 1 { "y" } else { "ies" },
                dbitems::FILE_NAME
            );
        }
        let page = crate::pgnindex::system_code_page();
        let Some(list) = dbitems::read(dir, page).map_err(|e| crate::log::error(&e))? else { return Ok(Vec::new()) };
        Ok(list
            .into_window_order()
            .into_iter()
            .map(|e| Listed { path: dbitems::local_path(dir, &e.path), name: e.name })
            .collect())
    }

    /// The `databases` of `bridge.toml`, as written; empty when there is no
    /// file. Read through `config::read`, the one reader of the file (#70).
    pub fn configured(&self) -> Result<Vec<PathBuf>, String> {
        let Some(file) = &self.config else { return Ok(Vec::new()) };
        config::read(file.path()).map(|c| c.map(|c| c.databases).unwrap_or_default())
    }
}

/// The most folders [`expand`] reads for one configured path, the configured
/// folder included (#320). A folder as large as a whole drive is searched no
/// further, which bounds the time one walk takes.
pub const MAX_FOLDERS: usize = 10_000;

/// The most folders a walk may read and still be repeated on every request
/// that looks for changes (#320). A larger tree is walked again only after
/// [`LOOK_EVERY`]: the tray app looks once a second besides the requests, and
/// a tree of 300 folders on OneDrive took 60 ms to walk on Windows, so 64
/// folders take about 13 ms a look.
pub const QUICK_FOLDERS: usize = 64;

/// The least time after a walk of more than [`QUICK_FOLDERS`] folders begins
/// before the next one, which is also never sooner than [`LOOK_SHARE`] times
/// the walk's own time: walks take at most a twentieth of one thread.
pub const LOOK_EVERY: Duration = Duration::from_secs(10);
const LOOK_SHARE: u32 = 20;

/// The databases a configured path names: the path itself, or for a folder
/// the ChessBase databases and PGN files in it and in every folder below it
/// (#320), by path ([`walk`]). In a folder only regular files (or links to
/// them) count: a pipe or a folder named like a database is none. The error,
/// for the log, names no path.
pub fn expand(path: &Path) -> Result<Vec<Listed>, String> {
    walk(path, MAX_FOLDERS).map(Walk::listed)
}

/// What [`walk`] found for a configured path.
#[derive(Debug, Default)]
struct Walk {
    /// The databases, sorted by path.
    found: Vec<PathBuf>,
    /// Folders below the configured one that were gone when read, or that
    /// may not be read; their databases are not listed.
    skipped: usize,
    /// The walk left folders unread to stay within `max`.
    cut: bool,
    /// The folders read, the configured one included.
    folders: usize,
}

impl Walk {
    fn listed(self) -> Vec<Listed> {
        self.found.into_iter().map(Listed::at).collect()
    }

    /// Until when this walk, begun at `started`, stands without another:
    /// `None` when it read at most [`QUICK_FOLDERS`] folders, so the next
    /// request walks again; else [`LOOK_EVERY`] after it began, or
    /// [`LOOK_SHARE`] times its own time when that is longer.
    fn until(&self, started: Instant) -> Option<Instant> {
        (self.folders > QUICK_FOLDERS).then(|| started + LOOK_EVERY.max(started.elapsed() * LOOK_SHARE))
    }

    /// What the walk left out: the folders it skipped, and whether it stopped
    /// at its cap.
    fn left_out(&self) -> (usize, bool) {
        (self.skipped, self.cut)
    }

    /// Logs what the walk left out, for the source `what`.
    fn log(&self, what: &dyn Display) {
        if self.skipped > 0 {
            let s = if self.skipped == 1 { "" } else { "s" };
            crate::log!("{what}: skipped {} folder{s} that could not be read", self.skipped);
        }
        if self.cut {
            crate::log!("{what}: more than {MAX_FOLDERS} folders, the rest is not searched");
        }
    }
}

/// [`expand`] reading at most `max` folders: breadth first, each folder's
/// subfolders in path order, so that folders near the top are searched
/// first and the same tree always gives the same databases. The folders
/// read and waiting stay within `max` together, so a tree of any width or
/// depth holds at most `max` paths of folders. A link or junction to a
/// folder is not followed, so the walk never loops and never leaves the
/// folder the user chose; a hidden folder is not searched ([`hidden`]). The
/// configured folder that cannot be read fails the walk, as does a folder
/// below it for any reason but being gone or closed to this user: a failure
/// keeps what was read before (`Kept`), so an error that passes loses no
/// databases, while a folder that stays closed, such as another user's,
/// never stops the others from being listed.
fn walk(path: &Path, max: usize) -> Result<Walk, String> {
    let err = |e: std::io::Error| e.to_string();
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {}
        // A database, or a path that names nothing and is then reported missing.
        Ok(_) => return Ok(Walk { found: vec![path.to_owned()], ..Walk::default() }),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok(Walk { found: vec![path.to_owned()], ..Walk::default() });
        }
        Err(e) => return Err(err(e)),
    }
    let mut walk = Walk::default();
    let mut folders = VecDeque::from([path.to_owned()]);
    while let Some(folder) = folders.pop_front() {
        walk.folders += 1;
        let keep = max.saturating_sub(walk.folders + folders.len());
        match read_folder(&folder, keep) {
            Ok((databases, subfolders, left_out)) => {
                walk.found.extend(databases);
                folders.extend(subfolders);
                walk.cut |= left_out;
            }
            Err(e) if walk.folders > 1 && matches!(e.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {
                walk.skipped += 1;
            }
            Err(e) => return Err(err(e)),
        }
    }
    walk.found.sort();
    Ok(walk)
}

/// The databases directly in `folder`, the first `keep` of the folders in it
/// to search, in path order, and whether it left others out. Only reading
/// the folder itself fails: an entry named like a database that cannot be
/// examined, such as a link into a folder closed to this user, is listed,
/// and the catalog shows it missing while it cannot be reached; one that
/// cannot be told a folder or a file is not searched.
fn read_folder(folder: &Path, keep: usize) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>, bool)> {
    let mut databases = Vec::new();
    // The `keep` least paths so far: the greatest is dropped as one more comes.
    let mut subfolders = BinaryHeap::new();
    let mut left_out = false;
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        // Not followed: a link, or on Windows a junction, is never a folder here.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if !hidden(&entry) {
                subfolders.push(path);
                if subfolders.len() > keep {
                    subfolders.pop();
                    left_out = true;
                }
            }
            continue;
        }
        if Format::of(&path) == Format::Other {
            continue;
        }
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => databases.push(path),
            Ok(_) => {}
            // Removed while the folder was read, or a link to nothing.
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => databases.push(path),
        }
    }
    Ok((databases, subfolders.into_sorted_vec(), left_out))
}

/// Whether the folder `entry` is hidden, and so not searched: its name starts
/// with a dot (`.git`), or on Windows it has the hidden attribute, as the
/// recycle bin, `System Volume Information` and `AppData`, which holds the
/// bridge's own indexes, have.
fn hidden(entry: &std::fs::DirEntry) -> bool {
    if entry.file_name().as_encoded_bytes().starts_with(b".") {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
        if entry.metadata().is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0) {
            return true;
        }
    }
    false
}

/// What was last read from each source. A source is read again when its
/// signature (sizes and modification times, and for a folder the databases
/// it lists) is not the one it was last read at. The signature is taken
/// before reading, or for a folder from the same walk that lists it, so that
/// a change made while it is read shows next time. A read that fails keeps
/// what was last read and records no signature, so the source is read again
/// on the next request: a folder or file that cannot be read for a while
/// loses none of its databases. A large folder is looked at again only after
/// a while ([`QUICK_FOLDERS`]).
#[derive(Default)]
pub(crate) struct Read {
    window: Kept<Vec<Listed>>,
    /// What the list has seen of the changes of `bridge.toml`, which it
    /// follows as every reader of it does (#70).
    config: config::Seen,
    /// The `databases` it names, as last read.
    configured: Vec<PathBuf>,
    /// The databases of each configured path.
    paths: HashMap<PathBuf, Configured>,
}

/// A configured path: its databases as last read, and until when that read
/// stands without another look.
#[derive(Default)]
struct Configured {
    kept: Kept<Vec<Listed>>,
    /// `None` when the next request looks again ([`Walk::until`]).
    until: Option<Instant>,
    /// What the last walk left out ([`Walk::left_out`]).
    left_out: (usize, bool),
}

impl Configured {
    /// Logs what `walk` left out, for the source `what`, when that is not
    /// what the walk before left out; whether it did. Noted apart from the
    /// list, which a walk that stops at its cap or skips a folder may leave
    /// as it was.
    fn note(&mut self, walk: &Walk, what: &dyn Display) -> bool {
        let left_out = walk.left_out();
        if left_out == self.left_out {
            return false;
        }
        self.left_out = left_out;
        walk.log(what);
        left_out != (0, false)
    }
}

impl Read {
    /// Reads again the sources that need it; whether any was read.
    pub(crate) fn update(&mut self, sources: &Sources) -> bool {
        let window_file = sources.chessbase.as_ref().map(|d| d.join(dbitems::FILE_NAME));
        let mut changed =
            self.window.update(signature(window_file.as_deref()), &dbitems::FILE_NAME, || sources.window());
        let configured = match &sources.config {
            Some(file) => {
                let look = file.look(&mut self.config);
                changed |= look.changed;
                look.config.databases
            }
            None => Vec::new(),
        };
        let before = self.paths.len();
        self.paths.retain(|p, _| configured.contains(p));
        changed |= self.paths.len() != before;
        for (i, path) in configured.iter().enumerate() {
            let entry = self.paths.entry(path.clone()).or_default();
            let started = Instant::now();
            if entry.until.is_some_and(|until| started < until) {
                continue;
            }
            // Named by its place: the path would name the user and the database.
            let place = i + 1;
            let what = format!("databases entry {place} of bridge.toml");
            let walked = walk(path, MAX_FOLDERS);
            entry.until = walked.as_ref().ok().and_then(|w| w.until(started));
            if let Ok(walk) = &walked {
                entry.note(walk, &what);
            }
            changed |= entry.kept.update(folder_signature(path, &walked), &what, || walked.map(Walk::listed));
        }
        self.configured = configured;
        changed
    }

    /// The databases in order: the window's, the configured ones, the fixed ones.
    pub(crate) fn listed(&self, sources: &Sources) -> Vec<Listed> {
        let mut listed = self.window.value.clone();
        for path in &self.configured {
            listed.extend(self.paths.get(path).into_iter().flat_map(|c| c.kept.value.iter().cloned()));
        }
        listed.extend(sources.fixed.iter().cloned().map(Listed::at));
        listed
    }
}

/// One source's last contents read without error.
#[derive(Default)]
struct Kept<T> {
    /// The signature they were read at; `None` until a read succeeds, and
    /// after one fails.
    signature: Option<u64>,
    value: T,
    /// The last read failed; its error has been logged.
    failing: bool,
}

impl<T> Kept<T> {
    /// Reads the source with `read` unless it was last read at `signature`;
    /// whether it was read. `what` names the source in the log, and the
    /// error it logs must name no path.
    fn update(&mut self, signature: u64, what: &dyn Display, read: impl FnOnce() -> Result<T, String>) -> bool {
        if self.signature == Some(signature) {
            return false;
        }
        match read() {
            Ok(value) => {
                (self.value, self.signature, self.failing) = (value, Some(signature), false);
                true
            }
            Err(e) => {
                if !self.failing {
                    crate::log!("{what} cannot be read, keeping what was read before: {e}");
                }
                (self.signature, self.failing) = (None, true);
                false
            }
        }
    }
}

/// The size and modification time of the file or folder at `path`.
pub(crate) fn signature(path: Option<&Path>) -> u64 {
    let mut hash = Hash::new();
    if let Some(path) = path {
        hash.write_file(path);
    }
    hash.finish()
}

/// [`signature`] of a configured path, and the paths `walked`, its walk, lists
/// for it: for a folder, its database files of every format, at every depth. A
/// folder's own time moves with the kernel's coarse clock, a few milliseconds
/// a step, and its size rarely changes, so a database added right after a
/// listing could leave both as they were and stay unseen until the folder
/// changed again; a database added in a subfolder changes neither at all.
/// Taken from the walk itself, the signature changes whenever the list does.
/// The files' sizes and times are left out: they never decide whether a file
/// is listed, and a PGN file that grows, or a database being saved, would
/// otherwise have the folder read again and the list rebuilt on every request
/// while it is written.
fn folder_signature(path: &Path, walked: &Result<Walk, String>) -> u64 {
    let mut hash = Hash::new();
    hash.write_file(path);
    match walked {
        Ok(walk) => {
            for database in &walk.found {
                hash.write(database.as_os_str().as_encoded_bytes());
            }
        }
        // Unlike the signature of any list, so that the read runs, logs why
        // the folder cannot be read, and is tried again until it can be.
        Err(_) => hash.write(&[0xff]),
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// [`folder_signature`] of `dir`, walked now.
    fn signature_of(dir: &Path) -> u64 {
        folder_signature(dir, &walk(dir, MAX_FOLDERS))
    }

    /// A folder's signature follows which databases it lists, not their sizes
    /// and times: a PGN file that grows and a database being saved leave it
    /// as it was, so the folder is not read again for them.
    #[test]
    fn a_database_written_keeps_its_folder_signature() {
        let f = cbformat::fixture::pgn_file("folder-signature", b"[White \"A\"]\n\n1. e4 *\n");
        let (pgn, twocbh) = (f.dir().join("db.pgn"), f.dir().join("db.2cbh"));
        std::fs::write(&twocbh, [0u8; 32]).unwrap();
        let listed: Vec<PathBuf> = expand(f.dir()).unwrap().into_iter().map(|l| l.path).collect();
        assert_eq!(listed, [twocbh.clone(), pgn.clone()]);
        let before = signature_of(f.dir());
        let append = |path: &Path, bytes: &[u8]| {
            std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
        };
        append(&pgn, b"\n[White \"B\"]\n\n1. d4 *\n");
        append(&twocbh, &[0u8; 32]);
        assert_eq!(signature_of(f.dir()), before);
    }

    /// A temporary folder, removed on drop, with `files` written in it as
    /// empty files, their folders made.
    fn tree(name: &str, files: &[&str]) -> cbformat::fixture::TempDb {
        let dir = std::env::temp_dir().join(format!("bridge-sources-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for file in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        cbformat::fixture::TempDb::at(dir)
    }

    /// The databases `found` names, by their paths from `dir` with `/`.
    fn relative(dir: &Path, found: &[PathBuf]) -> Vec<String> {
        found.iter().map(|p| p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/")).collect()
    }

    fn expanded(dir: &Path) -> Vec<String> {
        let paths: Vec<PathBuf> = expand(dir).unwrap().into_iter().map(|l| l.path).collect();
        relative(dir, &paths)
    }

    /// A folder gives the databases of every folder below it, at any depth
    /// and in path order (#320); a folder named like a database is searched
    /// like any other, and a dot folder is not searched.
    #[test]
    fn a_folder_gives_the_databases_of_every_folder_below_it() {
        let t = tree(
            "depth",
            &[
                "A.pgn",
                "notes.txt",
                "sub/B.2cbh",
                "sub/B.2cbg",
                "sub/deeper/C.CBH",
                "sub/deeper/deepest/D.pgn",
                "z.2cbh/E.pgn",
                ".git/F.pgn",
            ],
        );
        assert_eq!(
            expanded(t.dir()),
            ["A.pgn", "sub/B.2cbh", "sub/deeper/C.CBH", "sub/deeper/deepest/D.pgn", "z.2cbh/E.pgn"]
        );
    }

    /// A link to a folder is not followed, not even one to a folder above it,
    /// which would loop; a link to a database file is a database.
    #[cfg(unix)]
    #[test]
    fn links_to_folders_are_not_followed() {
        let t = tree("links", &["inner/G.pgn"]);
        let other = tree("links-other", &["H.pgn"]);
        std::os::unix::fs::symlink(t.dir(), t.dir().join("inner/loop")).unwrap();
        std::os::unix::fs::symlink(other.dir(), t.dir().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(other.dir().join("H.pgn"), t.dir().join("I.pgn")).unwrap();
        assert_eq!(expanded(t.dir()), ["I.pgn", "inner/G.pgn"]);
    }

    /// A folder with the hidden attribute is not searched.
    #[cfg(windows)]
    #[test]
    fn hidden_folders_are_not_searched() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

        let t = tree("hidden", &["shown/L.pgn", "hidden/M.pgn"]);
        let wide: Vec<u16> = t.dir().join("hidden").as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) }, 0);
        assert_eq!(expanded(t.dir()), ["shown/L.pgn"]);
    }

    /// The walk reads folders breadth first, so that the folders near the top
    /// are the ones searched when it stops at its cap, and says that it
    /// stopped.
    #[test]
    fn the_walk_stops_at_its_cap_with_the_folders_near_the_top_read() {
        let t = tree("cap", &["Z.pgn", "a/Y.pgn", "a/b/c/X.pgn", "m/W.pgn"]);
        let read = |max: usize| {
            let walk = walk(t.dir(), max).unwrap();
            (relative(t.dir(), &walk.found), walk.cut)
        };
        assert_eq!(read(3), (vec!["Z.pgn".to_string(), "a/Y.pgn".into(), "m/W.pgn".into()], true));
        assert_eq!(read(4), (vec!["Z.pgn".to_string(), "a/Y.pgn".into(), "m/W.pgn".into()], true));
        let all = vec!["Z.pgn".to_string(), "a/Y.pgn".into(), "a/b/c/X.pgn".into(), "m/W.pgn".into()];
        assert_eq!(read(5), (all.clone(), false), "five folders, all read");
        assert_eq!(read(MAX_FOLDERS), (all, false));
    }

    /// A folder below the configured one that may not be read is skipped and
    /// counted, and the others are listed; the configured folder that may not
    /// be read fails the walk, which keeps what was read before. Unix only,
    /// and skipped when the tests run with the rights to read any folder.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_may_not_be_read_is_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let t = tree("closed", &["open/K.pgn", "closed/J.pgn"]);
        let set =
            |path: &Path, mode: u32| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        let closed = t.dir().join("closed");
        set(&closed, 0o000);
        if std::fs::read_dir(&closed).is_ok() {
            set(&closed, 0o755);
            return;
        }
        let walk = walk(t.dir(), MAX_FOLDERS).unwrap();
        assert_eq!((relative(t.dir(), &walk.found), walk.skipped), (vec!["open/K.pgn".to_string()], 1));
        set(&closed, 0o755);
        set(t.dir(), 0o000);
        assert!(expand(t.dir()).is_err());
        set(t.dir(), 0o755);
    }

    /// A database added or removed deep in a configured folder changes the
    /// folder's signature, although no time of the configured folder moves.
    #[test]
    fn a_database_added_in_a_subfolder_changes_the_folder_signature() {
        let t = tree("deep-signature", &["sub/deeper/One.pgn"]);
        let before = signature_of(t.dir());
        let two = t.dir().join("sub/deeper/Two.pgn");
        std::fs::write(&two, b"").unwrap();
        assert_ne!(signature_of(t.dir()), before);
        std::fs::remove_file(&two).unwrap();
        assert_eq!(signature_of(t.dir()), before);
    }

    /// A wide folder keeps no more than the walk's room for folders: its
    /// first subfolders in path order are searched, and the walk says that
    /// it left the others out.
    #[test]
    fn a_wide_folder_keeps_only_the_folders_the_walk_has_room_for() {
        let many: Vec<String> = (0..40).map(|i| format!("w{i:02}/D{i:02}.pgn")).collect();
        let t = tree("wide", &many.iter().map(String::as_str).collect::<Vec<_>>());
        let (_, kept, left_out) = read_folder(t.dir(), 5).unwrap();
        assert_eq!(relative(t.dir(), &kept), ["w00", "w01", "w02", "w03", "w04"]);
        assert!(left_out);
        let (_, kept, left_out) = read_folder(t.dir(), 40).unwrap();
        assert_eq!((kept.len(), left_out), (40, false));

        let walk = walk(t.dir(), 10).unwrap();
        let first: Vec<String> = (0..9).map(|i| format!("w{i:02}/D{i:02}.pgn")).collect();
        assert_eq!((relative(t.dir(), &walk.found), walk.folders, walk.cut), (first, 10, true));
    }

    /// A database file that cannot be examined, here a link into a folder
    /// closed to this user, is listed and costs its folder nothing: the
    /// other databases there and below are listed, and no folder counts as
    /// skipped. Unix only.
    #[cfg(unix)]
    #[test]
    fn a_database_that_cannot_be_examined_leaves_its_folder_listed() {
        use std::os::unix::fs::PermissionsExt;

        let t = tree("unexamined", &["sub/good.pgn", "sub/deeper/d.pgn", "sibling/other.pgn"]);
        let closed = tree("unexamined-closed", &["x.pgn"]);
        std::os::unix::fs::symlink(closed.dir().join("x.pgn"), t.dir().join("sub/blocked.pgn")).unwrap();
        std::fs::set_permissions(closed.dir(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let walk = walk(t.dir(), MAX_FOLDERS);
        std::fs::set_permissions(closed.dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let walk = walk.unwrap();
        assert_eq!(
            (relative(t.dir(), &walk.found), walk.skipped),
            (
                vec![
                    "sibling/other.pgn".to_string(),
                    "sub/blocked.pgn".into(),
                    "sub/deeper/d.pgn".into(),
                    "sub/good.pgn".into()
                ],
                0
            )
        );
    }

    /// What a walk leaves out is logged when it changes, whether or not the
    /// list does: a walk newly stopped at its cap, or a folder newly closed,
    /// may leave the databases found as they were.
    #[test]
    fn what_a_walk_leaves_out_is_logged_when_it_changes() {
        let walk = |skipped, cut| Walk { skipped, cut, ..Walk::default() };
        let mut configured = Configured::default();
        assert!(!configured.note(&walk(0, false), &"entry"));
        assert!(configured.note(&walk(0, true), &"entry"), "newly cut");
        assert!(!configured.note(&walk(0, true), &"entry"), "logged once");
        assert!(configured.note(&walk(2, true), &"entry"), "and newly skipping");
        assert!(!configured.note(&walk(0, false), &"entry"), "nothing left out, nothing to log");
        assert!(configured.note(&walk(0, true), &"entry"), "cut again");
    }

    /// The list notes a folder newly closed below a configured one although
    /// its databases stay as they were. Unix only, and skipped when the
    /// tests run with the rights to read any folder.
    #[cfg(unix)]
    #[test]
    fn a_folder_newly_closed_is_noted_with_the_list_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let t = tree("newly-closed", &["A.pgn", "empty/notes.txt", "conf/x.txt"]);
        let toml = t.dir().join("conf/bridge.toml");
        std::fs::write(&toml, format!("databases = ['{}']\n", t.dir().display())).unwrap();
        let sources = Sources { config: Some(Arc::new(config::Watched::new(toml))), ..Sources::default() };
        let mut read = Read::default();
        assert!(read.update(&sources));
        assert_eq!(read.paths[t.dir()].left_out, (0, false));
        let empty = t.dir().join("empty");
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&empty).is_err() {
            assert!(!read.update(&sources), "the list stays as it was");
            assert_eq!(read.paths[t.dir()].left_out, (1, false));
        }
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A walk of at most [`QUICK_FOLDERS`] folders is repeated on the next
    /// request; a longer one stands for [`LOOK_EVERY`] at least.
    #[test]
    fn only_a_walk_of_many_folders_stands_for_a_while() {
        let quick: Vec<String> = (1..QUICK_FOLDERS).map(|i| format!("f{i}/notes.txt")).collect();
        let t = tree("quick", &quick.iter().map(String::as_str).collect::<Vec<_>>());
        let started = Instant::now();
        let walked = walk(t.dir(), MAX_FOLDERS).unwrap();
        assert_eq!((walked.folders, walked.until(started)), (QUICK_FOLDERS, None));
        std::fs::create_dir(t.dir().join("one-more")).unwrap();
        let walked = walk(t.dir(), MAX_FOLDERS).unwrap();
        assert_eq!(walked.folders, QUICK_FOLDERS + 1);
        assert!(walked.until(started).is_some_and(|until| until >= started + LOOK_EVERY));
    }

    /// The list does not walk a large configured folder again until its last
    /// walk has stood for its while, and then sees what changed in it.
    #[test]
    fn a_large_folder_is_looked_at_again_only_after_a_while() {
        let many: Vec<String> = (0..QUICK_FOLDERS).map(|i| format!("f{i}/notes.txt")).collect();
        let t = tree("large", &many.iter().map(String::as_str).collect::<Vec<_>>());
        std::fs::write(t.dir().join("f0/A.pgn"), b"").unwrap();
        let toml = t.dir().join("f1/bridge.toml");
        std::fs::write(&toml, format!("databases = ['{}']\n", t.dir().display())).unwrap();
        let sources = Sources { config: Some(Arc::new(config::Watched::new(toml))), ..Sources::default() };
        let names = |read: &Read| read.listed(&sources).into_iter().map(|l| l.name).collect::<Vec<_>>();
        let mut read = Read::default();
        assert!(read.update(&sources));
        assert_eq!(names(&read), ["A"]);
        assert!(read.paths[t.dir()].until.is_some());

        std::fs::write(t.dir().join("f5/B.pgn"), b"").unwrap();
        assert!(!read.update(&sources), "the last walk stands");
        assert_eq!(names(&read), ["A"]);
        read.paths.get_mut(t.dir()).unwrap().until = None;
        assert!(read.update(&sources));
        assert_eq!(names(&read), ["A", "B"]);
    }
}
