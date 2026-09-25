//! The indexes of PGN databases (`cbformat::pgnfile`): built in the
//! background, one at a time, the first time a PGN file is opened at a
//! generation, and kept in the data folder's `pgn` folder, so that a
//! restarted bridge opens the file at once. While its index is built the
//! database is `opening`, with the bytes of the file read so far.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cbformat::codepage::CodePage;
use cbformat::pgnfile;

use crate::explorer::SWEEP_GRACE;
use crate::fetch::{Progress, Serial};

/// How long a failed build is reported before the next request tries again.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

enum Build {
    Idle,
    /// A build running or queued, for the generation it reads.
    Working(Arc<Progress>),
    /// The build for `generation` failed at the instant.
    Failed(u64, Instant),
}

/// What opening a PGN database finds.
pub enum Opening {
    Ready(pgnfile::Database),
    /// Its index is being built; the database is `opening`.
    Pending(Arc<Progress>),
    /// Its index could not be built; the database is `unreadable`.
    Failed,
}

/// The index builds of all PGN databases, and the queue they run in.
pub struct Registry {
    dir: Mutex<Option<PathBuf>>,
    builds: Mutex<HashMap<String, Arc<Mutex<Build>>>>,
    queue: Arc<Serial>,
    /// The code page of games that are not UTF-8: this computer's.
    page: CodePage,
    /// Header indexes whose database is not on the list, and since when.
    unlisted: Mutex<HashMap<String, Instant>>,
    grace: Mutex<Duration>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry {
            dir: Mutex::default(),
            builds: Mutex::default(),
            queue: Arc::new(Serial::labelled("pgn")),
            page: system_code_page(),
            unlisted: Mutex::default(),
            grace: Mutex::new(SWEEP_GRACE),
        }
    }
}

/// What a file in the header index folder is, by its name.
enum Kept {
    /// `<id>.head`: a PGN database's header index.
    Index,
    /// `<id>.head.partial`: a build's work, which only the build running for
    /// `<id>` uses.
    Work,
}

