//! The queues of the position index builds (#149): one build runs at a time,
//! those a request waits for before those the keeper queues in the
//! background ([`super::keeper`]). A requested build stops a background build
//! of another database at its next batch, which then waits for its turn
//! again, behind it. A background build waits while the computer runs on
//! battery. A background build's threads run at the priority of work nothing
//! waits for ([`machine::background`]), a requested build's below the normal
//! priority.
//!
//! A background build also gives way to the work a user waits for
//! ([`crate::foreground`]): while a search, sort, list or explorer answer
//! runs, each of its threads waits before its next batch until none runs,
//! for at most [`PATIENCE`] at a time, then goes on with one batch whatever
//! runs; under foreground work that never ends, a build still ends, a batch
//! per thread every [`PATIENCE`]. A requested build never gives way: it is
//! the work a request waits for.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::machine::{self, Machine, Priority, System};
use crate::sync::{lock, unpoisoned};

use super::runs::Progress;

/// How often a background build waiting for mains power looks again, unless
/// something wakes it sooner ([`Scheduler::poke`]).
const POWER_RECHECK: Duration = Duration::from_secs(10);

/// How long each thread of a background build gives way to foreground work
/// at most, at a time, before it goes on with one batch whatever runs: long
/// enough for a search, a sort or an explorer answer to run without a batch
/// started beside it, short enough that a build under foreground work that
/// never ends still ends (#149).
pub const PATIENCE: Duration = Duration::from_millis(500);

/// Why a build is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A request for the database's positions waits for it.
    Requested,
    /// The keeper queued it for a database in use.
    Background,
}

impl Kind {
    /// The priority its threads run at.
    pub fn priority(self) -> Priority {
        match self {
            Kind::Requested => Priority::BelowNormal,
            Kind::Background => machine::background(),
        }
    }

    /// How long its threads give way to foreground work at most, at a time,
    /// when a background build's give way for `patience`: that long for a
    /// background build, never for a requested one.
    fn patience(self, patience: Duration) -> Duration {
        match self {
            Kind::Requested => Duration::ZERO,
            Kind::Background => patience,
        }
    }
}

/// How a build's run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ran {
    /// It is over: built, failed or dropped.
    Done,
    /// It stopped when asked before it was done, and runs again in its turn.
    Stopped,
}

/// A build's work, run as a build of the kind it is given, which a request
/// may have changed since it was queued; run again after it stopped.
pub type Work = Box<dyn FnMut(Kind) -> Ran + Send>;

/// A build waiting for its turn.
struct Job {
    id: String,
    kind: Kind,
    progress: Arc<Progress>,
    work: Work,
}

/// The build running: its database, its kind and its progress, through which
/// it is stopped.
struct Running {
    id: String,
    kind: Kind,
    progress: Arc<Progress>,
}

#[derive(Default)]
struct Queues {
    requested: VecDeque<Job>,
    background: VecDeque<Job>,
    running: Option<Running>,
    /// Whether a thread runs the builds.
    thread: bool,
}

pub struct Scheduler {
    queues: Mutex<Queues>,
    /// Told when a build is queued, and when the power may have changed.
    changed: Condvar,
    machine: Mutex<Arc<dyn Machine>>,
    /// How long a background build's threads give way at a time.
    patience: Mutex<Duration>,
    /// Starting the thread fails, for tests.
    refuse: AtomicBool,
}

impl Default for Scheduler {
    fn default() -> Scheduler {
        Scheduler {
            queues: Mutex::default(),
            changed: Condvar::new(),
            machine: Mutex::new(Arc::new(System)),
            patience: Mutex::new(PATIENCE),
            refuse: AtomicBool::new(false),
        }
    }
}

impl Scheduler {
    /// How the builds see the computer: its power, and the disks' free space.
    pub fn machine(&self) -> Arc<dyn Machine> {
        Arc::clone(&lock(&self.machine))
    }

    /// Sets how the builds see the computer. Tests stand in their own.
    pub fn set_machine(&self, machine: Arc<dyn Machine>) {
        *lock(&self.machine) = machine;
        self.poke();
    }

    /// How long a background build's threads give way to foreground work at
    /// most, at a time: [`PATIENCE`] unless set.
    pub fn patience(&self) -> Duration {
        *lock(&self.patience)
    }

    /// Sets how long a background build's threads give way to foreground
    /// work at most, at a time, from the next build on. Tests set it.
    pub fn set_patience(&self, patience: Duration) {
        *lock(&self.patience) = patience;
    }

