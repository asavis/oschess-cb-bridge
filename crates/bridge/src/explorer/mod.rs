//! The position index behind `GET /v1/databases/{id}/explorer`
//! (`docs/api.md`): for each position reached in the first plies of a
//! database's games, the games through it, their results, the moves played
//! from it and its notable games, and the games' main lines as a move stream
//! to find the games that reach a position beyond those plies in. An index is
//! built in the background the first time it is asked for and kept on disk in
//! the bridge's index folder ([`crate::token::index_dir`]); a change to the
//! database rebuilds it. An index kept on disk for the database as it is now
//! answers from the first request, without a build.

mod answer;
mod build;
pub mod deep;
pub mod file;
pub mod format;
mod map;
pub mod positions;
pub mod rendered;
pub mod runs;
pub mod source;
pub mod stream;
mod tree;

pub use answer::{board, deep, ready, rebuilding, render, route, stats, uci, unsupported};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::catalog::{Entry, Opened};
use crate::fetch::Serial;
use crate::indexdir::{self, Unlisted};
use crate::search::SearchError;

use build::Plan;
use file::{Bad, IndexFile};
use format::{MAX_PLY, PRUNE_PLY, Stats};
use runs::{Limits, Progress, Timings, opened};
use source::Source;
use stream::Stream;

/// How long a failed build is reported before the next request tries again.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The index of one database, at the generation it was built for, and the
/// move stream of the same build.
pub struct Loaded {
    pub generation: u64,
    pub base: IndexFile,
    pub stream: Stream,
    /// Where the build's time went, for an index this process built; `None`
    /// for one kept on disk.
    pub built: Option<Timings>,
    /// This index's key in the cache of rendered games, unique in the process.
    id: u64,
}

impl Loaded {
    pub fn new(generation: u64, base: IndexFile, stream: Stream) -> Loaded {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Loaded { generation, base, stream, built: None, id: NEXT.fetch_add(1, Ordering::Relaxed) }
    }

    /// Game `number`'s rating and rendered JSON, from the cache of rendered
    /// games or `make`.
    pub fn game(&self, number: u32, make: impl FnOnce() -> Option<(u16, String)>) -> Option<rendered::Game> {
        let cache = rendered::cache();
        if let Some(hit) = cache.get((self.id, number)) {
            return Some(hit);
        }
        let (elo, json) = make()?;
        let game = (elo, Arc::<str>::from(json));
        cache.put((self.id, number), &game);
        Some(game)
    }

    /// The position `key`.
    pub fn lookup(&self, key: u64) -> Result<Option<Stats>, Bad> {
        self.base.lookup(key)
    }

    /// The last record the index covers.
    pub fn records(&self) -> u32 {
        self.base.header.last_record
    }

    pub fn games(&self) -> u64 {
        self.base.header.games
    }
}

enum State {
    Idle,
    Working(Arc<Progress>),
    Ready(Arc<Loaded>),
    Failed(Instant, String),
}

/// What a request for a database's index finds.
pub enum Lookup {
    Ready(Arc<Loaded>),
    /// Being built: the request is answered `409` with the progress.
    Pending(Arc<Progress>),
    Failed(String),
    /// The search memory has no room for the index kept on disk now; the next
    /// request tries again.
    Busy,
}

/// The indexes of all databases, and the queue that builds them one at a time.
pub struct Registry {
    dir: Mutex<Option<PathBuf>>,
    states: Mutex<HashMap<String, Arc<Mutex<State>>>>,
    queue: Arc<Serial>,
    /// Index files whose database is not on the list, and since when.
    pub(crate) unlisted: Mutex<Unlisted>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry {
            dir: Mutex::default(),
            states: Mutex::default(),
            queue: Arc::new(Serial::labelled("index")),
            unlisted: Mutex::default(),
        }
    }
}

/// What a file in the index folder is, by its name.
enum Kept {
    /// `<id>.idx` or `<id>.moves`: a database's index or its move stream.
    Index,
    /// `<id>.idx.partial` or `<id>.moves.partial`: a build's work, which
    /// only the build running for `<id>` uses; or the `<id>.build` folder,
    /// the work of a build of a version before #147.
    Work,
}

/// Whether an index folder entry named `name` is a database's index, move
/// stream, or a build's work, which the bridge wrote.
pub fn is_index_file(name: &str) -> bool {
    index_entry(name).is_some()
}

