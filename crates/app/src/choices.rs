//! The order of engine choices (#54). A choice in the settings window takes
//! effect at once; installing Stockfish takes minutes, and chooses the build
//! it installed only if the user chose nothing else while it ran. The build
//! stays in the engine list either way.
//!
//! Also the offer of the newest Stockfish build in place of an older chosen
//! one, putting it off until the next bridge version, and the engine
//! section's view, whose names and numbers are made here so that the window
//! only shows them. The settings window's commands hand in what needs
//! Windows, the installation and the engine's probe, so that all of this is
//! tested on every system.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use bridge::config;
use bridge::engines::Found;
use bridge::stockfish::{self, Build, Progress};
use serde::Serialize;

use crate::prefs;

/// The engine choices and Stockfish installations of this process.
pub struct Choices {
    /// Counts the choices saved.
    saved: AtomicU64,
    /// One choice at a time: a slow probe cannot save its engine over a later one.
    choosing: Mutex<()>,
    /// One installation at a time. An installation that panics leaves it
    /// poisoned, not held: the next one takes it all the same.
    installing: Mutex<()>,
}

/// The count when a longer operation began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ticket(u64);

impl Default for Choices {
    fn default() -> Self {
        Self::new()
    }
}

impl Choices {
    pub const fn new() -> Self {
        Choices { saved: AtomicU64::new(0), choosing: Mutex::new(()), installing: Mutex::new(()) }
    }

    /// Whether Stockfish is being installed now (#61): an update waits for it.
    pub fn installing(&self) -> bool {
        matches!(self.installing.try_lock(), Err(TryLockError::WouldBlock))
    }

    /// Chooses `engine` in the `bridge.toml` at `config_path` once `probe`
    /// accepts it; a refusal is answered as `probe` gives it, and nothing is
    /// saved.
    pub fn choose<E: From<String>>(
        &self,
        config_path: &Path,
        engine: PathBuf,
        probe: impl FnOnce(&Path) -> Result<(), E>,
    ) -> Result<(), E> {
        let _one = self.choosing.lock().unwrap_or_else(PoisonError::into_inner);
        probe(&engine)?;
        Ok(self.save(config_path, engine)?)
    }

