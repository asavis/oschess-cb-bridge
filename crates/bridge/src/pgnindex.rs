//! The indexes of PGN databases (`cbformat::pgnfile`): built in the
//! background, one at a time, the first time a PGN file is opened at a
//! generation, and kept in the data folder's `pgn` folder, so that a
//! restarted bridge opens the file at once. While its index is built the
//! database is `opening`, with the bytes of the file read so far.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cbformat::codepage::CodePage;
use cbformat::pgnfile;

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
}

impl Default for Registry {
    fn default() -> Registry {
        Registry {
            dir: Mutex::default(),
            builds: Mutex::default(),
            queue: Arc::new(Serial::labelled("pgn")),
            page: system_code_page(),
        }
    }
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