/// The database id and kind of an index folder entry; `None` for anything
/// the bridge did not write there, which is never touched.
fn index_entry(name: &str) -> Option<(&str, Kept)> {
    let (id, kind) = indexdir::db_id(name, &[".idx", ".moves", ".idx.partial", ".moves.partial", ".build"])?;
    Some((id, if kind < 2 { Kept::Index } else { Kept::Work }))
}

impl Registry {
    /// Keeps index files in `dir`.
    pub fn set_dir(&self, dir: PathBuf) {
        *lock(&self.dir) = Some(dir);
    }

    /// Where index files are kept: the data folder's index folder
    /// ([`crate::token::index_dir`]) unless set.
    pub fn dir(&self) -> Option<PathBuf> {
        lock(&self.dir).clone().or_else(|| crate::token::data_dir().map(|d| crate::token::index_dir(&d)))
    }

    fn state(&self, id: &str) -> Arc<Mutex<State>> {
        Arc::clone(lock(&self.states).entry(id.to_string()).or_insert_with(|| Arc::new(Mutex::new(State::Idle))))
    }

    /// The index of `entry` at the generation of `open`: the one in memory,
    /// else the file kept on disk for that generation, else the build that
    /// makes it, which starts now if none runs.
    pub fn index(&self, entry: Arc<Entry>, open: &Opened) -> Lookup {
        let state = self.state(&entry.id);
        let mut s = lock(&state);
        match &*s {
            State::Ready(l) if l.generation == open.generation => return Lookup::Ready(Arc::clone(l)),
            State::Working(p) => return Lookup::Pending(Arc::clone(p)),
            State::Failed(at, why) if at.elapsed() < RETRY_AFTER_FAILURE => return Lookup::Failed(why.clone()),
            _ => {}
        }
        // An index of a former generation gives its memory back first.
        *s = State::Idle;
        let Some(dir) = self.dir() else { return Lookup::Failed("the bridge has no data folder for indexes".into()) };
        // Opening the file reads its header and block table, a matter of
        // milliseconds, so it is done here rather than behind a build of
        // another database in the queue: only a build is answered `indexing`.
        match current(&paths(&dir, &entry.id).0, open.generation, open.db.records()) {
            Ok(Some(loaded)) => {
                let loaded = Arc::new(loaded);
                *s = State::Ready(Arc::clone(&loaded));
                return Lookup::Ready(loaded);
            }
            Err(_) => return Lookup::Busy,
            Ok(None) => {}
        }
        let progress = Arc::new(Progress::default());
        *s = State::Working(Arc::clone(&progress));
        drop(s);
        let (db, generation, p) = (Arc::clone(&open.db), open.generation, Arc::clone(&progress));
        let job_state = Arc::clone(&state);
        let started = self.queue.submit(Box::new(move || {
            let result = index(&*db, generation, &dir, &entry.id, &p, &Limits::default());
            let still = entry.generation() == Some(generation);
            *lock(&job_state) = match result {
                Ok(loaded) if still => State::Ready(Arc::new(loaded)),
                // The database changed meanwhile: the next request starts afresh.
                Ok(_) => State::Idle,
                Err(failure) => {
                    crate::log!("indexing database {} failed: {}", entry.id, failure.logged());
                    State::Failed(Instant::now(), failure.answered(&dir))
                }
            };
        }));
        if !started {
            *lock(&state) = State::Failed(Instant::now(), "the index thread could not start".into());
        }
        Lookup::Pending(progress)
    }

