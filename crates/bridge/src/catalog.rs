//! The databases the bridge serves: identity, state, and the open handle,
//! reopened whenever the files change (`docs/api.md`, "Database identity and
//! generations"). The list follows its sources (`crate::sources`) and is read
//! again when they change.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cbformat::view::Base;

use crate::fetch::{Cloud, Progress, Serial, System};
use crate::pgnindex::{self, Opening};
use crate::search::Indexes;
use crate::sources::{Listed, Read, Sources};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    TwoCbh,
    Cbh,
    Pgn,
    Other,
}

impl Format {
    /// A listed path's format, as `cbformat` tells it by the extension; any
    /// other file is `Other`, and never a stem to guess from.
    pub fn of(path: &Path) -> Format {
        match cbformat::view::Format::of_extension(path) {
            Some(cbformat::view::Format::TwoCbh) => Format::TwoCbh,
            Some(cbformat::view::Format::Cbh) => Format::Cbh,
            Some(cbformat::view::Format::Pgn) => Format::Pgn,
            None => Format::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::TwoCbh => "2cbh",
            Format::Cbh => "cbh",
            Format::Pgn => "pgn",
            Format::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    /// A PGN file whose index is being built; see [`Entry::opening`].
    Opening,
    Missing,
    /// Files kept only in the cloud; opening the database for its games
    /// downloads them.
    CloudOnly,
    /// Being downloaded; see [`Entry::progress`].
    Downloading,
    Unsupported,
    Unreadable,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Ready => "ready",
            State::Opening => "opening",
            State::Missing => "missing",
            State::CloudOnly => "cloudOnly",
            State::Downloading => "downloading",
            State::Unsupported => "unsupported",
            State::Unreadable => "unreadable",
        }
    }
}

/// A database that is open, with the generation it was opened at.
#[derive(Clone)]
pub struct Opened {
    pub db: Arc<Base>,
    pub generation: u64,
    /// Sort orders, names and searches built for this generation.
    pub indexes: Arc<Indexes>,
}

pub struct Entry {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub format: Format,
    /// Set when the database left the list: it is then reported `missing`.
    removed: AtomicBool,
    held: Arc<Held>,
    shared: Arc<Shared>,
}

/// What entries share: how cloud files are seen, the download queue, and the
/// index builds of PGN files.
struct Shared {
    cloud: Arc<dyn Cloud>,
    downloads: Arc<Serial>,
    pgn: pgnindex::Registry,
}

/// What stays with a database when the list is read again or the window
/// renames it: the open handle and the download.
#[derive(Default)]
struct Held {
    open: Mutex<Option<Opened>>,
    /// The download running or queued.
    running: Mutex<Option<Arc<Progress>>>,
    /// The last download read every file, yet a file kept its cloud-only mark.
    kept: AtomicBool,
}

/// What the metadata of a database's files tells, without reading them.
struct Files {
    /// `None` when the header file is missing.
    generation: Option<u64>,
    /// The files that are there, each with its size and whether it is cloud-only.
    present: Vec<(PathBuf, u64, bool)>,
    /// Some file is there but is not a regular file: a directory, a pipe or a
    /// device, which could block a reader or mislead it.
    irregular: bool,
}

impl Files {
    fn size(&self) -> u64 {
        self.present.iter().map(|f| f.1).sum()
    }

    fn cloud_only(&self) -> bool {
        self.present.iter().any(|f| f.2)
    }
}

impl Entry {
    fn new(listed: Listed, shared: &Arc<Shared>, held: Arc<Held>) -> Entry {
        Entry {
            id: id_of(&listed.path),
            name: listed.name,
            format: Format::of(&listed.path),
            path: listed.path,
            removed: AtomicBool::new(false),
            held,
            shared: Arc::clone(shared),
        }
    }

    /// Whether the database is on the list; one that left it stays `missing`
    /// until the bridge restarts.
    pub fn listed(&self) -> bool {
        !self.removed.load(Ordering::Relaxed)
    }

    /// The database's state, opening it if it is ready.
    pub fn state(&self) -> State {
        match self.open() {
            Ok(_) => State::Ready,
            Err(state) => state,
        }
    }

