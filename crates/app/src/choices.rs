//! The order of engine choices (#54). A choice in the settings window takes
//! effect at once; installing Stockfish takes minutes, and chooses the build
//! it installed only if the user chose nothing else while it ran. The build
//! stays in the engine list either way.
//!
//! Also the offer of the pinned Stockfish build in place of an older chosen
//! one, putting it off until the next bridge version, and the engine
//! section's view, whose names and numbers are made here so that the window
//! only shows them. The settings window's commands hand in what needs
//! Windows, the installation and the engine's probe, so that all of this is
//! tested on every system.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, TryLockError};

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
        let _installing = match self.installing.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return Err("Stockfish is being installed already".into()),
        };
        let ticket = self.ticket();
        let exe = install()?;
        let _one = self.choosing.lock().unwrap_or_else(PoisonError::into_inner);
        probe(&exe)?;
        // A choice made while the download ran stands; the build stays listed.
        self.save_if_current(ticket, config_path, exe)
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

/// The name of the `chosen` engine when the engine section offers the pinned
/// Stockfish build in its place: the chosen engine is an older Stockfish
/// ([`stockfish::offer`]), the pinned build is not installed in the data
/// folder `data` (it would be in the list already), and the offer was not put
/// off for the `running` bridge version. The chosen engine is named as
/// `name_of` names it.
pub fn offer_for(data: &Path, chosen: Option<&Path>, found: &[Found], running: &str) -> Option<String> {
    let chosen = chosen?;
    let name = name_of(chosen, found);
    let build = stockfish::offer(Some((chosen, &name)))?;
    let dismissed = prefs::load(data).stockfish_offer_dismissed.as_deref() == Some(running);
    (!stockfish::is_installed(data, build) && !dismissed).then_some(name)
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
    prefs::save(data, &prefs::Prefs { stockfish_offer_dismissed: Some(running.into()), ..prefs::load(data) })
}

/// What the engine section shows: the engines found and the one chosen, the
/// official build the bridge can install, and whether to offer it instead.
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Installable {
    version: &'static str,
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
    /// the offer as [`offer_for`] makes it for the data folder `data` and the
    /// `running` bridge version.
    pub fn new(data: &Path, chosen: Option<&Path>, found: Vec<Found>, running: &str) -> EnginesView {
        let build = Build::for_arch(stockfish::machine_arch());
        EnginesView {
            chosen: chosen.map(|p| p.to_string_lossy().into_owned()),
            chosen_name: chosen.map(|p| name_of(p, &found)),
            offer_for: offer_for(data, chosen, &found, running),
            install: Installable { version: build.version, megabytes: build.megabytes() },
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
                rx.recv_timeout(Duration::from_secs(10)).expect("a choice does not wait for an installation")?;
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
        probing.recv_timeout(Duration::from_secs(10)).expect("the slow probe runs");
        let (done_tx, done) = mpsc::channel();
        let later = {
            let toml = toml.clone();
            std::thread::spawn(move || done_tx.send(CHOICES.choose(&toml, PathBuf::from("later.exe"), accept)))
        };
        assert!(done.recv_timeout(Duration::from_millis(200)).is_err(), "the later choice waits for the probe");
        go.send(()).unwrap();
        assert_eq!(slow.join().unwrap(), Ok(()));
        assert_eq!(done.recv_timeout(Duration::from_secs(10)).expect("then it is saved"), Ok(()));
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
        let offer = |chosen: Option<&str>| offer_for(&dir, chosen.map(Path::new), &found, "1.2.1");
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
        assert!(offer_for(&dir, chosen, &found, "1.2.1").is_some());
        dismiss_offer(&dir, "1.2.1").unwrap();
        assert_eq!(offer_for(&dir, chosen, &found, "1.2.1"), None);
        // «Update automatically» turned off saves the preferences again.
        prefs::save(&dir, &prefs::Prefs { auto_update: false, ..prefs::load(&dir) }).unwrap();
        assert_eq!(offer_for(&dir, chosen, &found, "1.2.1"), None);
        assert_eq!(offer_for(&dir, Some(Path::new(r"C:\x\stockfish_16_x64.exe")), &found, "1.2.1"), None);
        assert_eq!(offer_for(&dir, chosen, &found, "1.3.0").as_deref(), Some("Stockfish 17.1"), "the next version");
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
            serde_json::to_value(EnginesView::new(&dir, chosen.map(Path::new), found.clone(), "1.2.1")).unwrap()
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
