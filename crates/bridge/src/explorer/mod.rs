//! The position index behind `GET /v1/databases/{id}/explorer`
//! (`docs/api.md`): for each position reached in the first plies of a
//! database's games, the games through it, their results, the moves played
//! from it and its notable games, and the games' main lines as a move stream
//! to find the games that reach a position beyond those plies in. An index is
//! built the first time it is asked for, and kept on disk in the bridge's
//! index folder ([`crate::folders::index_dir`]); a change to the database
//! rebuilds it, at the next request, or in the background for a database in
//! use ([`keeper`]). An index kept on disk for the database as it is now
//! answers from the first request, without a build.

mod answer;
mod build;
pub mod deep;
pub mod file;
pub mod format;
pub mod keeper;
mod map;
pub mod positions;
pub mod rendered;
pub mod runs;
pub mod schedule;
pub mod source;
pub mod stream;
mod tree;

pub use answer::{board, deep, ready, rebuilding, render, route, stats, uci, unsupported};

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::catalog::{Entry, Opened};
use crate::indexdir::{self, Unlisted};
use crate::machine::Machine;
use crate::search::SearchError;

use build::Plan;
use file::{Bad, IndexFile};
use format::{MAX_PLY, PRUNE_PLY, Stats};
use runs::{Limits, Progress, Timings, opened};
use schedule::{Kind, Ran, Scheduler};
use source::Source;
use stream::Stream;

/// How long a failed build is reported before the next request tries again.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The free space a build needs on the disk of the index folder, beside the
/// files of the database's former index, which it deletes first: 400 bytes a
/// record for the index and the move stream it writes, which take about 330,
/// and 256 MiB more (#149). About 5 GB for the Mega Database.
pub fn space_needed(records: u32) -> u64 {
    400 * u64::from(records) + (256 << 20)
}

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
    /// Waiting for its turn, or being built.
    Working(Arc<Progress>),
    Ready(Arc<Loaded>),
    /// The build of the database at `generation` failed at `at`, for `why`:
    /// requests are answered so for a minute, and no background build is
    /// tried again until the database changes.
    Failed {
        at: Instant,
        why: String,
        generation: u64,
    },
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