    /// Removes from the index folder what no database on the list will use
    /// (#60): the index of a database that has been off the list for the
    /// grace ([`indexdir::SWEEP_GRACE`] unless the catalog sets another) or
    /// longer, and a build's work left by a build that no longer runs. The
    /// files of a database being built are never touched, nor anything the
    /// bridge did not write.
    pub fn sweep(&self, listed: &HashSet<String>) {
        let Some(dir) = self.dir() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let now = Instant::now();
        let mut unlisted = lock(&self.unlisted);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some((id, kind)) = name.to_str().and_then(index_entry) else { continue };
            // Held while the files go, so no build of `id` starts meanwhile.
            let state = self.state(id);
            let mut s = lock(&state);
            if matches!(*s, State::Working(_)) {
                continue;
            }
            let path = entry.path();
            match kind {
                Kept::Index if listed.contains(id) => {}
                Kept::Index => {
                    if unlisted.due(id, now) && std::fs::remove_file(&path).is_ok() {
                        *s = State::Idle;
                    } else {
                        unlisted.still(id);
                    }
                }
                Kept::Work if path.is_dir() => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                Kept::Work => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        // A database back on the list, or an index gone, starts afresh.
        unlisted.retain();
    }

    /// Drops the index of `id` after a read found it or its move stream
    /// damaged, and deletes both files, so the next request rebuilds them. A
    /// stream still mapped on Windows stays until the build replaces it; the
    /// index alone is gone, and without it the stream is never used.
    pub fn forget(&self, id: &str) {
        let state = self.state(id);
        let mut s = lock(&state);
        if let State::Ready(l) = &*s {
            let _ = std::fs::remove_file(&l.base.path);
            let _ = std::fs::remove_file(&l.stream.path);
        }
        *s = State::Idle;
    }

    /// Drops every index held in memory and leaves its files as they are: the
    /// next request opens them again. A build running goes on. Tests release a
    /// bridge's indexes before they change or remove the files, which a
    /// mapped move stream keeps from being replaced or removed on Windows.
    pub fn release(&self) {
        let states: Vec<Arc<Mutex<State>>> = lock(&self.states).values().cloned().collect();
        for state in states {
            let mut s = lock(&state);
            if matches!(*s, State::Ready(_)) {
                *s = State::Idle;
            }
        }
    }

    /// Where the time of the build of database `id`'s index went, when this
    /// process built the index it holds (`cbtool profile`).
    pub fn timings(&self, id: &str) -> Option<Timings> {
        let state = lock(&self.states).get(id).map(Arc::clone)?;
        match &*lock(&state) {
            State::Ready(l) => l.built.clone(),
            _ => None,
        }
    }

    /// The checks and builds running or waiting: database id, phase, done
    /// and total.
    pub fn building(&self) -> Vec<(String, &'static str, u64, u64)> {
        let states: Vec<(String, Arc<Mutex<State>>)> =
            lock(&self.states).iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();
        let mut out: Vec<_> = states
            .into_iter()
            .filter_map(|(id, s)| match &*lock(&s) {
                State::Working(p) => {
                    Some((id, p.phase(), p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed)))
                }
                _ => None,
            })
            .collect();
        out.sort();
        out
    }
}

/// The index file of database `id` in `dir`, and its move stream beside it
/// ([`stream::path_of`]).
pub fn paths(dir: &Path, id: &str) -> (PathBuf, PathBuf) {
    let index = dir.join(format!("{id}.idx"));
    let stream = stream::path_of(&index);
    (index, stream)
}

/// The index of `id` for the database at `generation`: the file kept on disk
/// when it was built at that generation, else a full build. Any change to the
/// database changes its generation and so rebuilds its index; an index is
/// never answered for another generation than its own.
pub fn prepare(db: &dyn Source, generation: u64, dir: &Path, id: &str, progress: &Progress) -> Result<Loaded, String> {
    prepare_with(db, generation, dir, id, progress, &Limits::default())
}

/// [`prepare`] within `limits`, which tests and tools set.
pub fn prepare_with(
    db: &dyn Source,
    generation: u64,
    dir: &Path,
    id: &str,
    progress: &Progress,
    limits: &Limits,
) -> Result<Loaded, String> {
    index(db, generation, dir, id, progress, limits).map_err(|failure| failure.answered(dir))
}

/// [`prepare_with`], its failure kept whole for the log.
fn index(
    db: &dyn Source,
    generation: u64,
    dir: &Path,
    id: &str,
    progress: &Progress,
    limits: &Limits,
) -> Result<Loaded, Failure> {
    std::fs::create_dir_all(dir).map_err(Failure::Folder)?;
    let (path, moves) = paths(dir, id);
    let count = db.records();
    progress.start("checking", u64::from(count));
    match current(&path, generation, count) {
        Ok(Some(loaded)) => return Ok(loaded),
        Err(_) => return Err(Failure::Busy),
        Ok(None) => {}
    }
    // The files there can never answer again: they go before the build takes
    // their room, and so does the work of an older version's build. A stream
    // still mapped on Windows stays until the build replaces it.
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&moves);
    let _ = std::fs::remove_dir_all(dir.join(format!("{id}.build")));
    let plan = Plan { first: 1, last: count, generation };
    let header = build::build_with(db, &plan, &path, progress, limits).map_err(Failure::Build)?;
    let file = opened(progress, || IndexFile::open(&path)).map_err(Failure::Open)?;
    let stream = opened(progress, || Stream::open(&moves)).map_err(Failure::Open)?;
    if stream.header.build_id != header.build_id || file.header.build_id != header.build_id {
        return Err(Failure::Open(Bad::Corrupt("another build's files")));
    }
    let mut loaded = Loaded::new(generation, file, stream);
    loaded.built = Some(progress.timings());
    Ok(loaded)
}

