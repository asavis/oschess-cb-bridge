//! The heads file (#106): every header record of one database generation as a
//! [`Slim`] row, kept in the index folder beside the position index. A pass
//! over it reads 36 bytes a record instead of the record: 430 MB for the Mega
//! Database instead of 2.2 GB.
//!
//! Layout: a 64-byte header, the rows in record order, then one CRC-32 per
//! block of [`BLOCK_ROWS`] rows. The header holds the generation it was built
//! for, the record count, the table's CRC and its own. A file of another
//! generation, or one that does not read back whole, is never used; a block
//! that fails its CRC is read from the database instead, and the file is
//! dropped and built again.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cbformat::file::DbFile;

use super::slim::{ROW, Slim};
use crate::indexdir::{self, Unlisted, crc32, u32_at, u64_at};
use crate::store::Store;

const MAGIC: [u8; 8] = *b"OSCBHDS\0";
const VERSION: u32 = 1;
const HEADER: usize = 64;
/// Rows a block holds and one CRC covers: one worker's batch in a pass.
pub const BLOCK_ROWS: u32 = super::scan::CHUNK;
/// Databases smaller than this keep their full records: a pass over them is
/// quick without a copy.
pub const MIN_RECORDS: u32 = 1 << 16;
/// How long after a build the database changed under, or failed, the next one
/// waits: a database that ChessBase is still writing is not copied over and
/// over.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// A database's heads file, open.
pub struct Heads {
    file: DbFile,
    pub path: PathBuf,
    pub generation: u64,
    pub records: u32,
    crcs: Vec<u32>,
    /// A block failed its CRC: passes read the database's own records again.
    broken: AtomicBool,
}

impl Heads {
    /// The heads file at `path`, when it was built for `generation` and a
    /// database of `records` records and reads back whole; `None` otherwise.
    pub fn open(path: &Path, generation: u64, records: u32) -> Option<Heads> {
        let file = DbFile::open(path.to_path_buf()).ok()?;
        let mut h = [0u8; HEADER];
        file.read_into(0, &mut h).ok()?;
        if h[0..8] != MAGIC || u32_at(&h, 8) != VERSION || crc32(&h[..60]) != u32_at(&h, 60) {
            return None;
        }
        let (row, file_records, block_rows) = (u32_at(&h, 12), u32_at(&h, 24), u32_at(&h, 28));
        if row as usize != ROW || u64_at(&h, 16) != generation || file_records != records || block_rows != BLOCK_ROWS {
            return None;
        }
        let blocks = records.div_ceil(BLOCK_ROWS) as usize;
        let table_at = HEADER as u64 + u64::from(records) * ROW as u64;
        if file.size().ok()? != table_at + blocks as u64 * 4 {
            return None;
        }
        let mut table = vec![0u8; blocks * 4];
        file.read_into(table_at, &mut table).ok()?;
        if crc32(&table) != u32_at(&h, 32) {
            return None;
        }
        Some(Heads {
            file,
            path: path.to_path_buf(),
            generation,
            records,
            crcs: table.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect(),
            broken: AtomicBool::new(false),
        })
    }

    pub fn blocks(&self) -> u32 {
        self.crcs.len() as u32
    }

    /// Whether passes read this file: no block has failed its CRC.
    pub fn usable(&self) -> bool {
        !self.broken.load(Ordering::Relaxed)
    }

    /// The rows of block `block` into `buf`, their count; `None` when the
    /// block cannot be read or fails its CRC, which marks the file broken.
    /// Every read is checked: the file can change after an earlier pass.
    pub fn read_block(&self, block: u32, buf: &mut [u8]) -> Option<u32> {
        let first = block * BLOCK_ROWS;
        let rows = (self.records - first).min(BLOCK_ROWS);
        let bytes = &mut buf[..rows as usize * ROW];
        let ok = self.file.read_into(HEADER as u64 + u64::from(first) * ROW as u64, bytes).is_ok()
            && crc32(bytes) == self.crcs[block as usize];
        if !ok {
            self.broken.store(true, Ordering::Relaxed);
        }
        ok.then_some(rows)
    }
}