    /// Installs Stockfish with `install`, which answers the executable, then
    /// chooses it once `probe` accepts it, unless an engine was chosen while
    /// it ran; whether it was chosen. One installation at a time. The
    /// installation holds no lock another choice waits on: only the probe and
    /// the save do.
    pub fn install(
        &self,
        config_path: &Path,
        install: impl FnOnce() -> Result<PathBuf, String>,
        probe: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<bool, String> {
        let _installing = self.begin_installing()?;
        let ticket = self.ticket();
        let exe = install()?;
        self.choose_installed(ticket, config_path, exe, probe, || Some(()))
    }

    /// Installs as [`Choices::install`] does, then waits with `wait` before
    /// the probe and the choice: an automatic update waits there until the
    /// bridge is idle, as choosing another engine stops a running analysis
    /// (#322). The installation no longer counts as running while it waits,
    /// so neither an update of the bridge nor the user's own installation
    /// waits for that; a choice made meanwhile stands. `wait` answering false
    /// gives the choice up: nothing is chosen, and the build stays listed.
    /// After the probe, `confirm` gives the choice up the same way by
    /// answering `None`; what it answers otherwise is held while the choice
    /// is saved, such as the preferences' lock ([`crate::prefs::hold_if`]),
    /// so that a probe of seconds cannot outlast a reason to give up.
    pub fn update<G>(
        &self,
        config_path: &Path,
        install: impl FnOnce() -> Result<PathBuf, String>,
        wait: impl FnOnce() -> bool,
        probe: impl FnOnce(&Path) -> Result<(), String>,
        confirm: impl FnOnce() -> Option<G>,
    ) -> Result<bool, String> {
        let (ticket, exe) = {
            let _installing = self.begin_installing()?;
            (self.ticket(), install()?)
        };
        if !wait() {
            return Ok(false);
        }
        self.choose_installed(ticket, config_path, exe, probe, confirm)
    }

    /// Chooses the build `exe` an installation that began at `ticket`
    /// installed, once `probe` accepts it and `confirm` answers, unless an
    /// engine was chosen since.
    fn choose_installed<G>(
        &self,
        ticket: Ticket,
        config_path: &Path,
        exe: PathBuf,
        probe: impl FnOnce(&Path) -> Result<(), String>,
        confirm: impl FnOnce() -> Option<G>,
    ) -> Result<bool, String> {
        let _one = self.choosing.lock().unwrap_or_else(PoisonError::into_inner);
        probe(&exe)?;
        let Some(_confirmed) = confirm() else { return Ok(false) };
        // A choice made while the download ran stands; the build stays listed.
        self.save_if_current(ticket, config_path, exe)
    }

    /// Runs `settled` while no installation runs and no engine is being
    /// chosen, and holds both off until it ends: the removal of older builds
    /// reads the choice and removes folders while neither can change (#322).
    /// `None`, with `settled` not run, while an installation runs.
    pub fn while_settled<R>(&self, settled: impl FnOnce() -> R) -> Option<R> {
        let _installing = self.begin_installing().ok()?;
        let _one = self.choosing.lock().unwrap_or_else(PoisonError::into_inner);
        Some(settled())
    }

    /// Holds the one installation, or refuses a second.
    fn begin_installing(&self) -> Result<MutexGuard<'_, ()>, String> {
        match self.installing.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::Poisoned(e)) => Ok(e.into_inner()),
            Err(TryLockError::WouldBlock) => Err("Stockfish is being installed already".into()),
        }
    }

    /// Taken when an operation that may choose later begins.
    fn ticket(&self) -> Ticket {
        Ticket(self.saved.load(Ordering::SeqCst))
    }

    /// Saves `engine` as the choice in the `bridge.toml` at `config_path`, now.
    fn save(&self, config_path: &Path, engine: PathBuf) -> Result<(), String> {
        config::update(config_path, |c| config::Config { engine: Some(engine), ..c.clone() })?;
        self.saved.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Saves `engine` as the choice only if nothing was chosen since `ticket`;
    /// whether it was saved. The caller holds the lock every choice takes.
    fn save_if_current(&self, ticket: Ticket, config_path: &Path, engine: PathBuf) -> Result<bool, String> {
        if self.saved.load(Ordering::SeqCst) != ticket.0 {
            return Ok(false);
        }
        self.save(config_path, engine)?;
        Ok(true)
    }
}

/// The name of the `chosen` engine when the engine section offers the
/// `newest` Stockfish build in its place: the chosen engine is an older
/// Stockfish ([`stockfish::offer`]), that build is not installed in the data
/// folder `data` (it would be in the list already), and the offer was not put
/// off for the `running` bridge version. The chosen engine is named as
/// `name_of` names it.
pub fn offer_for(data: &Path, chosen: Option<&Path>, found: &[Found], running: &str, newest: &Build) -> Option<String> {
    let chosen = chosen?;
    let name = name_of(chosen, found);
    if !stockfish::offer(newest, Some((chosen, &name))) {
        return None;
    }
    let dismissed = prefs::load(data).stockfish_offer_dismissed.as_deref() == Some(running);
    (!stockfish::is_installed(data, newest) && !dismissed).then_some(name)
}

/// Whether the first-run wizard recommends installing the `newest` Stockfish
/// build beside the engines `found` (#287): none of them, nor the `chosen`
/// engine, is a Stockfish of that major version or newer
/// ([`stockfish::is_current`]). A build the bridge installed is listed by its
/// version, so an installed newest build ends the recommendation.
pub fn recommend_install(chosen: Option<&Path>, found: &[Found], newest: &Build) -> bool {
    let chosen_current = chosen.is_some_and(|c| stockfish::is_current(newest, c, &name_of(c, found)));
    !chosen_current && !found.iter().any(|f| stockfish::is_current(newest, &f.path, &f.name))
}