    /// Queues the build of database `id`, which reports on `progress`, as
    /// `kind`: a requested one after the requested ones, where it stops a
    /// background build of another database running now, and a background
    /// one after all. A thread starts when none runs; when it cannot, the
    /// build is dropped unrun and `false` returned.
    pub fn submit(self: &Arc<Self>, id: &str, kind: Kind, progress: Arc<Progress>, work: Work) -> bool {
        let mut q = lock(&self.queues);
        let job = Job { id: id.to_string(), kind, progress, work };
        match kind {
            Kind::Requested => {
                q.requested.push_back(job);
                preempt(&q, id);
            }
            Kind::Background => q.background.push_back(job),
        }
        self.changed.notify_all();
        if q.thread {
            return true;
        }
        let me = Arc::clone(self);
        let started = if self.refuse.load(Ordering::Relaxed) {
            Err(std::io::Error::other("refused for a test"))
        } else {
            std::thread::Builder::new()
                .name("bridge-index".into())
                .stack_size(crate::THREAD_STACK)
                .spawn(move || me.run())
                .map(drop)
        };
        match started {
            Ok(()) => {
                q.thread = true;
                true
            }
            Err(e) => {
                crate::log!("cannot start an index thread: {e}");
                // The thread ends only once both queues are empty, so this
                // is the only build queued.
                let jobs = (std::mem::take(&mut q.requested), std::mem::take(&mut q.background));
                drop(q);
                drop(jobs);
                false
            }
        }
    }

    /// Makes the build of `id`, waiting or running, a requested one, since a
    /// request now waits for it: a waiting one goes after the requested ones,
    /// before the background ones, and stops a background build of another
    /// database; a running one goes on at a requested build's priority, gives
    /// way to foreground work no longer, and is no longer stopped for another
    /// request.
    pub fn promote(&self, id: &str) {
        let mut q = lock(&self.queues);
        if let Some(at) = q.background.iter().position(|j| j.id == id) {
            if let Some(mut job) = q.background.remove(at) {
                job.kind = Kind::Requested;
                q.requested.push_back(job);
                preempt(&q, id);
                self.changed.notify_all();
            }
        } else if let Some(running) = q.running.as_mut().filter(|r| r.id == id && r.kind == Kind::Background) {
            running.kind = Kind::Requested;
            running.progress.priority.store(Kind::Requested.priority() as u8, Ordering::Relaxed);
            running.progress.set_patience(Duration::ZERO);
        }
    }

    /// Stops the background build running now, which waits for its turn
    /// again: the keeper asks it while the computer runs on battery.
    pub fn pause(&self) {
        let q = lock(&self.queues);
        if let Some(running) = q.running.as_ref().filter(|r| r.kind == Kind::Background) {
            running.progress.ask_stop();
        }
    }

    /// Has a background build that waits for mains power look at it again.
    pub fn poke(&self) {
        self.changed.notify_all();
    }

    /// Makes starting the thread fail while `refuse` holds. Tests use it.
    pub fn refuse_starts(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::Relaxed);
    }

    /// The next build to run, marked running, its progress at its kind's
    /// priority and patience: the first requested one, else the first
    /// background one unless the computer runs on battery, when the thread
    /// waits for mains power or a requested build. `None`, the thread marked
    /// ended, once both queues are empty.
    fn next(&self) -> Option<Job> {
        let mut q = lock(&self.queues);
        let job = loop {
            if let Some(job) = q.requested.pop_front() {
                break job;
            }
            if q.background.is_empty() {
                q.thread = false;
                return None;
            }
            if !self.machine().on_battery() {
                match q.background.pop_front() {
                    Some(job) => break job,
                    None => continue,
                }
            }
            q = unpoisoned(self.changed.wait_timeout(q, POWER_RECHECK)).0;
        };
        q.running = Some(Running { id: job.id.clone(), kind: job.kind, progress: Arc::clone(&job.progress) });
        // Under the lock, so that a request that promotes the build from now
        // on has the last word.
        job.progress.priority.store(job.kind.priority() as u8, Ordering::Relaxed);
        job.progress.set_patience(job.kind.patience(self.patience()));
        Some(job)
    }

    fn run(&self) {
        // Should the thread end by a panic all the same, the builds still
        // waiting are dropped unrun and the next one queued starts a new
        // thread.
        let _exit = Exit(self);
        while let Some(mut job) = self.next() {
            let kind = job.kind;
            let ran = {
                let _at = machine::at(kind.priority());
                // A panicking build must not end the thread.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (job.work)(kind)))
            };
            let mut q = lock(&self.queues);
            // A request may have made it a requested build meanwhile.
            job.kind = q.running.take().map_or(job.kind, |r| r.kind);
            match ran {
                Ok(Ran::Done) => {}
                Ok(Ran::Stopped) => {
                    job.progress.again();
                    match job.kind {
                        Kind::Requested => q.requested.push_back(job),
                        Kind::Background => q.background.push_front(job),
                    }
                }
                Err(_) => crate::log!("an index build failed with a bug"),
            }
        }
    }
}

