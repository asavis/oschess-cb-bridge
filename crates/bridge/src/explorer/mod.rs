//! The position index behind `GET /v1/databases/{id}/explorer`
//! (`docs/api.md`): for each position reached in the first plies of a
//! database's games, the games through it, their results, the moves played
//! from it and its notable games. An index is built in the background the
//! first time it is asked for and kept on disk in the bridge's data folder;
//! a change to the database rebuilds it.

mod answer;
mod build;
pub mod file;
pub mod format;
pub mod runs;
pub mod source;

pub use answer::{render, route};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::catalog::{Entry, Opened};
use crate::fetch::Serial;

use build::Plan;
use file::{Bad, IndexFile};
use format::{MAX_PLY, PRUNE_PLY, Stats};
use runs::Progress;
use source::Source;

/// How long a failed build is reported before the next request tries again.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The index of one database, at the generation it was built for.
pub struct Loaded {
    pub generation: u64,
    pub base: IndexFile,
    /// Notable games already rendered, by number: their rating and JSON.
    games: Mutex<HashMap<u32, (u16, Arc<str>)>>,
}

/// Notable games kept rendered: some 200 bytes each.
const RENDERED_GAMES: usize = 1 << 16;

impl Loaded {
    pub fn new(generation: u64, base: IndexFile) -> Loaded {
        Loaded { generation, base, games: Mutex::default() }
    }

    /// Game `number`'s rating and rendered JSON, from the cache or `make`.
    pub fn game(&self, number: u32, make: impl FnOnce() -> Option<(u16, String)>) -> Option<(u16, Arc<str>)> {
        if let Some(hit) = lock(&self.games).get(&number) {
            return Some(hit.clone());
        }
        let (elo, json) = make()?;
        let value = (elo, Arc::<str>::from(json));
        let mut games = lock(&self.games);
        if games.len() >= RENDERED_GAMES {
            games.clear();
        }
        games.insert(number, value.clone());
        Some(value)
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
    /// Being checked or built: the request is answered `409` with the progress.
    Pending(Arc<Progress>),
    Failed(String),
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

    /// The index of `entry` at the generation of `open`, or the check or
    /// build that makes it, which starts now if none runs.
    pub fn index(&self, entry: Arc<Entry>, open: &Opened) -> Lookup {
        let state = self.state(&entry.id);
        let mut s = lock(&state);
        match &*s {
            State::Ready(l) if l.generation == open.generation => return Lookup::Ready(Arc::clone(l)),
            State::Working(p) => return Lookup::Pending(Arc::clone(p)),
            State::Failed(at, why) if at.elapsed() < RETRY_AFTER_FAILURE => return Lookup::Failed(why.clone()),
            _ => {}
        }
        let Some(dir) = self.dir() else { return Lookup::Failed("the bridge has no data folder for indexes".into()) };
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
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (path, work) = paths(dir, id);
    let count = db.records();
    progress.start("checking", u64::from(count));
    match IndexFile::open(&path) {
        Ok(file)
            if file.header.generation == generation
                && file.header.max_ply == MAX_PLY
                && file.header.prune_ply == PRUNE_PLY
                && file.header.first_record == 1
                && file.header.last_record == count =>
        {
            return Ok(Loaded::new(generation, file));
        }
        Err(Bad::Busy) => return Err("the search memory is taken by searches; retry".into()),
        // Absent, damaged, or of another generation: built afresh.
        _ => {}
    }
    let plan = Plan { first: 1, last: count, prune_ply: PRUNE_PLY, generation };
    build::build(db, &plan, &work, &path, progress).map_err(|e| format!("{e:?}"))?;
    let file = IndexFile::open(&path).map_err(|e| format!("{e:?}"))?;
    Ok(Loaded::new(generation, file))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