/// The indexes of all databases, the queues that build them one at a time
/// ([`schedule`]), and what the keeper of the databases in use knows
/// ([`keeper`]).
pub struct Registry {
    dir: Mutex<Option<PathBuf>>,
    states: Mutex<HashMap<String, Arc<Mutex<State>>>>,
    builds: Arc<Scheduler>,
    /// Index files whose database is not on the list, and since when.
    pub(crate) unlisted: Mutex<Unlisted>,
    /// The databases whose explorer or games of a position were asked for
    /// since the bridge started, and the one the keeper picked at its start.
    used: Mutex<HashSet<String>>,
    /// Whether the keeper has picked the database in use from the start.
    picked: AtomicBool,
    /// Each database's generation as the keeper first saw it, and since when
    /// it has had it.
    seen: Mutex<HashMap<String, keeper::Seen>>,
    keeping: Mutex<keeper::Keeping>,
    /// What builds may use; `None` for [`Limits::default`]. Tests set it.
    limits: Mutex<Option<Limits>>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry {
            dir: Mutex::default(),
            states: Mutex::default(),
            builds: Arc::default(),
            unlisted: Mutex::default(),
            used: Mutex::default(),
            picked: AtomicBool::new(false),
            seen: Mutex::default(),
            keeping: Mutex::default(),
            limits: Mutex::default(),
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
    /// ([`crate::folders::index_dir`]) unless set.
    pub fn dir(&self) -> Option<PathBuf> {
        lock(&self.dir).clone().or_else(|| crate::folders::data_dir().map(|d| crate::folders::index_dir(&d)))
    }

    fn state(&self, id: &str) -> Arc<Mutex<State>> {
        Arc::clone(lock(&self.states).entry(id.to_string()).or_insert_with(|| Arc::new(Mutex::new(State::Idle))))
    }

    /// Marks database `id` in use (#149): its explorer or the games of one
    /// of its positions were asked for. The keeper then rebuilds its index
    /// when it changes.
    pub fn mark_in_use(&self, id: &str) {
        let mut used = lock(&self.used);
        if !used.contains(id) {
            used.insert(id.to_string());
        }
    }

    /// Whether database `id` is marked in use ([`Registry::mark_in_use`]),
    /// for the tests of the requests that mark it.
    #[cfg(test)]
    pub(crate) fn in_use(&self, id: &str) -> bool {
        lock(&self.used).contains(id)
    }

    /// Sets how builds see the computer: its power, and the free space of
    /// the index folder's disk. Tests stand in their own.
    pub fn set_machine(&self, machine: Arc<dyn Machine>) {
        self.builds.set_machine(machine);
    }

    /// Sets how long the threads of a background build give way to
    /// foreground work at most, at a time, from the next build on:
    /// [`schedule::PATIENCE`] unless set. Tests set it.
    pub fn set_patience(&self, patience: Duration) {
        self.builds.set_patience(patience);
    }

    /// Sets what builds may use from now on, as [`prepare_with`] takes it.
    /// Tests make builds of few games take many passes with it.
    pub fn set_limits(&self, limits: Limits) {
        *lock(&self.limits) = Some(limits);
    }

    /// The index of `entry` at the generation of `open`: the one in memory,
    /// else the file kept on disk for that generation, else the build that
    /// makes it, which is queued now, before the background builds, if none
    /// runs or waits. A background build of the database waiting or running
    /// becomes the build the request waits for. The database is in use from
    /// now on.
    pub fn index(&self, entry: Arc<Entry>, open: &Opened) -> Lookup {
        self.mark_in_use(&entry.id);
        let state = self.state(&entry.id);
        let mut s = lock(&state);
        match &*s {
            State::Ready(l) if l.generation == open.generation => return Lookup::Ready(Arc::clone(l)),
            State::Working(p) => {
                self.builds.promote(&entry.id);
                return Lookup::Pending(Arc::clone(p));
            }
            State::Failed { at, why, .. } if at.elapsed() < RETRY_AFTER_FAILURE => return Lookup::Failed(why.clone()),
            _ => {}
        }
        // An index of a former generation gives its memory back first.
        *s = State::Idle;
        let Some(dir) = self.dir() else { return Lookup::Failed("the bridge has no data folder for indexes".into()) };
        // Opening the file reads its header and block table, a matter of
        // milliseconds, so it is done here rather than behind a build of
        // another database in the queue: only a build is answered `indexing`.
        let records = open.db.records();
        match current(&paths(&dir, &entry.id).0, open.generation, records) {
            Ok(Some(loaded)) => {
                let loaded = Arc::new(loaded);
                *s = State::Ready(Arc::clone(&loaded));
                return Lookup::Ready(loaded);
            }
            Err(_) => return Lookup::Busy,
            Ok(None) => {}
        }
        if let Err(why) = room(&*self.builds.machine(), &dir, &entry.id, records) {
            crate::log!("database {} is not indexed: {why}", entry.id);
            *s = State::Failed { at: Instant::now(), why: why.clone(), generation: open.generation };
            return Lookup::Failed(why);
        }
        Lookup::Pending(self.queue(s, &state, entry, open, dir, Kind::Requested))
    }

    /// Queues the build of the index of `entry` at the generation of `open`
    /// into `dir`, as `kind`; its state, which `s` holds locked, is working
    /// until the build is over. Returns its progress.
    ///
    /// When its turn comes, a build of a database that changed since is
    /// dropped: the next request, or the keeper once the database is quiet,
    /// queues the build of its new generation. So is a background build of a
    /// database that left the list, or that can no longer be read without a
    /// download: gone cloud-only, or downloading, which leave its generation
    /// as it was. A build that would not fit the free space
    /// of the index folder's disk fails, and one stopped for a requested build
    /// of another database, or for battery power, waits for its turn again.
    fn queue(
        &self,
        mut s: MutexGuard<'_, State>,
        state: &Arc<Mutex<State>>,
        entry: Arc<Entry>,
        open: &Opened,
        dir: PathBuf,
        kind: Kind,
    ) -> Arc<Progress> {
        let progress = Arc::new(Progress::default());
        *s = State::Working(Arc::clone(&progress));
        drop(s);
        let (db, generation, records) = (Arc::clone(&open.db), open.generation, open.db.records());
        let (machine, limits) = (self.builds.machine(), *lock(&self.limits));
        let (job_state, p, id) = (Arc::clone(state), Arc::clone(&progress), entry.id.clone());
        let work = move |kind: Kind| {
            let _bug = Unwinding { state: &job_state, generation };
            // Asked at every turn, a stopped build's too. A background build
            // reads the database only while it opens at the build's
            // generation: listed, wholly on this computer and not downloading.
            let current = match kind {
                Kind::Requested => entry.generation() == Some(generation),
                Kind::Background => entry.open().is_ok_and(|now| now.generation == generation),
            };
            if !current {
                *lock(&job_state) = State::Idle;
                return Ran::Done;
            }
            if let Err(why) = room(&*machine, &dir, &entry.id, records) {
                crate::log!("database {} is not indexed: {why}", entry.id);
                *lock(&job_state) = State::Failed { at: Instant::now(), why, generation };
                return Ran::Done;
            }
            let result = index(&*db, generation, &dir, &entry.id, &p, &limits.unwrap_or_default());
            if result.is_err() && p.stop.load(Ordering::Relaxed) {
                return Ran::Stopped;
            }
            let still = entry.generation() == Some(generation);
            *lock(&job_state) = match result {
                Ok(loaded) if still => State::Ready(Arc::new(loaded)),
                // The database changed meanwhile: the next request starts afresh.
                Ok(_) => State::Idle,
                Err(failure) => {
                    crate::log!("indexing database {} failed: {}", entry.id, failure.logged());
                    State::Failed { at: Instant::now(), why: failure.answered(&dir), generation }
                }
            };
            Ran::Done
        };
        if !self.builds.submit(&id, kind, Arc::clone(&progress), Box::new(work)) {
            let why = "the index thread could not start".to_string();
            *lock(state) = State::Failed { at: Instant::now(), why, generation };
        }
        progress
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

/// Fails a build whose work panics, as the work unwinds to the queue that
/// catches it ([`schedule`]), as a build that fails otherwise does (#172):
/// left working, its database would be answered `indexing` for ever, never
/// be built again, and keep a build's files that no sweep removes. A normal
/// return leaves the state to the work.
struct Unwinding<'a> {
    state: &'a Mutex<State>,
    generation: u64,
}

impl Drop for Unwinding<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let (why, generation) = ("the build failed with a bug".to_string(), self.generation);
            *lock(self.state) = State::Failed { at: Instant::now(), why, generation };
        }
    }
}