/// How a build ended.
pub enum Built {
    Ready(Heads),
    /// A record holds a value no row can: the database keeps its full records.
    Unsuited,
    /// The database changed while it was read; nothing was kept.
    Changed,
}

/// Builds the heads file of `db` at `generation` as `path`: written beside it
/// as `<path>.partial`, then renamed. `still` is asked between blocks whether
/// the database is still at that generation. The error, for the log, names
/// no path.
pub fn build<S: Store>(db: &S, generation: u64, path: &Path, still: &dyn Fn() -> bool) -> Result<Built, String> {
    let records = db.record_count();
    let partial = partial_path(path);
    let result = write(db, generation, records, &partial, still);
    match result {
        Ok(true) => {
            replace(&partial, path).map_err(|e| format!("renaming the new heads file: {e}"))?;
            Heads::open(path, generation, records)
                .map(Built::Ready)
                .ok_or_else(|| "the new file does not read back".into())
        }
        Ok(false) | Err(_) => {
            let _ = std::fs::remove_file(&partial);
            match result {
                Ok(_) if still() => Ok(Built::Unsuited),
                Ok(_) => Ok(Built::Changed),
                Err(e) => Err(e),
            }
        }
    }
}

/// [`build`] for an open database of any format.
pub fn build_base(
    db: &cbformat::view::Base,
    generation: u64,
    path: &Path,
    still: &dyn Fn() -> bool,
) -> Result<Built, String> {
    crate::store::with_store!(db, s => build(s, generation, path, still))
}

/// Renames `partial` to `path`. A pass still reading a broken file that was
/// removed holds its name on Windows until the pass ends, so a refusal is
/// tried again for a few seconds.
fn replace(partial: &Path, path: &Path) -> std::io::Result<()> {
    let until = Instant::now() + REPLACE_WAIT;
    loop {
        match std::fs::rename(partial, path) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

/// How long [`replace`] waits for the readers of a removed file.
const REPLACE_WAIT: Duration = Duration::from_secs(10);

/// Writes the file; `false` when a record does not fit a row or the database
/// changed meanwhile.
fn write<S: Store>(
    db: &S,
    generation: u64,
    records: u32,
    partial: &Path,
    still: &dyn Fn() -> bool,
) -> Result<bool, String> {
    let io = |e: std::io::Error| format!("writing the heads file: {e}");
    if let Some(dir) = partial.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut out = std::io::BufWriter::with_capacity(1 << 20, File::create(partial).map_err(io)?);
    out.write_all(&[0u8; HEADER]).map_err(io)?;
    let mut buf = vec![0u8; BLOCK_ROWS as usize * S::HEAD_BYTES];
    let mut rows = vec![0u8; BLOCK_ROWS as usize * ROW];
    let mut crcs = Vec::with_capacity(records.div_ceil(BLOCK_ROWS) as usize * 4);
    let mut first = 1u32;
    while first <= records {
        if !still() {
            return Ok(false);
        }
        let want = (records - first + 1).min(BLOCK_ROWS) as usize;
        let read =
            db.read_records(first, &mut buf[..want * S::HEAD_BYTES]).map_err(|e| crate::log::error(&e))? as usize;
        if read != want {
            return Err(format!("record {first} read short: {read} of {want}"));
        }
        for i in 0..read {
            let at = i * S::HEAD_BYTES;
            let Some(row) = Slim::encode(&S::head(first + i as u32, &buf[at..at + S::HEAD_BYTES])) else {
                return Ok(false);
            };
            rows[i * ROW..(i + 1) * ROW].copy_from_slice(&row);
        }
        let block = &rows[..read * ROW];
        out.write_all(block).map_err(io)?;
        crcs.extend_from_slice(&crc32(block).to_le_bytes());
        first += read as u32;
    }
    out.write_all(&crcs).map_err(io)?;
    let mut file = out.into_inner().map_err(|e| io(e.into_error()))?;
    let mut h = [0u8; HEADER];
    h[0..8].copy_from_slice(&MAGIC);
    h[8..12].copy_from_slice(&VERSION.to_le_bytes());
    h[12..16].copy_from_slice(&(ROW as u32).to_le_bytes());
    h[16..24].copy_from_slice(&generation.to_le_bytes());
    h[24..28].copy_from_slice(&records.to_le_bytes());
    h[28..32].copy_from_slice(&BLOCK_ROWS.to_le_bytes());
    h[32..36].copy_from_slice(&crc32(&crcs).to_le_bytes());
    let crc = crc32(&h[..60]);
    h[60..64].copy_from_slice(&crc.to_le_bytes());
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    file.write_all(&h).map_err(io)?;
    file.sync_all().map_err(io)?;
    Ok(still())
}

/// The heads file of database `id` in `dir`.
pub fn path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.heads"))
}

fn partial_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".partial");
    PathBuf::from(p)
}