/// The database id and kind of a header index folder entry; `None` for
/// anything the bridge did not write there, which is never touched.
fn index_entry(name: &str) -> Option<(&str, Kept)> {
    let (id, kind) = match name.strip_suffix(".head") {
        Some(id) => (id, Kept::Index),
        None => (name.strip_suffix(".head.partial")?, Kept::Work),
    };
    (id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some((id, kind))
}

impl Registry {
    /// Keeps index files in `dir` (the bridge's data folder's `pgn`).
    pub fn set_dir(&self, dir: PathBuf) {
        *lock(&self.dir) = Some(dir);
    }

    /// Where index files are kept; the data folder's `pgn` unless set.
    pub fn dir(&self) -> Option<PathBuf> {
        lock(&self.dir).clone().or_else(|| crate::token::data_dir().map(|d| d.join("pgn")))
    }

    /// The queue builds run in.
    pub fn queue(&self) -> &Arc<Serial> {
        &self.queue
    }

    fn build(&self, id: &str) -> Arc<Mutex<Build>> {
        Arc::clone(lock(&self.builds).entry(id.to_string()).or_insert_with(|| Arc::new(Mutex::new(Build::Idle))))
    }

    /// The build of `id` running or queued, if any.
    pub fn progress(&self, id: &str) -> Option<Arc<Progress>> {
        match &*lock(&self.build(id)) {
            Build::Working(p) => Some(Arc::clone(p)),
            _ => None,
        }
    }

    /// Sets how long a header index outlives its database's place on the list.
    pub fn set_sweep_grace(&self, grace: Duration) {
        *lock(&self.grace) = grace;
    }

    /// Removes from the header index folder what no database on the list will
    /// use, as the position index folder is swept (`crate::explorer::Registry::sweep`,
    /// #60): the index of a database off the list for [`SWEEP_GRACE`] or
    /// longer, and a build's work left by a build that no longer runs. The
    /// files of a database being read are never touched, nor anything the
    /// bridge did not write.
    pub fn sweep(&self, listed: &HashSet<String>) {
        let Some(dir) = self.dir() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let (now, grace) = (Instant::now(), *lock(&self.grace));
        let mut unlisted = lock(&self.unlisted);
        let mut still = HashSet::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some((id, kind)) = name.to_str().and_then(index_entry) else { continue };
            // Held while the file goes, so no build of `id` starts meanwhile.
            let build = self.build(id);
            let b = lock(&build);
            if matches!(*b, Build::Working(_)) {
                continue;
            }
            let path = entry.path();
            match kind {
                Kept::Index if listed.contains(id) => {}
                Kept::Index => {
                    let since = *unlisted.entry(id.to_string()).or_insert(now);
                    if now.duration_since(since) < grace || std::fs::remove_file(&path).is_err() {
                        still.insert(id.to_string());
                    }
                }
                Kept::Work => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        // A database back on the list, or an index gone, starts afresh.
        unlisted.retain(|id, _| still.contains(id));
    }

    /// The PGN file `path` of database `id` at `generation`: opened with the
    /// index built for that generation, else the build that makes it, which
    /// starts now if none runs. While another generation's build runs, the
    /// file waits for it: the next request after it ends starts afresh.
    pub fn open(&self, id: &str, path: &Path, generation: u64) -> Opening {
        let Some(dir) = self.dir() else { return Opening::Failed };
        let index = dir.join(format!("{id}.head"));
        let build = self.build(id);
        let mut b = lock(&build);
        match &*b {
            Build::Working(p) => return Opening::Pending(Arc::clone(p)),
            Build::Failed(g, at) if *g == generation && at.elapsed() < RETRY_AFTER_FAILURE => return Opening::Failed,
            _ => {}
        }
        if let Ok(db) = pgnfile::Database::open(path, &index, generation, self.page) {
            *b = Build::Idle;
            return Opening::Ready(db);
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("oschess-bridge: {}: {e}", dir.display());
            *b = Build::Failed(generation, Instant::now());
            return Opening::Failed;
        }
        let total = std::fs::metadata(path).map_or(0, |m| m.len());
        let progress = Arc::new(Progress::new(0, total));
        *b = Build::Working(Arc::clone(&progress));
        drop(b);
        let (job_build, p, path, page) = (Arc::clone(&build), Arc::clone(&progress), path.to_path_buf(), self.page);
        let started = self.queue.submit(Box::new(move || {
            let mut last = 0;
            let built = pgnfile::build(&path, &index, generation, page, &mut |read| {
                p.add(read.saturating_sub(last));
                last = read;
                true
            });
            *lock(&job_build) = match built {
                Ok(_) => Build::Idle,
                Err(e) => {
                    eprintln!("oschess-bridge: reading {} failed: {e}", path.display());
                    Build::Failed(generation, Instant::now())
                }
            };
        }));
        if !started {
            *lock(&build) = Build::Failed(generation, Instant::now());
            return Opening::Failed;
        }
        Opening::Pending(progress)
    }
}

/// The code page Windows reads text that is not Unicode in: the one ChessBase
/// reads a legacy PGN file in. Elsewhere, Windows-1252.
#[cfg(windows)]
fn system_code_page() -> CodePage {
    // SAFETY: GetACP takes no arguments and only reads the system's setting.
    CodePage::new(unsafe { windows_sys::Win32::Globalization::GetACP() })
}

#[cfg(not(windows))]
fn system_code_page() -> CodePage {
    CodePage::WESTERN
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header index folder is swept as the position index folder is: the
    /// index of a database off the list once the grace has passed, a build's
    /// leftover at once, nothing of a database being read, and nothing the
    /// bridge did not write.
    #[test]
    fn the_header_index_folder_keeps_only_what_the_list_uses() {
        let dir = std::env::temp_dir().join(format!("bridge-pgn-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Registry::default();
        registry.set_dir(dir.clone());
        let (listed, gone, reading) = ("1111111111111111", "0123456789abcdef", "fedcba9876543210");
        let names = [
            format!("{listed}.head"),
            format!("{listed}.head.partial"),
            format!("{gone}.head"),
            format!("{gone}.head.partial"),
            format!("{reading}.head"),
            format!("{reading}.head.partial"),
            "notes.txt".into(),
            "0123456789ABCDEF.head".into(),
        ];
        for name in &names {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        *lock(&registry.build(reading)) = Build::Working(Arc::new(Progress::new(0, 1)));
        let on_list: HashSet<String> = [listed.to_string()].into();
        let left = || {
            let mut n: Vec<String> =
                std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            n.sort();
            n
        };
        let dropped = [format!("{listed}.head.partial"), format!("{gone}.head.partial")];
        let mut keep: Vec<String> = names.iter().filter(|n| !dropped.contains(n)).cloned().collect();
        keep.sort();

        // Within the grace, only the builds' leftovers go.
        registry.sweep(&on_list);
        assert_eq!(left(), keep);

        // After it, the index of the database off the list goes too; the
        // database being read keeps everything.
        registry.set_sweep_grace(Duration::ZERO);
        registry.sweep(&on_list);
        keep.retain(|n| n != &format!("{gone}.head"));
        assert_eq!(left(), keep);

        // Its read over, its leftover goes.
        *lock(&registry.build(reading)) = Build::Idle;
        registry.set_sweep_grace(SWEEP_GRACE);
        registry.sweep(&on_list);
        keep.retain(|n| n != &format!("{reading}.head.partial"));
        assert_eq!(left(), keep);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