/// Why a build failed.
enum Failure {
    /// The index folder could not be made.
    Folder(std::io::Error),
    /// The search memory had no room to check the index on disk.
    Busy,
    Build(SearchError),
    /// The index just built does not open.
    Open(Bad),
}

impl Failure {
    /// What its `503 index_unavailable` says, which names the index folder
    /// `dir` or the file at fault.
    fn answered(&self, dir: &Path) -> String {
        match self {
            Failure::Folder(e) => format!("{}: {e}", dir.display()),
            Failure::Build(SearchError::Read(e)) => e.to_string(),
            _ => self.logged(),
        }
    }

    /// What its log line says, which names no path (#117).
    fn logged(&self) -> String {
        match self {
            Failure::Folder(e) => format!("the index folder: {e}"),
            Failure::Busy => "the search memory is taken by searches; retry".into(),
            Failure::Build(e) => describe(e),
            Failure::Open(e) => format!("{e:?}"),
        }
    }
}

/// The index file at `path` and its move stream when they are the whole
/// index of a database of `records` records at `generation`, built together
/// by this version; `None` when either is absent, damaged, of another
/// generation or version, or of another build than the other, and so both are
/// built afresh. The error is [`Bad::Busy`]: the search memory has no room
/// for their tables now.
fn current(path: &Path, generation: u64, records: u32) -> Result<Option<Loaded>, Bad> {
    let file = match IndexFile::open(path) {
        Ok(file)
            if file.header.generation == generation
                && file.header.max_ply == MAX_PLY
                && file.header.prune_ply == PRUNE_PLY
                && file.header.first_record == 1
                && file.header.last_record == records =>
        {
            file
        }
        Err(Bad::Busy) => return Err(Bad::Busy),
        _ => return Ok(None),
    };
    match Stream::open(&stream::path_of(path)) {
        Ok(stream)
            if stream.header.generation == generation
                && stream.header.build_id == file.header.build_id
                && stream.header.first_record == 1
                && stream.header.last_record == records =>
        {
            Ok(Some(Loaded::new(generation, file, stream)))
        }
        Err(Bad::Busy) => Err(Bad::Busy),
        _ => Ok(None),
    }
}