/// The index file of database `id` in `dir`, and its move stream beside it
/// ([`stream::path_of`]).
pub fn paths(dir: &Path, id: &str) -> (PathBuf, PathBuf) {
    let index = dir.join(format!("{id}.idx"));
    let stream = stream::path_of(&index);
    (index, stream)
}

/// Whether the index folder `dir` has room for a build of the index of
/// database `id`, of `records` records ([`space_needed`]): its disk's free
/// space and the files the build deletes first. Why not otherwise, as a
/// `503 index_unavailable` says it, naming no path. A disk whose free space
/// is not known has room.
fn room(machine: &dyn Machine, dir: &Path, id: &str, records: u32) -> Result<(), String> {
    let Some(free) = machine.free_bytes(dir) else { return Ok(()) };
    let (index, stream) = paths(dir, id);
    let former: u64 = [index, stream].iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    let needed = space_needed(records);
    if free.saturating_add(former) >= needed {
        return Ok(());
    }
    let mb = |bytes: u64| bytes.div_ceil(1_000_000);
    Err(format!(
        "the disk of the index folder has {} MB free, and the index of this database needs {} MB",
        mb(free),
        mb(needed - former)
    ))
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

/// Whether the index file at `path` and its move stream are, by their headers
/// alone, the whole index of a database of `records` records at
/// `generation`, built together by this version, as [`current`] would find
/// them: what the keeper asks without taking memory for their tables. A file
/// damaged beyond its header is found so by the first request, and rebuilt.
fn kept(path: &Path, generation: u64, records: u32) -> bool {
    fn head<const N: usize>(path: &Path) -> Option<[u8; N]> {
        let mut bytes = [0u8; N];
        std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut bytes)).ok()?;
        Some(bytes)
    }
    let index = head::<{ format::HEADER_LEN }>(path).and_then(|b| format::Header::decode(&b));
    let moves = head::<{ stream::HEADER_LEN }>(&stream::path_of(path)).and_then(|b| stream::Header::decode(&b));
    match (index, moves) {
        (Some(i), Some(m)) => {
            i.generation == generation
                && i.max_ply == MAX_PLY
                && i.prune_ply == PRUNE_PLY
                && i.first_record == 1
                && i.last_record == records
                && m.generation == generation
                && m.build_id == i.build_id
                && m.first_record == 1
                && m.last_record == records
        }
        _ => false,
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
        let other = Arc::new(Progress::default());
        assert!(catalog.explorer.builds.submit(
            "0123456789abcdef",
            Kind::Requested,
            other,
            Box::new(move |_| {
                let _ = held.recv();
                Ran::Done
            })
        ));
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

    /// A build whose work panics fails as a build that fails otherwise does
    /// (#172): its database is answered `index_unavailable` for a minute,
    /// not `indexing` for ever, and no longer shows as building; the next
    /// request after that minute starts a new build, which ends ready,
    /// leaving no partial file.
    #[test]
    fn a_build_that_panics_fails_and_is_tried_again() {
        /// A computer whose free space the index thread asks at the start of
        /// each build's work, which panics then while `bug` holds.
        struct Buggy {
            bug: AtomicBool,
        }
        impl Machine for Buggy {
            fn on_battery(&self) -> bool {
                false
            }
            fn free_bytes(&self, _: &Path) -> Option<u64> {
                if self.bug.load(Ordering::Relaxed) && std::thread::current().name() == Some("bridge-index") {
                    panic!("a bug in the build");
                }
                None
            }
        }
        let db = e4s("explorer-bug");
        let dir = std::env::temp_dir().join(format!("bridge-explorer-bug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let catalog = Catalog::new([db.dir().join("db.2cbh")]);
        catalog.explorer.set_dir(dir.clone());
        let machine = Arc::new(Buggy { bug: AtomicBool::new(true) });
        catalog.explorer.set_machine(Arc::clone(&machine) as Arc<dyn Machine>);
        let entry = Arc::clone(&catalog.entries()[0]);
        let Ok(open) = entry.open() else { panic!("the database does not open") };
        let state = catalog.explorer.state(&entry.id);
        let settled = || {
            let until = Instant::now() + Duration::from_secs(30);
            while matches!(*lock(&state), State::Working(_)) {
                assert!(Instant::now() < until, "the build is over");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert!(matches!(catalog.explorer.index(Arc::clone(&entry), &open), Lookup::Pending(_)));
        settled();
        match &*lock(&state) {
            State::Failed { why, generation, .. } => {
                assert_eq!((why.as_str(), *generation), ("the build failed with a bug", open.generation));
            }
            _ => panic!("the build did not fail"),
        }
        let Lookup::Failed(why) = catalog.explorer.index(Arc::clone(&entry), &open) else { panic!("not failed") };
        assert_eq!(why, "the build failed with a bug");
        assert!(catalog.explorer.building().is_empty(), "no build shows");
        // A minute later.
        if let State::Failed { at, .. } = &mut *lock(&state) {
            *at = at.checked_sub(RETRY_AFTER_FAILURE).expect("the clock goes back a minute");
        }
        machine.bug.store(false, Ordering::Relaxed);
        assert!(matches!(catalog.explorer.index(Arc::clone(&entry), &open), Lookup::Pending(_)), "a new build");
        settled();
        let Lookup::Ready(loaded) = catalog.explorer.index(Arc::clone(&entry), &open) else { panic!("not built") };
        assert_eq!((loaded.generation, loaded.records(), loaded.games()), (open.generation, 3, 3));
        let mut names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, [format!("{}.idx", entry.id), format!("{}.moves", entry.id)]);
        drop(loaded);
        catalog.explorer.release();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A build that panics, here in the workers of its stream pass, removes
    /// the files it was writing, as a build that fails otherwise does (#172).
    #[test]
    fn a_build_that_panics_leaves_no_partial_file() {
        /// Three records whose games no worker reads without a panic; the
        /// first to panic notes whether the move stream was being written.
        struct Buggy {
            partial: PathBuf,
            written: AtomicBool,
        }
        impl Source for Buggy {
            fn records(&self) -> u32 {
                3
            }
            fn lines(
                &self,
                _: u32,
                _: u32,
                _: u8,
                _: &mut source::Workspace,
                _: &mut dyn FnMut(&source::Line),
            ) -> cbformat::Result<()> {
                self.written.fetch_or(self.partial.exists(), Ordering::Relaxed);
                panic!("a bug in the reader");
            }
        }
        let dir = std::env::temp_dir().join(format!("bridge-explorer-bug-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Buggy { partial: dir.join("db.moves.partial"), written: AtomicBool::new(false) };
        let built = std::panic::catch_unwind(|| prepare(&db, 7, &dir, "db", &Progress::default()).map(drop));
        assert!(built.is_err(), "the build panicked");
        assert!(db.written.load(Ordering::Relaxed), "the move stream was being written");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "no file is left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What the keeper reads of the index files (#149): their headers alone,
    /// which say whether they are the index of the database as it is; and
    /// the room a build needs, beside the files of the former index.
    #[test]
    fn the_keeper_reads_the_headers_and_the_room() {
        struct Free(Option<u64>);
        impl Machine for Free {
            fn on_battery(&self) -> bool {
                false
            }
            fn free_bytes(&self, _: &Path) -> Option<u64> {
                self.0
            }
        }
        let db = e4s("explorer-kept-headers");
        let dir = std::env::temp_dir().join(format!("bridge-explorer-kept-headers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = cbformat::v2::Database::open(db.dir().join("db.2cbh")).unwrap();
        drop(prepare(&d, 7, &dir, "db", &Progress::default()).unwrap());
        let (index, stream) = paths(&dir, "db");
        assert!(kept(&index, 7, 3));
        assert!(!kept(&index, 8, 3), "another generation");
        assert!(!kept(&index, 7, 4), "other records");
        let former = std::fs::metadata(&index).unwrap().len() + std::fs::metadata(&stream).unwrap().len();
        let needed = space_needed(3);
        assert_eq!(needed, 1_200 + (256 << 20));
        assert!(room(&Free(None), &dir, "db", 3).is_ok(), "unknown free space");
        assert!(room(&Free(Some(needed - former)), &dir, "db", 3).is_ok(), "the former files count");
        let why = room(&Free(Some(needed - former - 1)), &dir, "db", 3).unwrap_err();
        assert!(why.starts_with("the disk of the index folder has 269 MB free"), "{why}");
        std::fs::remove_file(&stream).unwrap();
        assert!(!kept(&index, 7, 3), "no stream");
        assert!(room(&Free(Some(needed - former)), &dir, "db", 3).is_err());
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