/// Stops the background build running when it is of another database than
/// `id`, which a request waits for.
fn preempt(q: &Queues, id: &str) {
    if let Some(running) = q.running.as_ref().filter(|r| r.kind == Kind::Background && r.id != id) {
        running.progress.ask_stop();
    }
}

struct Exit<'a>(&'a Scheduler);

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let jobs = {
                let mut q = lock(&self.0.queues);
                q.thread = false;
                q.running = None;
                (std::mem::take(&mut q.requested), std::mem::take(&mut q.background))
            };
            drop(jobs);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;

    use super::*;

    /// The priority of a background build, in whichever mode the variable
    /// chose ([`machine::BACKGROUND_MODE`]).
    fn background_priority() -> Priority {
        Kind::Background.priority()
    }

    /// A computer whose power the test sets.
    #[derive(Default)]
    struct Power(AtomicBool);

    impl Machine for Power {
        fn on_battery(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }

        fn free_bytes(&self, _: &Path) -> Option<u64> {
            None
        }
    }

    /// A build that tells `events` of each run, `(id, priority, "run")`, and
    /// how it ended: it stops once asked to, and else ends once `release`
    /// sends, or at once without one.
    fn build(
        id: &'static str,
        events: &mpsc::Sender<(&'static str, Priority, &'static str)>,
        progress: &Arc<Progress>,
        release: Option<mpsc::Receiver<()>>,
    ) -> Work {
        let (events, progress) = (events.clone(), Arc::clone(progress));
        Box::new(move |_| {
            let _ = events.send((id, machine::current(), "run"));
            loop {
                if progress.stopped() {
                    let _ = events.send((id, machine::current(), "stopped"));
                    return Ran::Stopped;
                }
                match &release {
                    Some(r) if r.recv_timeout(Duration::from_millis(5)).is_err() => continue,
                    _ => break,
                }
            }
            let _ = events.send((id, machine::current(), "done"));
            Ran::Done
        })
    }

    fn next(events: &mpsc::Receiver<(&'static str, Priority, &'static str)>) -> (&'static str, Priority, &'static str) {
        events.recv_timeout(Duration::from_secs(10)).expect("the builds go on")
    }

    /// A requested build stops the background build of another database
    /// running at its next batch and goes first; the stopped one waits again,
    /// then runs to its end. Each runs at its kind's priority.
    #[test]
    fn a_requested_build_goes_before_a_background_one() {
        let scheduler = Arc::new(Scheduler::default());
        let (tx, events) = mpsc::channel();
        let (release_background, held) = mpsc::channel();
        let background = Arc::new(Progress::default());
        assert!(scheduler.submit(
            "a",
            Kind::Background,
            Arc::clone(&background),
            build("a", &tx, &background, Some(held))
        ));
        assert_eq!(next(&events), ("a", background_priority(), "run"));
        let requested = Arc::new(Progress::default());
        assert!(scheduler.submit("b", Kind::Requested, Arc::clone(&requested), build("b", &tx, &requested, None)));
        assert_eq!(next(&events), ("a", background_priority(), "stopped"));
        assert_eq!(next(&events), ("b", Priority::BelowNormal, "run"));
        assert_eq!(next(&events), ("b", Priority::BelowNormal, "done"));
        // The stopped build starts again, from nothing, at its own priority.
        assert_eq!(next(&events), ("a", background_priority(), "run"));
        assert!(!background.stopped());
        release_background.send(()).unwrap();
        assert_eq!(next(&events), ("a", background_priority(), "done"));
    }

    /// A request for a database whose build waits in the background makes it
    /// a requested one, which goes first; one whose background build runs
    /// raises its priority, and another request no longer stops it.
    #[test]
    fn a_request_promotes_the_background_build_of_its_database() {
        let scheduler = Arc::new(Scheduler::default());
        let (tx, events) = mpsc::channel();
        let (release_a, held_a) = mpsc::channel();
        let a = Arc::new(Progress::default());
        assert!(scheduler.submit("a", Kind::Background, Arc::clone(&a), build("a", &tx, &a, Some(held_a))));
        assert_eq!(next(&events), ("a", background_priority(), "run"));
        let (release_b, held_b) = mpsc::channel();
        let b = Arc::new(Progress::default());
        assert!(scheduler.submit("b", Kind::Background, Arc::clone(&b), build("b", &tx, &b, Some(held_b))));
        let c = Arc::new(Progress::default());
        assert!(scheduler.submit("c", Kind::Background, Arc::clone(&c), build("c", &tx, &c, None)));
        // A request for `a`, running: it goes on, at a requested priority.
        scheduler.promote("a");
        assert_eq!(Priority::from_code(a.priority.load(Ordering::Relaxed)), Priority::BelowNormal);
        // A request for `c`, waiting: it goes before `b`, after `a`, which
        // another request no longer stops.
        scheduler.promote("c");
        std::thread::sleep(Duration::from_millis(50));
        assert!(!a.stopped());
        release_a.send(()).unwrap();
        assert_eq!(next(&events), ("a", Priority::BelowNormal, "done"));
        assert_eq!(next(&events), ("c", Priority::BelowNormal, "run"));
        assert_eq!(next(&events), ("c", Priority::BelowNormal, "done"));
        assert_eq!(next(&events), ("b", background_priority(), "run"));
        release_b.send(()).unwrap();
        assert_eq!(next(&events), ("b", background_priority(), "done"));
    }

    /// A background build waits while the computer runs on battery, and one
    /// running then stops and waits; a requested build does not wait. Back
    /// on mains power, the waiting build runs.
    #[test]
    fn background_builds_wait_on_battery() {
        let scheduler = Arc::new(Scheduler::default());
        let power = Arc::new(Power::default());
        scheduler.set_machine(Arc::clone(&power) as Arc<dyn Machine>);
        let (tx, events) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let a = Arc::new(Progress::default());
        assert!(scheduler.submit("a", Kind::Background, Arc::clone(&a), build("a", &tx, &a, Some(held))));
        assert_eq!(next(&events), ("a", background_priority(), "run"));
        power.0.store(true, Ordering::Relaxed);
        scheduler.pause();
        assert_eq!(next(&events), ("a", background_priority(), "stopped"));
        let b = Arc::new(Progress::default());
        assert!(scheduler.submit("b", Kind::Requested, Arc::clone(&b), build("b", &tx, &b, None)));
        assert_eq!(next(&events), ("b", Priority::BelowNormal, "run"));
        assert_eq!(next(&events), ("b", Priority::BelowNormal, "done"));
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err(), "a background build ran on battery");
        assert_eq!(a.phase(), "waiting");
        power.0.store(false, Ordering::Relaxed);
        scheduler.poke();
        assert_eq!(next(&events), ("a", background_priority(), "run"));
        release.send(()).unwrap();
        assert_eq!(next(&events), ("a", background_priority(), "done"));
    }

    /// A background build gives way to foreground work for the scheduler's
    /// patience from its start, a requested build never, and a background
    /// build that a request promotes while it runs no longer (#149).
    #[test]
    fn only_a_background_build_gives_way() {
        let scheduler = Arc::new(Scheduler::default());
        assert_eq!(scheduler.patience(), PATIENCE);
        let patience = Duration::from_millis(70);
        scheduler.set_patience(patience);
        let (tx, events) = mpsc::channel();
        let (release_a, held_a) = mpsc::channel();
        let a = Arc::new(Progress::default());
        assert_eq!(a.patience(), Duration::ZERO, "a build not yet run");
        assert!(scheduler.submit("a", Kind::Background, Arc::clone(&a), build("a", &tx, &a, Some(held_a))));
        assert_eq!(next(&events), ("a", background_priority(), "run"));
        assert_eq!(a.patience(), patience);
        scheduler.promote("a");
        assert_eq!(a.patience(), Duration::ZERO);
        // Its thread takes the requested priority at its next batch.
        std::thread::sleep(Duration::from_millis(50));
        release_a.send(()).unwrap();
        assert_eq!(next(&events), ("a", Priority::BelowNormal, "done"));
        let (release_b, held_b) = mpsc::channel();
        let b = Arc::new(Progress::default());
        b.set_patience(PATIENCE);
        assert!(scheduler.submit("b", Kind::Requested, Arc::clone(&b), build("b", &tx, &b, Some(held_b))));
        assert_eq!(next(&events), ("b", Priority::BelowNormal, "run"));
        assert_eq!(b.patience(), Duration::ZERO);
        release_b.send(()).unwrap();
        assert_eq!(next(&events), ("b", Priority::BelowNormal, "done"));
    }

    /// A thread that cannot start leaves no build waiting, and the next
    /// build queued starts one.
    #[test]
    fn a_failed_start_leaves_nothing_waiting() {
        let scheduler = Arc::new(Scheduler::default());
        let (tx, events) = mpsc::channel();
        let p = Arc::new(Progress::default());
        scheduler.refuse_starts(true);
        assert!(!scheduler.submit("a", Kind::Background, Arc::clone(&p), build("a", &tx, &p, None)));
        assert!(!scheduler.submit("b", Kind::Requested, Arc::clone(&p), build("b", &tx, &p, None)));
        {
            let q = lock(&scheduler.queues);
            assert!(q.requested.is_empty() && q.background.is_empty() && !q.thread);
        }
        scheduler.refuse_starts(false);
        assert!(scheduler.submit("c", Kind::Background, Arc::clone(&p), build("c", &tx, &p, None)));
        assert_eq!(next(&events), ("c", background_priority(), "run"));
        assert_eq!(next(&events), ("c", background_priority(), "done"));
    }
}