    /// The open database at its current generation: the handle already held
    /// when the files have not changed, a freshly opened one when they have.
    /// A database with any file marked cloud-only is never opened, since
    /// reading that file would download it: see [`Entry::open_to_read`].
    pub fn open(&self) -> Result<Opened, State> {
        if self.removed.load(Ordering::Relaxed) {
            return Err(State::Missing);
        }
        if self.format == Format::Other {
            return Err(if self.path.exists() { State::Unsupported } else { State::Missing });
        }
        let files = self.files();
        let generation = files.generation.ok_or(State::Missing)?;
        if files.irregular {
            return Err(State::Unreadable);
        }
        if lock(&self.held.running).is_some() {
            return Err(State::Downloading);
        }
        // The current marks alone decide: a file the provider moves back to
        // the cloud makes the database cloud-only again, whatever was read.
        if files.cloud_only() {
            return Err(State::CloudOnly);
        }
        let mut slot = lock(&self.held.open);
        if let Some(open) = slot.as_ref().filter(|o| o.generation == generation) {
            return Ok(open.clone());
        }
        let db = match self.format {
            Format::Pgn => match self.shared.pgn.open(&self.id, &self.path, generation) {
                Opening::Ready(db) => Base::Pgn(db),
                Opening::Pending(_) => return Err(State::Opening),
                Opening::Failed => return Err(State::Unreadable),
            },
            _ => Base::open(&self.path).map_err(|_| State::Unreadable)?,
        };
        let open = Opened { db: Arc::new(db), generation, indexes: Indexes::shared() };
        *slot = Some(open.clone());
        Ok(open)
    }

    /// The build of a PGN file's index running or queued: the bytes of the
    /// file read, of all.
    pub fn opening(&self) -> Option<Arc<Progress>> {
        (self.format == Format::Pgn).then(|| self.shared.pgn.progress(&self.id)).flatten()
    }

    /// [`Entry::open`] for reading games: a cloud-only database starts
    /// downloading and is reported [`State::Downloading`]; it stays
    /// [`State::CloudOnly`] when the download cannot start.
    pub fn open_to_read(&self) -> Result<Opened, State> {
        match self.open() {
            Err(State::CloudOnly) if self.download() => Err(State::Downloading),
            other => other,
        }
    }

    /// Whether the last download read every file, yet the provider kept a
    /// file marked cloud-only, so that the database stayed cloud-only.
    pub fn marks_kept(&self) -> bool {
        self.held.kept.load(Ordering::Relaxed)
    }

    /// The download running or queued, if any.
    pub fn progress(&self) -> Option<Arc<Progress>> {
        lock(&self.held.running).clone()
    }

    /// The size of the database's files, in bytes.
    pub fn size(&self) -> u64 {
        self.files().size()
    }

    /// Queues the reading of the database's cloud-only files, in order. The
    /// state afterwards follows the marks as they then are: a failed download,
    /// a file moved to the cloud meanwhile or a mark the provider keeps leave
    /// the database cloud-only, until the next request for its games.
    /// Whether a download now runs or waits.
    fn download(&self) -> bool {
        let files = self.files();
        let local: u64 = files.present.iter().filter(|f| !f.2).map(|f| f.1).sum();
        let progress = Arc::new(Progress::new(local, files.size()));
        {
            let mut running = lock(&self.held.running);
            if running.is_some() {
                return true;
            }
            *running = Some(Arc::clone(&progress));
        }
        self.held.kept.store(false, Ordering::Relaxed);
        let cloud_files: Vec<PathBuf> = files.present.into_iter().filter(|f| f.2).map(|f| f.0).collect();
        let (held, cloud, path, name, format) =
            (Arc::clone(&self.held), Arc::clone(&self.shared.cloud), self.path.clone(), self.name.clone(), self.format);
        // Ends the download however the job ends, and also when it is
        // dropped unrun because no thread could start.
        let done = Done(Arc::clone(&held));
        self.shared.downloads.submit(Box::new(move || {
            let _done = done;
            if let Err(e) = cloud_files.iter().try_for_each(|file| cloud.fetch(file, &mut |n| progress.add(n))) {
                eprintln!("oschess-bridge: downloading {name} failed: {e}");
                return;
            }
            let after = generation_of(&path, format, &*cloud);
            let kept: Vec<&PathBuf> =
                after.present.iter().filter(|f| f.2 && cloud_files.contains(&f.0)).map(|f| &f.0).collect();
            if !kept.is_empty() {
                held.kept.store(true, Ordering::Relaxed);
                eprintln!(
                    "oschess-bridge: downloaded {name}, but {} of its files still show as kept in the cloud",
                    kept.len()
                );
            }
        }))
    }

    /// A hash of the sizes and modification times of the database's files;
    /// `None` when the header file is missing.
    pub fn generation(&self) -> Option<u64> {
        self.files().generation
    }

    fn files(&self) -> Files {
        generation_of(&self.path, self.format, &*self.shared.cloud)
    }
}

/// Ends a download when dropped.
struct Done(Arc<Held>);

