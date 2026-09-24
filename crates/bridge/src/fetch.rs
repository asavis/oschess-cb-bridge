//! Cloud-only databases: files a cloud storage provider keeps only as
//! placeholders until they are read (`docs/api.md`, "Cloud-only databases").
//! Listing looks at file attributes and never reads such a file; opening the
//! database for its games reads its files in the background, one database at
//! a time, which makes the provider bring them to this computer.

use std::collections::VecDeque;
use std::fs::Metadata;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How the bridge sees and fetches cloud-only files. [`System`] is the real
/// one; tests stand in their own, since only Windows has placeholders.
pub trait Cloud: Send + Sync {
    /// Whether `path`, whose metadata this is, is a cloud-only placeholder.
    /// Must not read the file.
    fn is_cloud_only(&self, path: &Path, meta: &Metadata) -> bool;

    /// Reads the whole of `path`, passing each count of bytes read to `read`.
    fn fetch(&self, path: &Path, read: &mut dyn FnMut(u64)) -> std::io::Result<()>;
}

/// Windows placeholders, recognised by their file attributes. Other systems
/// have none: there every file is taken as local.
pub struct System;

impl Cloud for System {
    #[cfg(windows)]
    fn is_cloud_only(&self, _: &Path, meta: &Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        cbformat::dbitems::is_cloud_only(meta.file_attributes())
    }

    #[cfg(not(windows))]
    fn is_cloud_only(&self, _: &Path, _: &Metadata) -> bool {
        false
    }

    fn fetch(&self, path: &Path, read: &mut dyn FnMut(u64)) -> std::io::Result<()> {
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => read(n as u64),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// A download's progress: bytes of the database's files on this computer, of
/// `total`.
pub struct Progress {
    present: AtomicU64,
    pub total: u64,
}

impl Progress {
    pub fn new(present: u64, total: u64) -> Progress {
        Progress { present: AtomicU64::new(present), total }
    }

    pub fn present(&self) -> u64 {
        self.present.load(Ordering::Relaxed).min(self.total)
    }

    pub fn add(&self, bytes: u64) {
        self.present.fetch_add(bytes, Ordering::Relaxed);
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// Runs jobs one after another on a background thread, which starts when a
/// job arrives and ends when none is left.
pub struct Serial {
    /// The jobs waiting, and whether a thread runs them.
    queue: Mutex<(VecDeque<Job>, bool)>,
    /// Starting a thread fails, for tests.
    refuse: AtomicBool,
    /// What the jobs do, for the thread's name and messages.
    label: &'static str,
}

impl Default for Serial {
    fn default() -> Serial {
        Serial::labelled("download")
    }
}

impl Serial {
    /// A queue whose thread and messages are named `label`.
    pub fn labelled(label: &'static str) -> Serial {
        Serial { queue: Mutex::default(), refuse: AtomicBool::new(false), label }
    }

    /// Queues `job`, starting the thread when none runs. When the thread
    /// cannot start, `job` is dropped unrun and `false` returned: nothing is
    /// left waiting for a thread that does not exist.
    pub fn submit(self: &Arc<Self>, job: Job) -> bool {
        let mut queue = lock(&self.queue);
        queue.0.push_back(job);
        if queue.1 {
            return true;
        }
        let me = Arc::clone(self);
        let started = if self.refuse.load(Ordering::Relaxed) {
            Err(std::io::Error::other("refused for a test"))
        } else {
            std::thread::Builder::new().name(self.label.into()).spawn(move || me.run()).map(drop)
        };
        match started {
            Ok(()) => {
                queue.1 = true;
                true
            }
            Err(e) => {
                eprintln!("oschess-bridge: cannot start a {} thread: {e}", self.label);
                // No thread runs, so this is the only job queued.
                let job = queue.0.pop_back();
                drop(queue);
                drop(job);
                false
            }
        }
    }

    /// Makes starting the thread fail while `refuse` holds. Tests use it.
    pub fn refuse_starts(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::Relaxed);
    }

    fn run(&self) {
        // Should the thread end by a panic all the same, the jobs still
        // waiting are dropped unrun and the next job submitted starts a
        // new thread.
        let _exit = Exit(self);
        loop {
            let job = {
                let mut queue = lock(&self.queue);
                match queue.0.pop_front() {
                    Some(job) => job,
                    None => {
                        queue.1 = false;
                        return;
                    }
                }
            };
            // A panicking job must not end the thread.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                eprintln!("oschess-bridge: a {} job failed with a bug", self.label);
            }
        }
    }
}

struct Exit<'a>(&'a Serial);

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let jobs = {
                let mut queue = lock(&self.0.queue);
                queue.1 = false;
                std::mem::take(&mut queue.0)
            };
            drop(jobs);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread that cannot start leaves no job waiting, and the next job
    /// submitted starts one.
    #[test]
    fn a_failed_start_leaves_nothing_waiting() {
        let serial = Arc::new(Serial::default());
        let ran = Arc::new(AtomicU64::new(0));
        let job = |ran: &Arc<AtomicU64>| -> Job {
            let ran = Arc::clone(ran);
            Box::new(move || {
                ran.fetch_add(1, Ordering::SeqCst);
            })
        };
        serial.refuse_starts(true);
        assert!(!serial.submit(job(&ran)));
        assert!(!serial.submit(job(&ran)));
        assert!(lock(&serial.queue).0.is_empty());
        serial.refuse_starts(false);
        assert!(serial.submit(job(&ran)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ran.load(Ordering::SeqCst) < 1 || lock(&serial.queue).1 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