/// The name of the `chosen` engine: as `found` names it, else after its file
/// without `.exe`, as the list names an engine by its file.
fn name_of(chosen: &Path, found: &[Found]) -> String {
    match found.iter().find(|f| f.path.as_os_str() == chosen.as_os_str()) {
        Some(f) => f.name.clone(),
        None => {
            let path = chosen.to_string_lossy();
            path.rsplit(['\\', '/']).next().unwrap_or(&path).trim_end_matches(".exe").to_string()
        }
    }
}

/// Puts off the Stockfish offer until the next bridge version: notes in the
/// data folder `data` that the offer of the `running` version was dismissed.
pub fn dismiss_offer(data: &Path, running: &str) -> Result<(), String> {
    prefs::update(data, |prefs| prefs.stockfish_offer_dismissed = Some(running.into()))
}

/// What the engine section shows: the engines found and the one chosen, the
/// newest official build the bridge knows of and can install, and whether to
/// offer it instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnginesView {
    /// The engine `bridge.toml` names, if any.
    chosen: Option<String>,
    /// The chosen engine's name, as the list names it, else after its file.
    chosen_name: Option<String>,
    found: Vec<FoundEngine>,
    install: Installable,
    /// The chosen engine's name when it is an older Stockfish and the offer
    /// was not put off for this bridge version.
    offer_for: Option<String>,
    /// Whether the first-run wizard recommends installing the pinned build
    /// ([`recommend_install`]).
    recommend_install: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Installable {
    version: String,
    /// Its size in whole megabytes ([`stockfish::megabytes`]).
    megabytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FoundEngine {
    name: String,
    path: String,
    source: &'static str,
    /// For a build the bridge installed: its version, whose licence the
    /// window can open.
    version: Option<String>,
}

impl EnginesView {
    /// The engine section for the engines `found` and the `chosen` one, with
    /// the `newest` build the bridge knows of and the offer as [`offer_for`]
    /// makes it for the data folder `data` and the `running` bridge version.
    pub fn new(data: &Path, chosen: Option<&Path>, found: Vec<Found>, running: &str, newest: &Build) -> EnginesView {
        EnginesView {
            chosen: chosen.map(|p| p.to_string_lossy().into_owned()),
            chosen_name: chosen.map(|p| name_of(p, &found)),
            offer_for: offer_for(data, chosen, &found, running, newest),
            recommend_install: recommend_install(chosen, &found, newest),
            install: Installable { version: newest.version.to_string(), megabytes: newest.megabytes() },
            found: found
                .into_iter()
                .map(|f| FoundEngine {
                    name: f.name,
                    path: f.path.to_string_lossy().into_owned(),
                    source: f.source,
                    version: f.version,
                })
                .collect(),
        }
    }
}

/// An installation's progress, for the settings window: a download counts in
/// whole megabytes, rounded as the build's size is ([`stockfish::megabytes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallProgress {
    phase: &'static str,
    done_megabytes: u64,
    total_megabytes: u64,
}

