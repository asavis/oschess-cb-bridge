//! The keeper of the position indexes of the databases in use (#149): a
//! thread that a serving bridge starts, which looks at them at once and then
//! every minute, and queues a background build ([`super::schedule`]) for each
//! whose index is not of the generation the database has had for a minute.
//!
//! A database is in use when it is on the list and either has index files in
//! the index folder, or had its explorer or the games of one of its positions
//! asked for since the bridge started ([`Registry::mark_in_use`]); at the
//! start, the ready database with the most records is in use too. Its build
//! is queued once it opens `ready` ([`Entry::open`], which never downloads: a
//! cloud-only or downloading database is never read in the background, and a
//! PGN file waits until its header index is ready), and once it has had its
//! generation for the quiet period: a database that keeps changing, as while
//! ChessBase writes to it or a cloud provider syncs it, waits until it stops.
//! A build that failed, or had no room on the disk, is not tried again in the
//! background until the database changes.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::api::App;
use crate::catalog::{Entry, Opened};
use crate::machine::Machine;

use super::schedule::Kind;
use super::source::Source;
use super::{Registry, State, kept, lock, paths, room};

/// How often the keeper looks at the databases in use.
pub const TICK: Duration = Duration::from_secs(60);
/// How long a database keeps its generation before its index is rebuilt in
/// the background.
pub const QUIET: Duration = Duration::from_secs(60);

/// How often the keeper looks, how long a database must have kept its
/// generation before its index is rebuilt in the background, and whether the
/// keeper was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Keeping {
    tick: Duration,
    quiet: Duration,
    stopped: bool,
}

impl Default for Keeping {
    fn default() -> Keeping {
        Keeping { tick: TICK, quiet: QUIET, stopped: false }
    }
}

/// A database's generation as the keeper first saw it, and since when the
/// database has had it.
pub(super) struct Seen {
    generation: u64,
    since: Instant,
}

/// Starts the keeper of the indexes of `app`'s databases, which looks at once
/// and then every [`TICK`], and ends once `app` is gone or the keeper is
/// stopped ([`Registry::stop_keeping`]). It keeps indexes only in an index
/// folder the bridge was given ([`Registry::set_dir`]), never in the default
/// one.
pub fn start(app: &Arc<App>) {
    let app: Weak<App> = Arc::downgrade(app);
    let spawned =
        std::thread::Builder::new().name("bridge-keeper".into()).stack_size(crate::THREAD_STACK).spawn(move || {
            while let Some(app) = app.upgrade() {
                let registry = &app.catalog.explorer;
                let Keeping { tick, stopped, .. } = *lock(&registry.keeping);
                if stopped {
                    return;
                }
                if lock(&registry.dir).is_some() {
                    registry.keep(&app.catalog.entries());
                }
                drop(app);
                std::thread::sleep(tick);
            }
        });
    if let Err(e) = spawned {
        crate::log!("cannot start the keeper of the position indexes: {e}");
    }
}

impl Registry {
    /// Sets how often the keeper looks, from its next look on, and how long
    /// a database must have kept its generation before its index is rebuilt
    /// in the background: [`TICK`] and [`QUIET`] unless set. Tests set them
    /// before the keeper starts.
    pub fn set_keeping(&self, tick: Duration, quiet: Duration) {
        let mut keeping = lock(&self.keeping);
        (keeping.tick, keeping.quiet) = (tick, quiet);
    }

    /// Stops the keeper for good, once the look it may be taking is over:
    /// it queues no build from now on. Tests stop it before they remove the
    /// files it would rebuild.
    pub fn stop_keeping(&self) {
        lock(&self.keeping).stopped = true;
    }

    /// One look of the keeper at `entries`, the databases as listed now:
    /// queues a background build for each database in use whose index is not
    /// of the generation it has had for the quiet period. While the computer
    /// runs on battery, the background build running stops and waits. Does
    /// nothing without an index folder the bridge was given.
    pub fn keep(&self, entries: &[Arc<Entry>]) {
        // Held while it looks, so that a stop waits for the look to end.
        let keeping = lock(&self.keeping);
        let Some(dir) = lock(&self.dir).clone().filter(|_| !keeping.stopped) else { return };
        let machine = self.builds.machine();
        if machine.on_battery() {
            self.builds.pause();
        }
        self.builds.poke();
        let listed: Vec<&Arc<Entry>> = entries.iter().filter(|e| e.listed()).collect();
        if !self.picked.swap(true, Ordering::Relaxed) {
            // The first look: the ready database with the most records, the
            // first of those with as many, is in use from the start.
            let mut largest: Option<(u32, &str)> = None;
            for entry in &listed {
                let Ok(open) = entry.open() else { continue };
                let records = open.db.record_count();
                if largest.is_none_or(|(most, _)| records > most) {
                    largest = Some((records, &entry.id));
                }
            }
            if let Some((_, id)) = largest {
                self.mark_in_use(id);
            }
        }
        let quiet = keeping.quiet;
        let now = Instant::now();
        let mut seen = lock(&self.seen);
        seen.retain(|id, _| listed.iter().any(|e| &e.id == id));
        for entry in listed {
            let (index, stream) = paths(&dir, &entry.id);
            if !lock(&self.used).contains(&entry.id) && !index.exists() && !stream.exists() {
                continue;
            }
            // Missing, cloud-only, downloading, opening or unreadable: not
            // read here.
            let (open, changed) = entry.open_dated();
            let Ok(open) = open else { continue };
            let since = match seen.get(&entry.id) {
                Some(s) if s.generation == open.generation => s.since,
                // A generation first seen: the database has had it since its
                // files last changed, as far as the quiet period goes back.
                _ => {
                    let age = changed.and_then(|t| SystemTime::now().duration_since(t).ok()).unwrap_or_default();
                    let since = now.checked_sub(age.min(quiet)).unwrap_or(now);
                    seen.insert(entry.id.clone(), Seen { generation: open.generation, since });
                    since
                }
            };
            if now.duration_since(since) >= quiet {
                self.renew(entry, &open, &dir, &*machine);
            }
        }
    }

    /// Queues a background build of the index of `entry` at the generation of
    /// `open`, unless its index is of that generation, in memory or on disk,
    /// its build runs or waits, or its build at that generation failed. With
    /// too little room on the disk, the build fails without being queued.
    fn renew(&self, entry: &Arc<Entry>, open: &Opened, dir: &Path, machine: &dyn Machine) {
        let state = self.state(&entry.id);
        let mut s = lock(&state);
        match &*s {
            State::Ready(l) if l.generation == open.generation => return,
            State::Working(_) => return,
            State::Failed { generation, .. } if *generation == open.generation => return,
            _ => {}
        }
        let records = open.db.records();
        if kept(&paths(dir, &entry.id).0, open.generation, records) {
            return;
        }
        if let Err(why) = room(machine, dir, &entry.id, records) {
            crate::log!("database {} is not indexed in the background: {why}", entry.id);
            *s = State::Failed { at: Instant::now(), why, generation: open.generation };
            return;
        }
        self.queue(s, &state, Arc::clone(entry), open, dir.to_path_buf(), Kind::Background);
    }
}