impl Drop for Done {
    fn drop(&mut self) {
        *lock(&self.0.running) = None;
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The metadata of the database at `path`, of `format`: its generation and
/// files. Metadata only, following links: nothing is opened. A PGN database
/// is its one file.
fn generation_of(path: &Path, format: Format, cloud: &dyn Cloud) -> Files {
    let stem = path.with_extension("");
    let mut hash = Hash::new();
    let mut files = Files { generation: None, present: Vec::new(), irregular: false };
    let extensions: &[&str] = match format {
        // The files the classic reader opens: the search boosters and other
        // optional files are neither read nor downloaded.
        Format::Cbh => &cbformat::cbh::READ,
        Format::Pgn => &[""],
        _ => &cbformat::v2::EXTENSIONS,
    };
    for &ext in extensions {
        let path = match format {
            Format::Pgn => path.to_path_buf(),
            _ => {
                let mut path = stem.clone().into_os_string();
                path.push(ext);
                PathBuf::from(path)
            }
        };
        match std::fs::metadata(&path) {
            Ok(m) if !m.is_file() => {
                files.irregular = true;
                hash.write(&[0xfe]);
            }
            Ok(m) => {
                hash.write_meta(&m);
                let cloud_only = cloud.is_cloud_only(&path, &m);
                files.present.push((path, m.len(), cloud_only));
            }
            Err(_) if ext == extensions[0] => return files,
            Err(_) => hash.write(&[0xff]),
        }
    }
    files.generation = Some(hash.finish());
    files
}

pub struct Catalog {
    /// The position indexes of the databases, and the queue that builds them.
    pub explorer: crate::explorer::Registry,
    sources: Sources,
    shared: Arc<Shared>,
    listing: Mutex<Listing>,
    /// Called after the sources are read, before the list is rebuilt.
    after_read: Mutex<Option<Hook>>,
    /// Whether the index folder is swept of what the list no longer uses
    /// (#60), and when it last was.
    sweeping: AtomicBool,
    swept: Mutex<Option<Instant>>,
}

/// The longest the index folder goes unswept while the list is asked for.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

type Hook = Box<dyn Fn() + Send + Sync>;

/// The list as last read.
struct Listing {
    read: Read,
    /// The listed databases in order, then those that left the list.
    entries: Vec<Arc<Entry>>,
}

impl Catalog {
    /// The databases at `paths`, in order, each once.
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Catalog {
        Catalog::with_sources(Sources { fixed: paths.into_iter().collect(), ..Sources::default() }, Arc::new(System))
    }

    /// The databases of `sources`, read now and again whenever they change.
    pub fn with_sources(sources: Sources, cloud: Arc<dyn Cloud>) -> Catalog {
        let shared = Arc::new(Shared { cloud, downloads: Arc::default(), pgn: pgnindex::Registry::default() });
        let listing = Listing { read: Read::default(), entries: Vec::new() };
        let catalog = Catalog {
            explorer: crate::explorer::Registry::default(),
            sources,
            shared,
            listing: Mutex::new(listing),
            after_read: Mutex::new(None),
            sweeping: AtomicBool::new(false),
            swept: Mutex::new(None),
        };
        catalog.refresh(true);
        catalog
    }

    /// The queue downloads run in.
    pub fn downloads(&self) -> &Serial {
        &self.shared.downloads
    }

    /// The index builds of PGN files.
    pub fn pgn(&self) -> &pgnindex::Registry {
        &self.shared.pgn
    }

    /// Sets a function called each time the sources have been read, before
    /// the list is rebuilt from them. Tests use it to change a source at
    /// exactly that moment.
    pub fn after_read(&self, hook: impl Fn() + Send + Sync + 'static) {
        *lock(&self.after_read) = Some(Box::new(hook));
    }

    /// The databases, the list read again first if its sources changed.
    pub fn entries(&self) -> Vec<Arc<Entry>> {
        let rebuilt = self.refresh(false);
        let entries = lock(&self.listing).entries.clone();
        self.sweep_if_due(&entries, rebuilt);
        entries
    }

    /// Sweeps the index folder, and the PGN header index folder, of what the
    /// list no longer uses (#60), now and
    /// from now on: after each change of the list, and at least once a minute
    /// while the list is asked for. The bridge calls it once it has its data
    /// folder; tests call it once they have set theirs.
    pub fn sweep_indexes(&self) {
        self.sweeping.store(true, Ordering::Relaxed);
        *lock(&self.swept) = None;
        self.entries();
    }

    fn sweep_if_due(&self, entries: &[Arc<Entry>], rebuilt: bool) {
        if !self.sweeping.load(Ordering::Relaxed) {
            return;
        }
        let mut swept = lock(&self.swept);
        if !rebuilt && swept.is_some_and(|at| at.elapsed() < SWEEP_EVERY) {
            return;
        }
        *swept = Some(Instant::now());
        drop(swept);
        // A database only missing, in the cloud or downloading is still on
        // the list, and keeps its index.
        let listed = entries.iter().filter(|e| e.listed()).map(|e| e.id.clone()).collect();
        self.explorer.sweep(&listed);
        self.shared.pgn.sweep(&listed);
    }

    pub fn get(&self, id: &str) -> Option<Arc<Entry>> {
        lock(&self.listing).entries.iter().find(|e| e.id == id).cloned()
    }

    /// Reads again the sources that changed or failed last time
    /// ([`Read`]), and rebuilds the list when any was read, or `always`;
    /// whether it did.
    fn refresh(&self, always: bool) -> bool {
        let mut listing = lock(&self.listing);
        let changed = listing.read.update(&self.sources);
        if let Some(hook) = lock(&self.after_read).as_ref() {
            hook();
        }
        if changed || always {
            let listed = listing.read.listed(&self.sources);
            listing.entries = self.merge(&listing.entries, listed);
        }
        changed || always
    }

    /// The new list: each database once, in order, keeping the entry (and its
    /// open handle) of a database already listed; then the databases that
    /// left the list, marked removed.
    fn merge(&self, old: &[Arc<Entry>], listed: Vec<Listed>) -> Vec<Arc<Entry>> {
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for item in listed {
            let id = id_of(&item.path);
            if !seen.insert(id.clone()) {
                continue;
            }
            let entry = match old.iter().find(|e| e.id == id) {
                Some(e) if e.name == item.name => Arc::clone(e),
                // Renamed in the window: the same database under its new name.
                Some(e) => Arc::new(Entry::new(item, &self.shared, Arc::clone(&e.held))),
                None => Arc::new(Entry::new(item, &self.shared, Arc::default())),
            };
            entry.removed.store(false, Ordering::Relaxed);
            entries.push(entry);
        }
        for e in old.iter().filter(|e| !seen.contains(&e.id)) {
            e.removed.store(true, Ordering::Relaxed);
            entries.push(Arc::clone(e));
        }
        entries
    }
}

/// 16 hexadecimal characters from the normalised path: separators unified and,
/// on Windows, case folded, since its paths are case-insensitive.
pub fn id_of(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text = text.to_lowercase();
    }
    let mut hash = Hash::new();
    hash.write(text.as_bytes());
    format!("{:016x}", hash.finish())
}

/// 64-bit FNV-1a: stable across runs and platforms, unlike the standard
/// library's randomly keyed hasher.
pub(crate) struct Hash(u64);

impl Hash {
    pub(crate) fn new() -> Hash {
        Hash(0xcbf2_9ce4_8422_2325)
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    /// A file's size and modification time.
    fn write_meta(&mut self, m: &std::fs::Metadata) {
        self.write(&m.len().to_le_bytes());
        let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        self.write(&modified.map_or(0, |d| d.as_nanos()).to_le_bytes());
    }

    /// [`Hash::write_meta`] of the file at `path`, or a marker when it is missing.
    pub(crate) fn write_file(&mut self, path: &Path) {
        match std::fs::metadata(path) {
            Ok(m) => self.write_meta(&m),
            Err(_) => self.write(&[0xff]),
        }
    }

    pub(crate) fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = id_of(Path::new("C:\\Bases\\Mega.2cbh"));
        assert_eq!(a.len(), 16);
        assert_eq!(a, id_of(Path::new("C:/Bases/Mega.2cbh")));
        assert_ne!(a, id_of(Path::new("C:/Bases/Mega2.2cbh")));
        // FNV-1a test vector.
        let mut h = Hash::new();
        h.write(b"a");
        assert_eq!(h.finish(), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn formats_and_states_without_files() {
        assert_eq!(Format::of(Path::new("x.2CBH")), Format::TwoCbh);
        assert_eq!(Format::of(Path::new("x.cbh")), Format::Cbh);
        assert_eq!(Format::of(Path::new("x.pgn")), Format::Pgn);
        let catalog = Catalog::new([PathBuf::from("/a/x.2cbh"), PathBuf::from("/a/x.2cbh"), PathBuf::from("/a/y.pgn")]);
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "x");
        assert_eq!(entries[0].state(), State::Missing);
        assert_eq!(entries[1].state(), State::Missing);
    }
}
