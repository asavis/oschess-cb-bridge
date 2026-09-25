//! The position index behind `GET /v1/databases/{id}/explorer`
//! (`docs/api.md`): for each position reached in the first plies of a
//! database's games, the games through it, their results, the moves played
//! from it and its notable games. An index is built in the background the
//! first time it is asked for and kept on disk in the bridge's data folder;
//! a change to the database rebuilds it. An index kept on disk for the
//! database as it is now answers from the first request, without a build.

mod answer;
mod build;
pub mod file;
pub mod format;
pub mod rendered;
pub mod runs;
pub mod source;

pub use answer::{render, route};
pub use build::WRITER_BYTES;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::catalog::{Entry, Opened};
use crate::fetch::Serial;
use crate::search::SearchError;

use build::Plan;
use file::{Bad, IndexFile};
use format::{MAX_PLY, PRUNE_PLY, Stats};
use runs::{Limits, Progress};
use source::Source;

/// How long a failed build is reported before the next request tries again.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The index of one database, at the generation it was built for.
pub struct Loaded {
    pub generation: u64,
    pub base: IndexFile,
    /// This index's key in the cache of rendered games, unique in the process.
    id: u64,
}

impl Loaded {
    pub fn new(generation: u64, base: IndexFile) -> Loaded {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Loaded { generation, base, id: NEXT.fetch_add(1, Ordering::Relaxed) }
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
}

impl Default for Registry {
    fn default() -> Registry {
        Registry { dir: Mutex::default(), states: Mutex::default(), queue: Arc::new(Serial::labelled("index")) }
    }
}

impl Registry {
    /// Keeps index files in `dir` (the bridge's data folder's `index`).
    pub fn set_dir(&self, dir: PathBuf) {
        *lock(&self.dir) = Some(dir);
    }

    /// Where index files are kept; the data folder's `index` unless set.
    pub fn dir(&self) -> Option<PathBuf> {
        lock(&self.dir).clone().or_else(|| crate::token::data_dir().map(|d| d.join("index")))
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
            Ok(Some(file)) => {
                let loaded = Arc::new(Loaded::new(open.generation, file));
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
            let result = prepare(&*db, generation, &dir, &entry.id, &p);
            let still = entry.generation() == Some(generation);
            *lock(&job_state) = match result {
                Ok(loaded) if still => State::Ready(Arc::new(loaded)),
                // The database changed meanwhile: the next request starts afresh.
                Ok(_) => State::Idle,
                Err(why) => {
                    eprintln!("oschess-bridge: indexing {} failed: {why}", entry.name);
                    State::Failed(Instant::now(), why)
                }
            };
        }));
        if !started {
            *lock(&state) = State::Failed(Instant::now(), "the index thread could not start".into());
        }
        Lookup::Pending(progress)
    }

    /// Drops the index of `id` after a read found it damaged, and deletes its
    /// file, so the next request rebuilds it.
    pub fn forget(&self, id: &str) {
        let state = self.state(id);
        let mut s = lock(&state);
        if let State::Ready(l) = &*s {
            let _ = std::fs::remove_file(&l.base.path);
        }
        *s = State::Idle;
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

/// The index file of database `id` in `dir`, and the folder its build uses.
pub fn paths(dir: &Path, id: &str) -> (PathBuf, PathBuf) {
    (dir.join(format!("{id}.idx")), dir.join(format!("{id}.build")))
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
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (path, work) = paths(dir, id);
    let count = db.records();
    progress.start("checking", u64::from(count));
    match current(&path, generation, count) {
        Ok(Some(file)) => return Ok(Loaded::new(generation, file)),
        Err(_) => return Err("the search memory is taken by searches; retry".into()),
        Ok(None) => {}
    }
    let plan = Plan { first: 1, last: count, prune_ply: PRUNE_PLY, generation };
    build::build_with(db, &plan, &work, &path, progress, limits).map_err(describe)?;
    let file = IndexFile::open(&path).map_err(|e| format!("{e:?}"))?;
    Ok(Loaded::new(generation, file))
}

/// The index file at `path` when it is the whole index of a database of
/// `records` records at `generation`, built by this version; `None` when it is
/// absent, damaged, or of another generation or version, and so is built
/// afresh. The error is [`Bad::Busy`]: the search memory has no room for its
/// table now.
fn current(path: &Path, generation: u64, records: u32) -> Result<Option<IndexFile>, Bad> {
    match IndexFile::open(path) {
        Ok(file)
            if file.header.generation == generation
                && file.header.max_ply == MAX_PLY
                && file.header.prune_ply == PRUNE_PLY
                && file.header.first_record == 1
                && file.header.last_record == records =>
        {
            Ok(Some(file))
        }
        Err(Bad::Busy) => Err(Bad::Busy),
        _ => Ok(None),
    }
}

/// Why a build failed, for its log line and its `503 index_unavailable`.
fn describe(e: SearchError) -> String {
    match e {
        SearchError::TooLarge => "the search memory budget is too small to build this index".into(),
        SearchError::Busy => "the search memory stayed taken by searches; retry".into(),
        SearchError::Superseded => "the build was stopped".into(),
        SearchError::Read(e) => e.to_string(),
        SearchError::Unsupported(q) => q,
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
    /// another database's build, and the file is only read.
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
        drop((loaded, again));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