/// Why a build failed, as its log line says it: a failed read names its file
/// by the extension alone, where the `503 index_unavailable` gives its path.
fn describe(e: &SearchError) -> String {
    match e {
        SearchError::TooLarge => "the search memory budget is too small to build this index".into(),
        SearchError::Busy => "the search memory stayed taken by searches; retry".into(),
        SearchError::Superseded => "the build was stopped".into(),
        SearchError::Read(e) => crate::log::error(e),
        SearchError::Unsupported(q) => q.clone(),
        SearchError::IndexDamaged => "the position index is damaged".into(),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::catalog::Catalog;
    use cbformat::fixture::{Builder, TempDb, quiet};
    use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

    /// A database of three games of 1.e4.
    fn e4s(name: &str) -> TempDb {
        let mut b = Builder::new();
        let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
        for _ in 0..3 {
            b.game(e4);
        }
        b.write(name)
    }

    /// The index kept on disk for the database as it is answers the first
    /// request after the bridge starts at once, even while the queue runs
    /// another database's build, and the file is only read. Released, it is
    /// held by the requests that took it alone.
    #[test]
    fn a_kept_index_answers_at_once_while_the_queue_builds() {
        let db = e4s("explorer-kept");
        let dir = std::env::temp_dir().join(format!("bridge-explorer-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let catalog = Catalog::new([db.dir().join("db.2cbh")]);
        catalog.explorer.set_dir(dir.clone());
        let entry = Arc::clone(&catalog.entries()[0]);
        let Ok(open) = entry.open() else { panic!("the database does not open") };
        // Built by the bridge before it restarted.
        drop(prepare(&*open.db, open.generation, &dir, &entry.id, &Progress::default()).unwrap());
        let (path, _) = paths(&dir, &entry.id);
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Another database's build holds the queue until released.
        let (release, held) = mpsc::channel::<()>();
        assert!(catalog.explorer.queue.submit(Box::new(move || {
            let _ = held.recv();
        })));
        let lookup = catalog.explorer.index(Arc::clone(&entry), &open);
        let building = catalog.explorer.building();
        release.send(()).unwrap();
        let Lookup::Ready(loaded) = lookup else { panic!("the kept index waited for the queue") };
        assert_eq!((loaded.generation, loaded.records(), loaded.games()), (open.generation, 3, 3));
        assert!(building.is_empty(), "no build was reported: {building:?}");
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), written, "the file was rewritten");
        // The next request finds it in memory.
        let Lookup::Ready(again) = catalog.explorer.index(Arc::clone(&entry), &open) else { panic!() };
        assert!(Arc::ptr_eq(&loaded, &again));
        // The catalog holds it too, its stream mapped, until released.
        catalog.explorer.release();
        assert_eq!(Arc::strong_count(&loaded), 2, "held by the two requests alone");
        drop((loaded, again));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The index folder is swept of what the list no longer uses (#60): the
    /// index and move stream of a database off the list once the grace has
    /// passed, and a build's leftovers at once. A listed database keeps its
    /// index even while it is missing; a database being built keeps
    /// everything; files the bridge did not write stay.
    #[test]
    fn the_index_folder_keeps_only_what_the_list_uses() {
        let dir = std::env::temp_dir().join(format!("bridge-explorer-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = PathBuf::from("/no/such/folder/Mega Database.2cbh");
        let catalog = Catalog::new([missing.clone()]);
        catalog.explorer.set_dir(dir.clone());
        let listed = crate::catalog::id_of(&missing);
        let (gone, building) = ("0123456789abcdef", "fedcba9876543210");
        let touch = |name: &str| std::fs::write(dir.join(name), b"x").unwrap();
        for name in [
            format!("{listed}.idx"),
            format!("{listed}.moves"),
            format!("{listed}.idx.partial"),
            format!("{listed}.moves.partial"),
            format!("{gone}.idx"),
            format!("{gone}.moves"),
            format!("{gone}.idx.partial"),
            format!("{gone}.moves.partial"),
            format!("{building}.idx"),
            format!("{building}.moves"),
            format!("{building}.idx.partial"),
            format!("{building}.moves.partial"),
            "notes.txt".into(),
            format!("{listed}.moves.old"),
            // Upper case, and no test id in any case: on a file system that
            // ignores case, as Windows's does, it must not name another file.
            "ABCDEF0123456789.idx".into(),
            "short.idx".into(),
        ] {
            touch(&name);
        }
        for id in [gone, building, listed.as_str()] {
            std::fs::create_dir_all(dir.join(format!("{id}.build")).join("runs")).unwrap();
            std::fs::write(dir.join(format!("{id}.build")).join("runs").join("0"), b"run").unwrap();
        }
        *lock(&catalog.explorer.state(building)) = State::Working(Arc::new(Progress::default()));
        let names = || {
            let mut n: Vec<String> =
                std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            n.sort();
            n
        };
        let mut keep = vec![
            format!("{listed}.idx"),
            format!("{listed}.moves"),
            format!("{listed}.moves.old"),
            format!("{gone}.idx"),
            format!("{gone}.moves"),
            format!("{building}.build"),
            format!("{building}.idx"),
            format!("{building}.moves"),
            format!("{building}.idx.partial"),
            format!("{building}.moves.partial"),
            "ABCDEF0123456789.idx".into(),
            "notes.txt".into(),
            "short.idx".into(),
        ];
        keep.sort();

        // Within the grace, only the builds' leftovers go.
        catalog.sweep_indexes();
        assert_eq!(names(), keep);

        // After it, the index and stream of the database off the list go too.
        catalog.set_sweep_grace(Duration::ZERO);
        catalog.sweep_indexes();
        keep.retain(|n| n != &format!("{gone}.idx") && n != &format!("{gone}.moves"));
        assert_eq!(names(), keep);

        // The build that ran ends: its leftovers go; its database, off the
        // list, keeps its index until the grace has passed since now.
        *lock(&catalog.explorer.state(building)) = State::Idle;
        catalog.set_sweep_grace(indexdir::SWEEP_GRACE);
        catalog.sweep_indexes();
        keep.retain(|n| {
            !n.starts_with(&format!("{building}.build"))
                && n != &format!("{building}.idx.partial")
                && n != &format!("{building}.moves.partial")
        });
        assert_eq!(names(), keep);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