/// Partial files being written now, which the sweep leaves alone: a names
/// file's writer is no build the registry tracks (#108).
static WRITING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Marks `path` as being written until the guard drops.
pub struct Writing(PathBuf);

impl Writing {
    pub fn new(path: PathBuf) -> Writing {
        WRITING.lock().unwrap_or_else(|e| e.into_inner()).push(path.clone());
        Writing(path)
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let mut writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = writing.iter().position(|p| *p == self.0) {
            writing.swap_remove(i);
        }
    }
}

fn being_written(path: &Path) -> bool {
    WRITING.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|p| p == path)
}

/// What the heads sweep keeps and removes: the heads file and the names
/// files beside it (#108).
const KEPT: [&str; 4] = [".heads", ".players", ".tournaments", ".annotators"];

/// The database id of a heads folder entry: `<id>` and one of [`KEPT`],
/// perhaps followed by `.partial`.
pub fn entry_id(name: &str) -> Option<&str> {
    let whole = name.strip_suffix(".partial").unwrap_or(name);
    indexdir::db_id(whole, &KEPT).map(|(id, _)| id)
}

/// Where the heads files of the listed databases stand.
#[derive(Default)]
pub struct Registry {
    states: Mutex<HashMap<String, State>>,
    min_records: Mutex<Option<u32>>,
    /// Heads files whose database is not on the list, and since when.
    pub(crate) unlisted: Mutex<Unlisted>,
}

enum State {
    /// A build of this generation runs.
    Working(u64),
    Ready(Arc<Heads>),
    /// This generation's records do not all fit rows.
    Unsuited(u64),
    /// The last build failed or its database changed under it: no new one
    /// starts before [`RETRY_AFTER`].
    Waiting(Instant),
}

/// What [`Registry::lookup`] tells the caller.
pub enum Lookup {
    Ready(Arc<Heads>),
    /// Nothing to use now; a build of `generation` should start.
    Build,
    /// Nothing to use now, and nothing to start.
    None,
}

