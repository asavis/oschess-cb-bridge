//! The databases the bridge serves: identity, state, and the open handle,
//! reopened whenever the files change (`docs/api.md`, "Database identity and
//! generations"). The list follows its sources (`crate::sources`) and is read
//! again when they change.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cbformat::v2::{Database, EXTENSIONS};

use crate::fetch::{Cloud, Progress, Serial, System};
use crate::sources::{self, Listed, Sources};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    TwoCbh,
    Cbh,
    Pgn,
    Other,
}

impl Format {
    pub fn of(path: &Path) -> Format {
        match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("2cbh") => Format::TwoCbh,
            Some("cbh") => Format::Cbh,
            Some("pgn") => Format::Pgn,
            _ => Format::Other,
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
    pub db: Arc<Database>,
    pub generation: u64,
}

pub struct Entry {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub format: Format,
    open: Mutex<Option<Opened>>,
    /// Set when the database left the list: it is then reported `missing`.
    removed: AtomicBool,
    fetch: Arc<Fetch>,
    shared: Arc<Shared>,
}

/// What entries share: how cloud files are seen, and the download queue.
struct Shared {
    cloud: Arc<dyn Cloud>,
    downloads: Arc<Serial>,
}

/// A database's download: the one running or queued, and the generation its
/// files had when they were last all read.
#[derive(Default)]
struct Fetch {
    running: Mutex<Option<Arc<Progress>>>,
    fetched: Mutex<Option<u64>>,
}

/// What the metadata of a database's files tells, without reading them.
struct Files {
    /// `None` when the header file is missing.
    generation: Option<u64>,
    /// The files that are there, each with its size and whether it is cloud-only.
    present: Vec<(PathBuf, u64, bool)>,
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
    fn new(listed: Listed, shared: &Arc<Shared>, fetch: Arc<Fetch>) -> Entry {
        Entry {
            id: id_of(&listed.path),
            name: listed.name,
            format: Format::of(&listed.path),
            path: listed.path,
            open: Mutex::new(None),
            removed: AtomicBool::new(false),
            fetch,
            shared: Arc::clone(shared),
        }
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
    /// A cloud-only database stays unread: see [`Entry::open_to_read`].
    pub fn open(&self) -> Result<Opened, State> {
        if self.removed.load(Ordering::Relaxed) {
            return Err(State::Missing);
        }
        if self.format != Format::TwoCbh {
            return Err(if self.path.exists() { State::Unsupported } else { State::Missing });
        }
        let files = self.files();
        let generation = files.generation.ok_or(State::Missing)?;
        if lock(&self.fetch.running).is_some() {
            return Err(State::Downloading);
        }
        if files.cloud_only() && *lock(&self.fetch.fetched) != Some(generation) {
            return Err(State::CloudOnly);
        }
        let mut slot = lock(&self.open);
        if let Some(open) = slot.as_ref().filter(|o| o.generation == generation) {
            return Ok(open.clone());
        }
        let db = Database::open(&self.path).map_err(|_| State::Unreadable)?;
        let open = Opened { db: Arc::new(db), generation };
        *slot = Some(open.clone());
        Ok(open)
    }

    /// [`Entry::open`] for reading games: a cloud-only database starts
    /// downloading and is reported [`State::Downloading`].
    pub fn open_to_read(&self) -> Result<Opened, State> {
        match self.open() {
            Err(State::CloudOnly) => {
                self.download();
                Err(State::Downloading)
            }
            other => other,
        }
    }

    /// The download running or queued, if any.
    pub fn progress(&self) -> Option<Arc<Progress>> {
        lock(&self.fetch.running).clone()
    }

    /// The size of the database's files, in bytes.
    pub fn size(&self) -> u64 {
        self.files().size()
    }

    /// Queues the reading of the database's cloud-only files, in order. A
    /// failed download leaves the database cloud-only.
    fn download(&self) {
        let mut running = lock(&self.fetch.running);
        if running.is_some() {
            return;
        }
        let files = self.files();
        let local: u64 = files.present.iter().filter(|f| !f.2).map(|f| f.1).sum();
        let progress = Arc::new(Progress::new(local, files.size()));
        *running = Some(Arc::clone(&progress));
        let cloud_files: Vec<PathBuf> = files.present.into_iter().filter(|f| f.2).map(|f| f.0).collect();
        let (fetch, cloud, path, name) =
            (Arc::clone(&self.fetch), Arc::clone(&self.shared.cloud), self.path.clone(), self.name.clone());
        self.shared.downloads.submit(Box::new(move || {
            // Clears the running download however the job ends.
            let _done = Done(Arc::clone(&fetch));
            let result = cloud_files.iter().try_for_each(|file| cloud.fetch(file, &mut |n| progress.add(n)));
            match result {
                Ok(()) => *lock(&fetch.fetched) = generation_of(&path, &*cloud).generation,
                Err(e) => eprintln!("oschess-bridge: downloading {name} failed: {e}"),
            }
        }));
    }

    /// A hash of the sizes and modification times of the database's files;
    /// `None` when the header file is missing.
    pub fn generation(&self) -> Option<u64> {
        self.files().generation
    }

    fn files(&self) -> Files {
        generation_of(&self.path, &*self.shared.cloud)
    }
}

/// Ends a download when dropped.
struct Done(Arc<Fetch>);

impl Drop for Done {
    fn drop(&mut self) {
        *lock(&self.0.running) = None;
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The metadata of the 2CBH database at `path`: its generation and files.
fn generation_of(path: &Path, cloud: &dyn Cloud) -> Files {
    let stem = path.with_extension("");
    let mut hash = Hash::new();
    let mut present = Vec::new();
    for ext in EXTENSIONS {
        let mut path = stem.clone().into_os_string();
        path.push(ext);
        let path = PathBuf::from(path);
        match std::fs::metadata(&path) {
            Ok(m) => {
                hash.write_meta(&m);
                let cloud_only = cloud.is_cloud_only(&path, &m);
                present.push((path, m.len(), cloud_only));
            }
            Err(_) if ext == ".2cbh" => return Files { generation: None, present },
            Err(_) => hash.write(&[0xff]),
        }
    }
    Files { generation: Some(hash.finish()), present }
}

pub struct Catalog {
    sources: Sources,
    shared: Arc<Shared>,
    listing: Mutex<Listing>,
}

/// The list as last read.
struct Listing {
    /// The sources' signature when they were read.
    signature: u64,
    /// The last lists read without error, kept when a later read fails.
    window: Vec<Listed>,
    configured: Vec<PathBuf>,
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
        let shared = Arc::new(Shared { cloud, downloads: Arc::default() });
        let listing = Listing { signature: 0, window: Vec::new(), configured: Vec::new(), entries: Vec::new() };
        let catalog = Catalog { sources, shared, listing: Mutex::new(listing) };
        catalog.refresh(true);
        catalog
    }

    /// The databases, the list read again first if its sources changed.
    pub fn entries(&self) -> Vec<Arc<Entry>> {
        self.refresh(false);
        lock(&self.listing).entries.clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Entry>> {
        lock(&self.listing).entries.iter().find(|e| e.id == id).cloned()
    }

    /// Reads the list again when its sources' signature changed, or `always`.
    fn refresh(&self, always: bool) {
        let mut listing = lock(&self.listing);
        let signature = self.sources.signature(&listing.configured);
        if !always && signature == listing.signature {
            return;
        }
        match self.sources.window() {
            Ok(window) => listing.window = window,
            Err(e) => eprintln!("oschess-bridge: the database window list is unreadable, keeping the last one: {e}"),
        }
        match self.sources.configured() {
            Ok(configured) => listing.configured = configured,
            Err(e) => eprintln!("oschess-bridge: keeping the last databases of bridge.toml: {e}"),
        }
        // Taken after reading, so that a change during the read shows next time.
        listing.signature = self.sources.signature(&listing.configured);
        let mut listed = listing.window.clone();
        listed.extend(listing.configured.iter().flat_map(|p| sources::expand(p)));
        listed.extend(self.sources.fixed.iter().cloned().map(Listed::at));
        listing.entries = self.merge(&listing.entries, listed);
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
                Some(e) => Arc::new(Entry::new(item, &self.shared, Arc::clone(&e.fetch))),
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
