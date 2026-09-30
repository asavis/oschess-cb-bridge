//! The databases the bridge serves: identity, state, and the open handle,
//! reopened whenever the files change (`docs/api.md`, "Database identity and
//! generations"). The list follows its sources (`crate::sources`) and is read
//! again when they change.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cbformat::view::Base;

use crate::fetch::{Cloud, Progress, System};
use crate::pgnindex::{self, Opening};
use crate::search::Indexes;
use crate::search::heads::{self, Lookup};
use crate::serial::Serial;
use crate::sources::{Listed, Read, Sources};
use crate::sync::lock;

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

    /// The format as `cbformat` names it; `None` for another file.
    fn view(self) -> Option<cbformat::view::Format> {
        match self {
            Format::TwoCbh => Some(cbformat::view::Format::TwoCbh),
            Format::Cbh => Some(cbformat::view::Format::Cbh),
            Format::Pgn => Some(cbformat::view::Format::Pgn),
            Format::Other => None,
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
/// renames it: whether it is on the list, the open handle and the download.
/// A job holding the entry of a database since renamed sees its removal too.
#[derive(Default)]
struct Held {
    /// Set when the database left the list: it is then reported `missing`.
    removed: AtomicBool,
    open: Mutex<Option<Opened>>,
    /// The download running or queued.
    running: Mutex<Option<Arc<Progress>>>,
    /// The last download read every file, yet a file kept its cloud-only mark.
    /// Only tests read it, through [`Entry::marks_kept`].
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
    /// When the file changed last that changed last, as its modification
    /// time says.
    modified: Option<SystemTime>,
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
            held,
            shared: Arc::clone(shared),
        }
    }

    /// Whether the database is on the list; one that left it stays `missing`
    /// until the bridge restarts.
    pub fn listed(&self) -> bool {
        !self.held.removed.load(Ordering::Relaxed)
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
        self.open_sized().0
    }

    /// [`Entry::open`], and the bytes of the database's files from the same
    /// look at their metadata: `None` when the files were not looked at, for
    /// a database that left the list or is of another format (#67).
    pub fn open_sized(&self) -> (Result<Opened, State>, Option<u64>) {
        if self.held.removed.load(Ordering::Relaxed) {
            return (Err(State::Missing), None);
        }
        if self.format == Format::Other {
            return (Err(if self.path.exists() { State::Unsupported } else { State::Missing }), None);
        }
        let files = self.files();
        (self.open_files(&files), Some(files.size()))
    }

    /// [`Entry::open`], and when its files last changed, by their
    /// modification times, from the same look at their metadata: what the
    /// keeper of the position indexes asks (#149). `None` when that is not
    /// known.
    pub fn open_dated(&self) -> (Result<Opened, State>, Option<SystemTime>) {
        if self.held.removed.load(Ordering::Relaxed) || self.format == Format::Other {
            return (self.open(), None);
        }
        let files = self.files();
        (self.open_files(&files), files.modified)
    }

    fn open_files(&self, files: &Files) -> Result<Opened, State> {
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
    /// file marked cloud-only, so that the database stayed cloud-only. For
    /// tests: the bridge reports such a database `cloudOnly` as any other,
    /// and its log says why.
    #[doc(hidden)]
    pub fn marks_kept(&self) -> bool {
        self.held.kept.load(Ordering::Relaxed)
    }

    /// The download running or queued, if any.
    pub fn progress(&self) -> Option<Arc<Progress>> {
        lock(&self.held.running).clone()
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
        let (held, cloud, path, id, format) =
            (Arc::clone(&self.held), Arc::clone(&self.shared.cloud), self.path.clone(), self.id.clone(), self.format);
        // Ends the download however the job ends, and also when it is
        // dropped unrun because no thread could start.
        let done = Done(Arc::clone(&held));
        self.shared.downloads.submit(Box::new(move || {
            let _done = done;
            if let Err(e) = cloud_files.iter().try_for_each(|file| cloud.fetch(file, &mut |n| progress.add(n))) {
                crate::log!("downloading database {id} failed: {e}");
                return;
            }
            let after = generation_of(&path, format, &*cloud);
            let kept: Vec<&PathBuf> =
                after.present.iter().filter(|f| f.2 && cloud_files.contains(&f.0)).map(|f| &f.0).collect();
            if !kept.is_empty() {
                held.kept.store(true, Ordering::Relaxed);
                crate::log!(
                    "downloaded database {id}, but {} of its files still show as kept in the cloud",
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

    /// Reads the database's generation when called, from any thread, without
    /// holding the entry: what a background job asks between its steps.
    pub fn generation_probe(&self) -> impl Fn() -> Option<u64> + Send + 'static {
        let (path, format, cloud) = (self.path.clone(), self.format, Arc::clone(&self.shared.cloud));
        move || generation_of(&path, format, &*cloud).generation
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

/// The metadata of the database at `path`, of `format`: its generation and
/// files, the ones its reader opens ([`cbformat::view::Format::files`]), so
/// that the search boosters and other optional classic files are neither
/// read nor downloaded. Metadata only, following links: nothing is opened. A
/// PGN database is its one file; another file has no generation.
fn generation_of(path: &Path, format: Format, cloud: &dyn Cloud) -> Files {
    let mut hash = Hash::new();
    let mut files = Files { generation: None, present: Vec::new(), irregular: false, modified: None };
    let Some(format) = format.view() else { return files };
    // Every cache keyed on a PGN database's generation (the header index, the
    // heads and names files, the position index) is then built again once
    // when the reading of PGN files changes; other formats' caches stay.
    if format == cbformat::view::Format::Pgn {
        hash.write(&cbformat::pgnfile::VERSION.to_le_bytes());
    }
    for (i, (path, _)) in format.files(path).into_iter().enumerate() {
        match std::fs::metadata(&path) {
            Ok(m) if !m.is_file() => {
                files.irregular = true;
                hash.write(&[0xfe]);
            }
            Ok(m) => {
                hash.write_meta(&m);
                files.modified = files.modified.max(m.modified().ok());
                let cloud_only = cloud.is_cloud_only(&path, &m);
                files.present.push((path, m.len(), cloud_only));
            }
            // The main file comes first: without it there is no database.
            Err(_) if i == 0 => return files,
            Err(_) => hash.write(&[0xff]),
        }
    }
    files.generation = Some(hash.finish());
    files
}

pub struct Catalog {
    /// The position indexes of the databases, and the queue that builds them.
    pub explorer: crate::explorer::Registry,
    /// The heads files of the databases (#106), and the queue that builds them.
    pub heads: Arc<heads::Registry>,
    heads_queue: Arc<Serial>,
    sources: Sources,
    shared: Arc<Shared>,
    /// The sources as last read. Held while they are read again and the list
    /// is rebuilt from them, so that one refresh runs at a time; a request for
    /// one database never waits on it, however long a source takes to read.
    read: Mutex<Read>,
    /// The listed databases in order, then those that left the list. A
    /// rebuild replaces the whole list, so this lock is held only to take a
    /// handle to it.
    entries: Mutex<Arc<Vec<Arc<Entry>>>>,
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

impl Catalog {
    /// The databases at `paths`, in order, each once.
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Catalog {
        Catalog::with_sources(Sources { fixed: paths.into_iter().collect(), ..Sources::default() }, Arc::new(System))
    }

    /// The databases of `sources`, read now and again whenever they change.
    pub fn with_sources(sources: Sources, cloud: Arc<dyn Cloud>) -> Catalog {
        let downloads = Arc::new(Serial::labelled("download"));
        let shared = Arc::new(Shared { cloud, downloads, pgn: pgnindex::Registry::default() });
        let catalog = Catalog {
            explorer: crate::explorer::Registry::default(),
            heads: Arc::default(),
            heads_queue: Arc::new(Serial::labelled("heads")),
            sources,
            shared,
            read: Mutex::default(),
            entries: Mutex::default(),
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

    /// Keeps the indexes where the bridge whose data folder is `dir` keeps
    /// them ([`crate::folders`]): the position indexes, with the heads and
    /// names files, in its index folder, apart from the data folder where
    /// that roams (#147), and the header indexes of PGN files in its `pgn`
    /// folder. A start sets its data folder here too, so a tool or a test
    /// that gives its own keeps its indexes where a start would (#175).
    pub fn use_data_dir(&self, dir: &Path) {
        self.explorer.set_dir(crate::folders::index_dir(dir));
        self.shared.pgn.set_dir(crate::folders::pgn_dir(dir));
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
        let entries = self.listed();
        self.sweep_if_due(&entries, rebuilt);
        entries.to_vec()
    }

    /// The list as last rebuilt.
    fn listed(&self) -> Arc<Vec<Arc<Entry>>> {
        Arc::clone(&lock(&self.entries))
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

    /// Sets how long the files of a database that left the list stay in the
    /// index folders (#60): its position index, heads and names files, and
    /// the header index of its PGN file; [`crate::indexdir::SWEEP_GRACE`]
    /// unless set.
    pub fn set_sweep_grace(&self, grace: Duration) {
        lock(&self.explorer.unlisted).set_grace(grace);
        lock(&self.shared.pgn.unlisted).set_grace(grace);
        lock(&self.heads.unlisted).set_grace(grace);
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
        if let Some(dir) = self.explorer.dir() {
            self.heads.sweep(&dir, &listed);
        }
    }

    /// Hands `open` the heads file of its generation when one is ready, and
    /// starts the build of one when none is (#106). Until it is ready, passes
    /// over the database read its own records.
    pub fn attach_heads(&self, entry: &Entry, open: &Opened) {
        // A file that failed its CRC is left for the registry, which drops it
        // and builds it again.
        if open.indexes.has_usable_heads() {
            return;
        }
        let Some(dir) = self.explorer.dir() else { return };
        match self.heads.lookup(&dir, &entry.id, open.generation, open.db.record_count()) {
            Lookup::Ready(h) => open.indexes.set_heads(h),
            Lookup::None => {}
            Lookup::Build => {
                let (registry, id, generation) = (Arc::clone(&self.heads), entry.id.clone(), open.generation);
                let (db, indexes, probe) = (Arc::clone(&open.db), Arc::clone(&open.indexes), entry.generation_probe());
                let path = heads::path(&dir, &entry.id);
                let started = self.heads_queue.submit(Box::new(move || {
                    let _bug = registry.unwinding(&id, generation);
                    let still = || probe() == Some(generation);
                    let result = heads::build_base(&db, generation, &path, &still);
                    if let Some(h) = registry.built(&id, generation, result) {
                        indexes.set_heads(h);
                    }
                }));
                if !started {
                    self.heads.built(&entry.id, open.generation, Err("the heads thread could not start".into()));
                }
            }
        }
    }

    /// The database `id` of the list as last rebuilt, without reading the
    /// sources again: a request for one database never waits on a source
    /// that is slow to read, such as a folder on a network drive that no
    /// longer answers.
    pub fn get(&self, id: &str) -> Option<Arc<Entry>> {
        self.listed().iter().find(|e| e.id == id).cloned()
    }

    /// Reads again the sources that changed or failed last time
    /// ([`Read`]), and rebuilds the list when any was read, or `always`;
    /// whether it did. One refresh runs at a time; the rebuilt list replaces
    /// the one [`Catalog::get`] reads.
    fn refresh(&self, always: bool) -> bool {
        let mut read = lock(&self.read);
        let changed = read.update(&self.sources);
        if let Some(hook) = lock(&self.after_read).as_ref() {
            hook();
        }
        if changed || always {
            // Only a refresh replaces the list, and refreshes run one at a
            // time: the list it starts from is still the last one.
            let entries = self.merge(&self.listed(), read.listed(&self.sources));
            *lock(&self.entries) = Arc::new(entries);
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
            entry.held.removed.store(false, Ordering::Relaxed);
            entries.push(entry);
        }
        for e in old.iter().filter(|e| !seen.contains(&e.id)) {
            e.held.removed.store(true, Ordering::Relaxed);
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

    #[test]
    fn a_pgn_generation_carries_the_index_version() {
        let f = cbformat::fixture::pgn_file("catalog-generation", b"[White \"A\"]\n\n1. e4 *\n");
        let path = f.dir().join("db.pgn");
        let generation = generation_of(&path, Format::Pgn, &System).generation.unwrap();
        // The version salt is what rebuilds the caches of an unchanged PGN
        // file once its reading changes: without it the generation is the
        // file's metadata alone.
        let mut metadata = Hash::new();
        metadata.write_meta(&std::fs::metadata(&path).unwrap());
        assert_ne!(generation, metadata.finish());
        let mut salted = Hash::new();
        salted.write(&cbformat::pgnfile::VERSION.to_le_bytes());
        salted.write_meta(&std::fs::metadata(&path).unwrap());
        assert_eq!(generation, salted.finish());
    }
}