impl Registry {
    /// Sets the size from which databases get a heads file, for tests.
    pub fn set_min_records(&self, n: u32) {
        *self.min_records.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    fn min_records(&self) -> u32 {
        self.min_records.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(MIN_RECORDS)
    }

    /// The heads of database `id` at `generation` with `records` records: the
    /// one open, else the file on disk in `dir` for that generation, else
    /// [`Lookup::Build`], after which the caller runs [`Registry::built`].
    pub fn lookup(&self, dir: &Path, id: &str, generation: u64, records: u32) -> Lookup {
        if records < self.min_records() {
            return Lookup::None;
        }
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.get(id) {
            Some(State::Ready(h)) if h.generation == generation && h.usable() => return Lookup::Ready(Arc::clone(h)),
            Some(State::Ready(h)) if h.generation == generation => {
                // A block failed its CRC: the file goes, and is built again.
                // Its handles are let go first: Windows keeps a file's name
                // until the last one closes.
                let path = h.path.clone();
                states.remove(id);
                let _ = std::fs::remove_file(&path);
            }
            Some(State::Working(g)) if *g == generation => return Lookup::None,
            Some(State::Unsuited(g)) if *g == generation => return Lookup::None,
            Some(State::Waiting(at)) if at.elapsed() < RETRY_AFTER => return Lookup::None,
            _ => {}
        }
        if let Some(h) = Heads::open(&path(dir, id), generation, records) {
            let h = Arc::new(h);
            states.insert(id.to_string(), State::Ready(Arc::clone(&h)));
            return Lookup::Ready(h);
        }
        states.insert(id.to_string(), State::Working(generation));
        Lookup::Build
    }

    /// Records how the build of `id` at `generation` ended.
    pub fn built(&self, id: &str, generation: u64, result: Result<Built, String>) -> Option<Arc<Heads>> {
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let (state, ready) = match result {
            Ok(Built::Ready(h)) => {
                let h = Arc::new(h);
                (State::Ready(Arc::clone(&h)), Some(h))
            }
            Ok(Built::Unsuited) => (State::Unsuited(generation), None),
            Ok(Built::Changed) => (State::Waiting(Instant::now()), None),
            Err(why) => {
                crate::log!("copying the headers of database {id} failed: {why}");
                (State::Waiting(Instant::now()), None)
            }
        };
        states.insert(id.to_string(), state);
        ready
    }

    /// Records the build of `id` at `generation`, which runs on this thread
    /// until the guard drops, as failed should it panic ([`Unwinding`]).
    pub fn unwinding<'a>(&'a self, id: &'a str, generation: u64) -> Unwinding<'a> {
        Unwinding { registry: self, id, generation }
    }

    /// Removes from `dir` the heads files no listed database uses, as the
    /// position index's sweep does (#60): a database's file once it has been
    /// off the list for the grace, and a build's partial file when no build
    /// of its database runs. Nothing else in `dir` is touched.
    pub fn sweep(&self, dir: &Path, listed: &std::collections::HashSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let now = Instant::now();
        let mut unlisted = self.unlisted.lock().unwrap_or_else(|e| e.into_inner());
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = entry_id(name) else { continue };
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(states.get(id), Some(State::Working(_))) {
                continue;
            }
            if name.ends_with(".partial") {
                if !being_written(&entry.path()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            } else if !listed.contains(id) {
                if unlisted.due(id, now) && std::fs::remove_file(entry.path()).is_ok() {
                    states.remove(id);
                } else {
                    unlisted.still(id);
                }
            }
        }
        unlisted.retain();
    }
}

/// Records a heads build whose job panics as failed, as the job unwinds to
/// the queue that catches it ([`crate::fetch::Serial`]) (#172): left
/// working, its database would never have its file built, and keep a partial
/// file that no sweep removes. The next build starts after [`RETRY_AFTER`],
/// and the sweep removes the partial file meanwhile. A normal return leaves
/// the record to [`Registry::built`].
pub struct Unwinding<'a> {
    registry: &'a Registry,
    id: &'a str,
    generation: u64,
}

impl Drop for Unwinding<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.registry.built(self.id, self.generation, Err("the build failed with a bug".into()));
        }
    }
}