impl From<Progress> for InstallProgress {
    fn from(progress: Progress) -> InstallProgress {
        let (phase, done, total) = match progress {
            Progress::Downloading { done, total } => ("downloading", done, total),
            Progress::Checking => ("checking", 0, 0),
            Progress::Unpacking => ("unpacking", 0, 0),
        };
        InstallProgress {
            phase,
            done_megabytes: stockfish::megabytes(done),
            total_megabytes: stockfish::megabytes(total),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::mpsc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::settings::Failure;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bridge-app-choices-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn engine_in(config_path: &Path) -> Option<PathBuf> {
        config::load_or_create(config_path).unwrap().engine
    }

    fn accept(_: &Path) -> Result<(), String> {
        Ok(())
    }

    /// The pinned build for this computer: the newest the bridge knows of
    /// before a lookup.
    fn pinned() -> &'static Build {
        Build::for_arch(stockfish::machine_arch())
    }

    #[test]
    fn an_installation_does_not_replace_a_choice_made_while_it_ran() {
        let dir = folder("tickets");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();

        // Installing begins; the user chooses Lc0 meanwhile; the installation ends.
        let installing = choices.ticket();
        choices.save(&toml, PathBuf::from("lc0.exe")).unwrap();
        assert!(!choices.save_if_current(installing, &toml, PathBuf::from("stockfish.exe")).unwrap());
        assert_eq!(engine_in(&toml), Some(PathBuf::from("lc0.exe")));

        // With no choice in between, the installed build is chosen.
        let installing = choices.ticket();
        assert!(choices.save_if_current(installing, &toml, PathBuf::from("stockfish.exe")).unwrap());
        assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish.exe")));

        // An installation's own choice counts too: an older one that ends later
        // does not take it back.
        let older = choices.ticket();
        let newer = choices.ticket();
        assert!(choices.save_if_current(newer, &toml, PathBuf::from("stockfish-20.exe")).unwrap());
        assert!(!choices.save_if_current(older, &toml, PathBuf::from("stockfish-19.exe")).unwrap());
        assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish-20.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An installation installs, then probes what it installed, then chooses
    /// it; while it runs, Stockfish is being installed.
    #[test]
    fn an_installation_installs_then_probes_then_chooses() {
        let dir = folder("install");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();
        let events = RefCell::new(Vec::new());
        let chosen = choices.install(
            &toml,
            || {
                events.borrow_mut().push(format!("install, installing {}", choices.installing()));
                Ok(PathBuf::from("stockfish.exe"))
            },
            |exe| {
                events.borrow_mut().push(format!("probe {}", exe.display()));
                Ok(())
            },
        );
        assert_eq!(chosen, Ok(true));
        assert_eq!(*events.borrow(), ["install, installing true", "probe stockfish.exe"]);
        assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish.exe")));
        assert!(!choices.installing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An update waits after the installation, which no longer counts as
    /// running then, and before the probe and the choice (#322); a choice the
    /// user makes while it waits stands.
    #[test]
    fn an_update_waits_between_the_installation_and_the_choice() {
        let dir = folder("update");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();
        let events = RefCell::new(Vec::new());
        let chosen = choices.update(
            &toml,
            || {
                events.borrow_mut().push(format!("install, installing {}", choices.installing()));
                Ok(PathBuf::from("stockfish-20.exe"))
            },
            || {
                events.borrow_mut().push(format!("wait, installing {}", choices.installing()));
                true
            },
            |exe| {
                events.borrow_mut().push(format!("probe {}", exe.display()));
                Ok(())
            },
            || Some(()),
        );
        assert_eq!(chosen, Ok(true));
        assert_eq!(*events.borrow(), ["install, installing true", "wait, installing false", "probe stockfish-20.exe"]);
        assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish-20.exe")));

        // The user chooses Lc0 while the update waits for the bridge to be idle.
        let chosen = choices.update(
            &toml,
            || Ok(PathBuf::from("stockfish-21.exe")),
            || {
                choices.choose(&toml, PathBuf::from("lc0.exe"), accept).unwrap();
                true
            },
            accept,
            || Some(()),
        );
        assert_eq!(chosen, Ok(false));
        assert_eq!(engine_in(&toml), Some(PathBuf::from("lc0.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wait that gives the update up chooses nothing; the installation
    /// still happened, and the next one starts.
    #[test]
    fn an_update_given_up_while_it_waits_chooses_nothing() {
        let dir = folder("given-up");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();
        let probed = RefCell::new(false);
        let chosen = choices.update(
            &toml,
            || Ok(PathBuf::from("stockfish-20.exe")),
            || false,
            |_| {
                *probed.borrow_mut() = true;
                Ok(())
            },
            || Some(()),
        );
        assert_eq!(chosen, Ok(false));
        assert!(!*probed.borrow(), "nothing is probed either");
        assert_eq!(engine_in(&toml), None);
        assert_eq!(choices.install(&toml, || Ok(PathBuf::from("stockfish.exe")), accept), Ok(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// After the probe, a refused confirmation chooses nothing; a granted one
    /// is held while the choice is saved (#322 review).
    #[test]
    fn an_update_confirms_after_the_probe() {
        let dir = folder("confirm");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();
        let events = RefCell::new(Vec::new());
        let refused = choices.update(
            &toml,
            || Ok(PathBuf::from("stockfish-20.exe")),
            || true,
            |_| {
                events.borrow_mut().push("probe");
                Ok(())
            },
            || {
                events.borrow_mut().push("confirm");
                None::<()>
            },
        );
        assert_eq!(refused, Ok(false));
        assert_eq!(*events.borrow(), ["probe", "confirm"]);
        assert_eq!(engine_in(&toml), None);
        struct Saved<'a>(&'a Path, &'a RefCell<Vec<&'static str>>);
        impl Drop for Saved<'_> {
            fn drop(&mut self) {
                assert!(engine_in(self.0).is_some(), "held until the choice is saved");
                self.1.borrow_mut().push("released");
            }
        }
        let granted = choices.update(
            &toml,
            || Ok(PathBuf::from("stockfish-20.exe")),
            || true,
            accept,
            || Some(Saved(&toml, &events)),
        );
        assert_eq!(granted, Ok(true));
        assert_eq!(events.borrow().last(), Some(&"released"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manual installation counts as running until its build is chosen:
    /// an update of the bridge waits through the probe too (#61).
    #[test]
    fn an_installation_runs_until_its_build_is_chosen() {
        let dir = folder("runs");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();
        let during_probe = RefCell::new(None);
        let chosen = choices.install(
            &toml,
            || Ok(PathBuf::from("stockfish.exe")),
            |_| {
                *during_probe.borrow_mut() = Some(choices.installing());
                Ok(())
            },
        );
        assert_eq!(chosen, Ok(true));
        assert_eq!(*during_probe.borrow(), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Settled work runs only with no installation running, and holds off
    /// installations and choices until it ends (#322).
    #[test]
    fn settled_work_holds_off_installations_and_choices() {
        static CHOICES: Choices = Choices::new();
        let dir = folder("settled");
        let toml = dir.join("bridge.toml");
        // Not while an installation runs.
        let inside = CHOICES.install(
            &toml,
            || {
                assert_eq!(CHOICES.while_settled(|| panic!("settled work runs during an installation")), None);
                Ok(PathBuf::from("stockfish.exe"))
            },
            accept,
        );
        assert_eq!(inside, Ok(true));
        // A choice made while it runs waits for it; an installation is refused.
        let (done_tx, done) = mpsc::channel();
        let settled = CHOICES.while_settled(|| {
            let path = toml.clone();
            std::thread::spawn(move || done_tx.send(CHOICES.choose(&path, PathBuf::from("lc0.exe"), accept)));
            assert!(done.recv_timeout(Duration::from_millis(200)).is_err(), "the choice waits");
            assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish.exe")));
            assert_eq!(
                CHOICES.install(&toml, || panic!("an installation runs"), accept),
                Err("Stockfish is being installed already".into())
            );
            "settled"
        });
        assert_eq!(settled, Some("settled"));
        assert_eq!(done.recv_timeout(crate::PATIENCE).expect("then it is saved"), Ok(()));
        assert_eq!(engine_in(&toml), Some(PathBuf::from("lc0.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An installation that panics leaves no installation running: the next
    /// one starts, and is the only one while it runs.
    #[test]
    fn an_installation_that_panicked_leaves_the_next_free_to_start() {
        let dir = folder("panicked");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            choices.install(&toml, || panic!("the installation panics"), accept)
        }));
        assert!(panicked.is_err());
        assert!(choices.installing.is_poisoned());
        assert!(!choices.installing(), "a panicked installation still counts as running");
        let chosen = choices.install(
            &toml,
            || {
                assert!(choices.installing());
                let second = choices.install(&toml, || panic!("a second installation runs"), accept);
                assert_eq!(second, Err("Stockfish is being installed already".into()));
                Ok(PathBuf::from("stockfish.exe"))
            },
            accept,
        );
        assert_eq!(chosen, Ok(true));
        assert_eq!(engine_in(&toml), Some(PathBuf::from("stockfish.exe")));
        assert!(!choices.installing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The user chooses Lc0 while Stockfish downloads: the choice does not
    /// wait for the download, and it stands when the installation ends. The
    /// installed build is still probed, and stays listed.
    #[test]
    fn a_choice_made_during_an_installation_wins() {
        static CHOICES: Choices = Choices::new();
        let dir = folder("during");
        let toml = dir.join("bridge.toml");
        let probed = RefCell::new(Vec::new());
        let chosen = CHOICES.install(
            &toml,
            || {
                let (tx, rx) = mpsc::channel();
                let toml = toml.clone();
                std::thread::spawn(move || tx.send(CHOICES.choose(&toml, PathBuf::from("lc0.exe"), accept)));
                rx.recv_timeout(crate::PATIENCE).expect("a choice does not wait for an installation")?;
                Ok(PathBuf::from("stockfish.exe"))
            },
            |exe| {
                probed.borrow_mut().push(exe.to_path_buf());
                Ok(())
            },
        );
        assert_eq!(chosen, Ok(false));
        assert_eq!(engine_in(&toml), Some(PathBuf::from("lc0.exe")));
        assert_eq!(*probed.borrow(), [PathBuf::from("stockfish.exe")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One choice at a time: a choice made while an earlier one's probe runs
    /// waits for it, and so is saved after it, as the later choice.
    #[test]
    fn a_slow_probe_does_not_save_its_engine_over_a_later_choice() {
        static CHOICES: Choices = Choices::new();
        let dir = folder("slow");
        let toml = dir.join("bridge.toml");
        let (probing_tx, probing) = mpsc::channel();
        let (go, go_rx) = mpsc::channel::<()>();
        let slow = {
            let toml = toml.clone();
            std::thread::spawn(move || {
                CHOICES.choose(&toml, PathBuf::from("slow.exe"), |_| {
                    probing_tx.send(()).unwrap();
                    go_rx.recv().map_err(|e| e.to_string())
                })
            })
        };
        probing.recv_timeout(crate::PATIENCE).expect("the slow probe runs");
        let (done_tx, done) = mpsc::channel();
        let later = {
            let toml = toml.clone();
            std::thread::spawn(move || done_tx.send(CHOICES.choose(&toml, PathBuf::from("later.exe"), accept)))
        };
        assert!(done.recv_timeout(Duration::from_millis(200)).is_err(), "the later choice waits for the probe");
        go.send(()).unwrap();
        assert_eq!(slow.join().unwrap(), Ok(()));
        assert_eq!(done.recv_timeout(crate::PATIENCE).expect("then it is saved"), Ok(()));
        later.join().unwrap().unwrap();
        assert_eq!(engine_in(&toml), Some(PathBuf::from("later.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One installation at a time. An installation that fails, or whose build
    /// does not answer, chooses nothing and leaves the next free to start; an
    /// engine the probe refuses is not chosen either, and the refusal is
    /// answered as the probe gives it, while a choice that cannot be saved
    /// fails with the general message.
    #[test]
    fn a_failed_installation_or_a_refused_engine_chooses_nothing() {
        let dir = folder("failed");
        let toml = dir.join("bridge.toml");
        let choices = Choices::new();
        let failed = choices.install(
            &toml,
            || {
                let second = choices.install(&toml, || panic!("a second installation runs"), accept);
                assert_eq!(second, Err("Stockfish is being installed already".into()));
                Err("The download failed (exit code: 22)".into())
            },
            accept,
        );
        assert_eq!(failed, Err("The download failed (exit code: 22)".into()));
        assert!(!choices.installing());
        let silent = choices.install(&toml, || Ok(PathBuf::from("stockfish.exe")), |_| Err("no uciok".into()));
        assert_eq!(silent, Err("no uciok".into()));
        let refused =
            choices.choose(&toml, PathBuf::from("notes.txt"), |_| Err(Failure::new("settings.engine.refused")));
        assert_eq!(refused, Err(Failure::new("settings.engine.refused")));
        assert_eq!(engine_in(&toml), None);
        let unsaved = choices.choose(&dir, PathBuf::from("lc0.exe"), |_| Ok::<(), Failure>(()));
        assert_eq!(unsaved.map_err(|f| f.key), Err("settings.error"), "the folder is no bridge.toml");
        assert!(!choices.installing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn listed(name: &str, path: &str) -> Found {
        Found { name: name.into(), path: PathBuf::from(path), source: "ChessBase", version: None }
    }

    /// The pinned build is offered in place of an older chosen Stockfish,
    /// which is named as the list names it, else after its file; not in place
    /// of another engine or the pinned version, and not once the pinned build
    /// is installed.
    #[test]
    fn offers_the_pinned_build_in_place_of_an_older_stockfish() {
        let dir = folder("offer");
        let found = [
            listed("Stockfish 17.1", r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe"),
            listed("Lc0 0.31", r"C:\lc0\lc0.exe"),
        ];
        let offer = |chosen: Option<&str>| offer_for(&dir, chosen.map(Path::new), &found, "1.2.1", pinned());
        assert_eq!(offer(Some(r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe")).as_deref(), Some("Stockfish 17.1"));
        assert_eq!(offer(Some(r"C:\x\stockfish_16_x64.exe")).as_deref(), Some("stockfish_16_x64"), "not listed");
        assert_eq!(offer(Some(r"C:\lc0\lc0.exe")), None);
        assert_eq!(offer(Some(r"C:\x\stockfish-19\stockfish.exe")), None, "the pinned version");
        assert_eq!(offer(None), None);

        let build = stockfish::Build::for_arch(stockfish::machine_arch());
        for file in [build.installed(&dir), build.dir(&dir).join(stockfish::LICENCE)] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"MZ").unwrap();
        }
        assert_eq!(offer(Some(r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe")), None, "installed, it is listed");
        // A newer release that a lookup named is offered in place of the pinned build (#322).
        let newer = Build::released(stockfish::machine_arch(), "20", 1, &"a".repeat(64)).unwrap();
        let chosen = Some(Path::new(r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe"));
        assert_eq!(offer_for(&dir, chosen, &found, "1.2.1", &newer).as_deref(), Some("Stockfish 17.1"));
        let installed_19 = build.installed(&dir);
        assert_eq!(
            offer_for(&dir, Some(&installed_19), &found, "1.2.1", &newer).as_deref(),
            Some(build.exe.trim_end_matches(".exe"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wizard recommends the pinned build beside older Stockfish builds and
    /// other engines, found or chosen by file (#287), and not once an engine
    /// found or chosen is that version or newer, as the build the bridge
    /// installed is.
    #[test]
    fn recommends_the_pinned_build_until_a_current_stockfish_is_found_or_chosen() {
        let older = [
            listed("Stockfish 17.1", r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe"),
            listed("Lc0 0.31", r"C:\lc0\lc0.exe"),
        ];
        assert!(recommend_install(None, &older, pinned()));
        assert!(recommend_install(None, &[], pinned()), "nothing found");
        assert!(recommend_install(Some(Path::new(r"C:\lc0\lc0.exe")), &older, pinned()));
        assert!(recommend_install(Some(Path::new(r"C:\x\stockfish.exe")), &older, pinned()), "a version nobody names");
        assert!(
            !recommend_install(Some(Path::new(r"C:\x\stockfish_19_x64.exe")), &older, pinned()),
            "chosen by its file"
        );

        let dir = folder("recommend");
        let build = Build::for_arch(stockfish::machine_arch());
        for file in [build.installed(&dir), build.dir(&dir).join(stockfish::LICENCE)] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"MZ").unwrap();
        }
        let roots = bridge::engines::Roots { bridge_data: Some(dir.clone()), ..Default::default() };
        let mut with_installed = bridge::engines::find(&roots);
        assert_eq!(with_installed.len(), 1, "the installed build is listed");
        with_installed.extend(older);
        assert!(!recommend_install(None, &with_installed, pinned()));
        let view = serde_json::to_value(EnginesView::new(&dir, None, with_installed, "1.2.1", pinned())).unwrap();
        assert_eq!(view["recommendInstall"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// «Later» puts the offer off for this bridge version: it stays put off
    /// when the preferences are saved again or another older Stockfish is
    /// chosen, and the next bridge version offers again.
    #[test]
    fn a_dismissed_offer_stays_dismissed_until_the_next_version() {
        let dir = folder("dismissed");
        let found = [listed("Stockfish 17.1", r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe")];
        let chosen = Some(Path::new(r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe"));
        assert!(offer_for(&dir, chosen, &found, "1.2.1", pinned()).is_some());
        dismiss_offer(&dir, "1.2.1").unwrap();
        assert_eq!(offer_for(&dir, chosen, &found, "1.2.1", pinned()), None);
        // «Update automatically» turned off saves the preferences again.
        prefs::save(&dir, &prefs::Prefs { auto_update: false, ..prefs::load(&dir) }).unwrap();
        assert_eq!(offer_for(&dir, chosen, &found, "1.2.1", pinned()), None);
        assert_eq!(offer_for(&dir, Some(Path::new(r"C:\x\stockfish_16_x64.exe")), &found, "1.2.1", pinned()), None);
        assert_eq!(
            offer_for(&dir, chosen, &found, "1.3.0", pinned()).as_deref(),
            Some("Stockfish 17.1"),
            "the next version"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The engine section as the window shows it (#186): the chosen engine
    /// named as the offer names it, also when it is not in the list; the
    /// build's size and a download's progress in whole megabytes, rounded up
    /// alike, so that the progress ends on the size the section named.
    #[test]
    fn the_engine_section_comes_named_and_counted() {
        let dir = folder("view");
        let found = vec![listed("Stockfish 17.1", r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe")];
        let view = |chosen: Option<&str>| {
            serde_json::to_value(EnginesView::new(&dir, chosen.map(Path::new), found.clone(), "1.2.1", pinned()))
                .unwrap()
        };
        let unlisted = view(Some(r"C:\x\stockfish_16_x64.exe"));
        assert_eq!(unlisted["chosen"], r"C:\x\stockfish_16_x64.exe");
        assert_eq!(unlisted["chosenName"], "stockfish_16_x64");
        assert_eq!(unlisted["offerFor"], unlisted["chosenName"], "the offer names it alike");
        let chosen = view(Some(r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe"));
        assert_eq!(chosen["chosenName"], "Stockfish 17.1");
        assert_eq!(
            chosen["found"],
            json!([{
                "name": "Stockfish 17.1",
                "path": r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe",
                "source": "ChessBase",
                "version": null,
            }])
        );
        let none = view(None);
        for field in ["chosen", "chosenName", "offerFor"] {
            assert!(none[field].is_null(), "{field}");
        }
        let build = Build::for_arch(stockfish::machine_arch());
        assert_eq!(none["install"], json!({ "version": build.version, "megabytes": build.megabytes() }));
        assert_eq!(none["recommendInstall"], true, "only an older Stockfish found");

        let progress = |p: Progress| serde_json::to_value(InstallProgress::from(p)).unwrap();
        for build in &stockfish::PINNED {
            let (start, end) = (
                progress(Progress::Downloading { done: 0, total: build.size }),
                progress(Progress::Downloading { done: build.size, total: build.size }),
            );
            assert_eq!(
                start,
                json!({ "phase": "downloading", "doneMegabytes": 0, "totalMegabytes": build.megabytes() })
            );
            assert_eq!(end["doneMegabytes"], build.megabytes(), "{:?}", build.arch);
        }
        // The ARM64 build's 76.48 MB are 77, as its size is shown, not 76.
        assert_eq!(progress(Progress::Downloading { done: 1, total: 80_190_536 })["totalMegabytes"], 77);
        assert_eq!(
            progress(Progress::Checking),
            json!({ "phase": "checking", "doneMegabytes": 0, "totalMegabytes": 0 })
        );
        assert_eq!(progress(Progress::Unpacking)["phase"], "unpacking");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