/// Record numbers a pass over the heads file has read, for tests.
pub static ROWS_READ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_leaves_a_partial_file_being_written() {
        let dir = std::env::temp_dir().join(format!("bridge-heads-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("0123456789abcdef.players.partial");
        std::fs::write(&partial, b"x").unwrap();
        let listed: std::collections::HashSet<String> = ["0123456789abcdef".to_string()].into();
        let registry = Registry::default();
        let writing = Writing::new(partial.clone());
        registry.sweep(&dir, &listed);
        assert!(partial.exists(), "a partial file being written stays");
        drop(writing);
        registry.sweep(&dir, &listed);
        assert!(!partial.exists(), "a partial file left by no writer goes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A heads build whose job panics is recorded as failed (#172): no build
    /// starts again before [`RETRY_AFTER`], the sweep removes the partial
    /// file the build left, and the next request after that starts a new
    /// build, whose file is attached.
    #[test]
    fn a_build_that_panics_is_recorded_as_failed() {
        use crate::catalog::Catalog;
        use crate::fetch::Cloud;
        use crate::sources::Sources;
        use cbformat::fixture::{Builder, quiet};
        use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

        /// Every file on this computer. A build asks between its blocks
        /// whether its database is still the same, which looks at its files:
        /// asked so on the heads thread while `bug` holds, it panics, noting
        /// whether the build's partial file was being written.
        struct Buggy {
            bug: AtomicBool,
            partial: PathBuf,
            written: AtomicBool,
        }
        impl Cloud for Buggy {
            fn is_cloud_only(&self, _: &Path, _: &std::fs::Metadata) -> bool {
                if self.bug.load(Ordering::Relaxed) && std::thread::current().name() == Some("heads") {
                    self.written.fetch_or(self.partial.exists(), Ordering::Relaxed);
                    panic!("a bug in the build");
                }
                false
            }
            fn fetch(&self, _: &Path, _: &mut dyn FnMut(u64)) -> std::io::Result<()> {
                Err(std::io::Error::other("not for a test"))
            }
        }
        let mut b = Builder::new();
        let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
        for _ in 0..3 {
            b.game(e4);
        }
        let db = b.write("heads-bug");
        let db_path = db.dir().join("db.2cbh");
        let dir = std::env::temp_dir().join(format!("bridge-heads-bug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = crate::catalog::id_of(&db_path);
        let partial = partial_path(&path(&dir, &id));
        let buggy = Buggy { bug: AtomicBool::new(true), partial: partial.clone(), written: AtomicBool::new(false) };
        let cloud = Arc::new(buggy);
        let sources = Sources { fixed: vec![db_path], ..Sources::default() };
        let catalog = Catalog::with_sources(sources, Arc::clone(&cloud) as Arc<dyn Cloud>);
        catalog.explorer.set_dir(dir.clone());
        catalog.heads.set_min_records(1);
        let entry = Arc::clone(&catalog.entries()[0]);
        let Ok(open) = entry.open() else { panic!("the database does not open") };
        let states = || catalog.heads.states.lock().unwrap_or_else(|e| e.into_inner());
        catalog.attach_heads(&entry, &open);
        let until = Instant::now() + Duration::from_secs(30);
        while matches!(states().get(&id), Some(State::Working(_))) {
            assert!(Instant::now() < until, "the build is over");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(states().get(&id), Some(State::Waiting(_))), "the build is recorded as failed");
        assert!(cloud.written.load(Ordering::Relaxed), "the build was writing its partial file");
        let records = open.db.record_count();
        assert!(matches!(catalog.heads.lookup(&dir, &id, open.generation, records), Lookup::None), "too soon");
        catalog.sweep_indexes();
        assert!(!partial.exists(), "the partial file is swept");
        // RETRY_AFTER later.
        if let Some(State::Waiting(at)) = states().get_mut(&id) {
            *at = at.checked_sub(RETRY_AFTER).expect("the clock goes back that far");
        }
        cloud.bug.store(false, Ordering::Relaxed);
        let until = Instant::now() + Duration::from_secs(30);
        while !open.indexes.has_usable_heads() {
            catalog.attach_heads(&entry, &open);
            assert!(Instant::now() < until, "a heads file was attached");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(path(&dir, &id).exists() && !partial.exists());
        drop((open, entry, catalog));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_bridges_own_names_are_heads_entries() {
        assert_eq!(entry_id("0123456789abcdef.heads"), Some("0123456789abcdef"));
        assert_eq!(entry_id("0123456789abcdef.heads.partial"), Some("0123456789abcdef"));
        assert_eq!(entry_id("0123456789abcdef.players"), Some("0123456789abcdef"));
        assert_eq!(entry_id("0123456789abcdef.annotators.partial"), Some("0123456789abcdef"));
        assert_eq!(entry_id("0123456789abcdef.idx"), None);
        assert_eq!(entry_id("notes.heads"), None);
    }
}
