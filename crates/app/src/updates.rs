//! Updates: whether this build looks for them at all, when one may be
//! installed, and the note that lets the next start say it was updated.
//!
//! The updater reads the `latest.json` of this repository's newest GitHub
//! release and installs only an installer signed with the key whose public
//! half is `plugins.updater.pubkey` in `tauri.conf.json` ([docs/release.md]).
//! A value that is no public key, such as the `PLACEHOLDER` text of the builds
//! before #51, turns updates off: the app neither registers the updater nor
//! looks for updates.
//!
//! [docs/release.md]: https://github.com/asavis/oschess-cb-bridge/blob/main/docs/release.md

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Duration;

use serde_json::Value;

use crate::status::View;

/// The file in the data folder that names the version being installed, so
/// that the start after the installer can say the bridge was updated.
const NOTE: &str = "updating-to";
/// The file that names the version a Store install started from (#153): the
/// Store does not say which version it installs, so the start after it says
/// the bridge was updated when it runs another version than this one.
const FROM: &str = "updating-from";
/// The file that names the version the bridge last started as (#265). The
/// Store also installs an update by itself while the bridge is closed, after
/// an exit or across a restart of Windows, and leaves no note; a start that
/// runs a newer version than the last one says it was updated all the same.
const LAST: &str = "last-run";

/// How long the process must have run before Windows restarts it after a
/// Store install: `RegisterApplicationRestart` restarts only a process that
/// ran for at least 60 seconds.
pub const RESTARTABLE_AFTER: Duration = Duration::from_secs(61);

/// The updater's public key in the `updater` section of the plugins'
/// configuration, when it is a real one; `None` when there is no section or
/// no key, or the key is the placeholder or anything else the updater could
/// not use. Surrounding white space is dropped, and the app hands the updater
/// this value, never the raw one.
pub fn public_key(updater: Option<&Value>) -> Option<&str> {
    let key = updater?.get("pubkey")?.as_str()?.trim();
    is_public_key(key).then_some(key)
}

/// Whether the updater accepts `key`: it is decoded exactly as
/// tauri-plugin-updater decodes it before checking a signature, standard
/// base64 and then a minisign public key (an algorithm, an 8-byte key id and
/// the 32 bytes of an Ed25519 key), and it opens with an untrusted comment, as
/// `tauri signer generate` writes it.
fn is_public_key(key: &str) -> bool {
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(key) else { return false };
    let Ok(text) = String::from_utf8(bytes) else { return false };
    text.lines().next().is_some_and(|line| line.starts_with("untrusted comment:"))
        && minisign_verify::PublicKey::decode(&text).is_ok()
}

/// One look for updates at a time. An automatic look skips while another
/// runs; a look on request waits for the running one and then looks itself,
/// so the user always hears the outcome of the look they asked for.
pub struct Gate(Mutex<()>);

impl Default for Gate {
    fn default() -> Gate {
        Gate::new()
    }
}

impl Gate {
    pub const fn new() -> Gate {
        Gate(Mutex::new(()))
    }

    /// Enters the gate for a look, `asked` or automatic; `None` when an
    /// automatic look should skip. The look runs while the guard lives.
    pub fn enter(&self, asked: bool) -> Option<MutexGuard<'_, ()>> {
        if asked {
            return Some(self.0.lock().unwrap_or_else(PoisonError::into_inner));
        }
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::WouldBlock) => None,
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        }
    }
}

/// Whether an update may be installed now (#61): the bridge has no work a
/// restart would lose — a download, a database opening, a position index
/// built, an analysis streamed (for a few minutes; see
/// `bridge::snapshot::ANALYSIS_HOLDS_UPDATES`) — and no Stockfish is being
/// installed. The installer restarts the bridge, which would lose them.
pub fn idle(view: &View, installing: bool) -> bool {
    !view.busy && !installing
}

/// What a look in the Store channel does with what it found (#153). The
/// Store installs a packaged app's update only while the app is closed, and
/// the bridge runs in the tray all the time, so the app installs its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreStep {
    /// No update waits; a look the user asked for says so.
    Latest,
    /// Windows installs updates silently for this user («Update apps
    /// automatically» on, a network that is not metered): wait until the
    /// bridge is idle, then install; Windows closes the bridge and starts it
    /// again.
    InstallQuietly,
    /// Windows would not install silently, and the user asked (#232): ask
    /// Windows to download the update, then wait until the bridge is idle and
    /// ask it to install; each request shows Windows' own dialog. Once the
    /// user accepts the install, Windows closes the bridge for it and starts
    /// it again. The bridge's page in the Microsoft Store opens only when a
    /// request could not run.
    RequestInstall,
    /// Windows would not install silently, and nobody asked: a notification
    /// says an update waits, or that it is required when the submission marks
    /// it mandatory.
    Tell { mandatory: bool },
}

/// Whether a Store install may start now (#153): the bridge is idle, as for
/// the direct installer, and it has run long enough (`alive`) for Windows to
/// start it again after closing it for the install.
pub fn store_ready(view: &View, installing: bool, alive: Duration) -> bool {
    idle(view, installing) && alive >= RESTARTABLE_AFTER
}

/// How a Store download or install ended. Windows closes the bridge for an
/// install that takes place, so the process that hears how one ended runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreOutcome {
    /// Windows reported it complete.
    Completed,
    /// The user declined it in Windows' dialog (#232): nothing failed.
    Declined,
    /// It did not complete.
    Incomplete,
}

/// What installing a Store update needs from Windows, so that its order is
/// tested without Windows (#153).
pub trait StoreInstall {
    /// Downloads the update without installing it.
    fn download(&self) -> Result<StoreOutcome, String>;
    /// Asks Windows to start the bridge again after closing it.
    fn register_restart(&self) -> Result<(), String>;
    /// Installs the downloaded update; Windows closes the bridge for it, so
    /// this returns only when the install did not take place, saying how it
    /// ended.
    fn install(&self) -> Result<StoreOutcome, String>;
}

/// A [`StoreInstall`] made of three calls, for a caller whose Windows types
/// are simplest captured in closures.
pub struct StoreCalls<D, R, I> {
    pub download: D,
    pub register_restart: R,
    pub install: I,
}

impl<D, R, I> StoreInstall for StoreCalls<D, R, I>
where
    D: Fn() -> Result<StoreOutcome, String>,
    R: Fn() -> Result<(), String>,
    I: Fn() -> Result<StoreOutcome, String>,
{
    fn download(&self) -> Result<StoreOutcome, String> {
        (self.download)()
    }
    fn register_restart(&self) -> Result<(), String> {
        (self.register_restart)()
    }
    fn install(&self) -> Result<StoreOutcome, String> {
        (self.install)()
    }
}

/// Installs a Store update, silently or through Windows' dialogs (#232): it
/// downloads first, then waits until `ready` (idle and restartable,
/// [`store_ready`]) so that no work begun during the download is lost, notes
/// where it started from in `dir`, asks for the restart and installs. A
/// process still running after the install forgets the note. `Ok` when the
/// install reported completion or the user declined the download or the
/// install; the error says why it did not take place otherwise.
pub fn install_store_update(
    store: &impl StoreInstall,
    dir: &Path,
    running: &str,
    wait_ready: impl FnOnce(),
) -> Result<(), String> {
    match store.download()? {
        StoreOutcome::Completed => {}
        StoreOutcome::Declined => return Ok(()),
        StoreOutcome::Incomplete => return Err("the Store download did not complete".into()),
    }
    wait_ready();
    note_from(dir, running)?;
    store.register_restart()?;
    let installed = store.install();
    forget(dir);
    match installed? {
        StoreOutcome::Completed | StoreOutcome::Declined => Ok(()),
        StoreOutcome::Incomplete => Err("the Store install did not complete".into()),
    }
}

/// Runs `job` on the thread `dispatch` hands it to, and waits on this one for
/// what it returns (#232): Microsoft requires the Store requests that show
/// Windows' dialogs to start on the UI thread, which must never wait for
/// them. A dispatch that fails, or that drops the job without running it, as
/// an event loop that has ended does, is an error, never a wait without end.
pub fn run_on<T: Send + 'static>(
    dispatch: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), String>,
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    dispatch(Box::new(move || {
        let _ = tx.send(job());
    }))?;
    rx.recv().map_err(|_| "the job was dropped without running".to_string())?
}

/// The step for a look that `found` an update or not, when Windows allows
/// `silent` installs, the update is `mandatory`, and the user `asked`.
pub fn store_step(found: bool, silent: bool, mandatory: bool, asked: bool) -> StoreStep {
    match (found, silent, asked) {
        (false, _, _) => StoreStep::Latest,
        (true, true, _) => StoreStep::InstallQuietly,
        (true, false, true) => StoreStep::RequestInstall,
        (true, false, false) => StoreStep::Tell { mandatory },
    }
}

/// Notes in `dir` that `version` is being installed. The error names the
/// note by its file name alone: it goes to the log, which holds no path
/// (#117).
pub fn note(dir: &Path, version: &str) -> Result<(), String> {
    std::fs::write(dir.join(NOTE), version).map_err(|e| format!("{NOTE}: {e}"))
}

/// Notes in `dir` that a Store install starts from `running` (#153).
pub fn note_from(dir: &Path, running: &str) -> Result<(), String> {
    std::fs::write(dir.join(FROM), running).map_err(|e| format!("{FROM}: {e}"))
}

/// Removes the notes, after an install that did not take place.
pub fn forget(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(NOTE));
    let _ = std::fs::remove_file(dir.join(FROM));
}

/// The version this start was updated to: the one the direct installer's note
/// names, when it is the version now `running`; the one `running` after a
/// Store install that started from another version; or the one `running`
/// when it is newer than the version of the last start, however it was
/// installed (#265). The notes are removed and this start recorded either
/// way, so that an install that failed says nothing and a later start says
/// nothing twice.
pub fn updated(dir: &Path, running: &str) -> Option<String> {
    let to = std::fs::read_to_string(dir.join(NOTE)).ok();
    let from = std::fs::read_to_string(dir.join(FROM)).ok();
    let last = std::fs::read_to_string(dir.join(LAST)).ok();
    forget(dir);
    // A start that cannot be recorded only costs the next update its notice.
    let _ = std::fs::write(dir.join(LAST), running);
    let reached = to.is_some_and(|v| v.trim() == running);
    let moved = from.is_some_and(|v| !v.trim().is_empty() && v.trim() != running);
    let newer = last.is_some_and(|v| newer_than(running, v.trim()));
    (reached || moved || newer).then(|| running.to_string())
}

/// Whether version `a` is newer than `b`, both dotted numbers such as
/// `1.10.0`; `false` when either is not one, as nothing is then said.
fn newer_than(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>();
    matches!((parts(a), parts(b)), (Some(a), Some(b)) if a > b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge::catalog::State;

    use crate::status::testing::{self, view};

    /// A public key as `tauri signer generate` prints it, made here from an
    /// arbitrary key id and key.
    fn tauri_key(key: &[u8]) -> String {
        let text = format!("untrusted comment: minisign public key: 1A2B3C4D5E6F7081\n{}\n", encode(key));
        encode(text.as_bytes())
    }

    fn encode(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for group in bytes.chunks(3) {
            let n = group.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
            for i in 0..4 {
                out.push(if i <= group.len() { DIGITS[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        out
    }

    fn ed25519() -> Vec<u8> {
        [&b"Ed"[..], &[0x81, 0x70, 0x6f, 0x5e, 0x4d, 0x3c, 0x2b, 0x1a], &[7u8; 32]].concat()
    }

    fn updater(pubkey: &str) -> Value {
        serde_json::json!({ "pubkey": pubkey, "endpoints": ["https://example.org/latest.json"] })
    }

    /// A shipped value that does not start with `PLACEHOLDER` must be a public
    /// key, or this fails. The `PLACEHOLDER` text of the builds before #51 still
    /// passes: with it, a build registers no updater and never looks for
    /// updates. The release workflow tells the two apart by the same prefix.
    #[test]
    fn the_shipped_configuration_decides() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let section = config.pointer("/plugins/updater").expect("tauri.conf.json configures the updater");
        let shipped = section["pubkey"].as_str().expect("a pubkey");
        assert_eq!(public_key(Some(section)).is_some(), !shipped.starts_with("PLACEHOLDER"), "{shipped:?}");

        let mut placeholder = section.clone();
        placeholder["pubkey"] = Value::String("PLACEHOLDER: the owner replaces this (docs/release.md).".into());
        assert_eq!(public_key(Some(&placeholder)), None);
        let mut real = section.clone();
        real["pubkey"] = Value::String(tauri_key(&ed25519()));
        assert_eq!(public_key(Some(&real)), real["pubkey"].as_str());
    }

    #[test]
    fn only_a_real_key_turns_updates_on() {
        // The public key in the Tauri CLI's own tests (tauri-cli 2.11.5, src/migrate).
        let published = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDE5QzMxNjYwNTM5OEUwNTgKUldSWTRKaFRZQmJER1h4d1ZMYVA3dnluSjdpN2RmMldJR09hUFFlZDY0SlFqckkvRUJhZDJVZXAK";
        assert_eq!(public_key(Some(&updater(published))), Some(published));

        let good = tauri_key(&ed25519());
        assert_eq!(public_key(Some(&updater(&good))), Some(good.as_str()));
        assert_eq!(public_key(Some(&updater(&format!("  {good}\n")))), Some(good.as_str()), "surrounding space");

        let short = tauri_key(&ed25519()[..41]);
        let no_comment = encode(format!("{}\n", encode(&ed25519())).as_bytes());
        let bare_line = encode(&ed25519());
        for bad in ["", "REPLACE-ME", &short, &no_comment, &bare_line, &good[1..]] {
            assert_eq!(public_key(Some(&updater(bad))), None, "{bad:?}");
        }
        // What the updater's own decoding refuses is refused here too:
        // non-zero padding bits in the outer base64, and space around the key
        // line inside it.
        let padded = {
            let text = format!("untrusted comment: minisign public key: 1A2B3C4D5E6F\n{}\n", encode(&ed25519()));
            assert_ne!(text.len() % 3, 0, "the outer base64 ends in padding");
            encode(text.as_bytes())
        };
        assert_eq!(public_key(Some(&updater(&padded))), Some(padded.as_str()));
        let last = padded.trim_end_matches('=').len() - 1;
        let mut loose = padded.clone().into_bytes();
        loose[last] += 1;
        let loose = String::from_utf8(loose).unwrap();
        let spaced = encode(format!("untrusted comment: minisign\n {} \n", encode(&ed25519())).as_bytes());
        for bad in [&loose, &spaced] {
            assert_eq!(public_key(Some(&updater(bad))), None, "{bad:?}");
        }
        assert_eq!(public_key(None), None);
        assert_eq!(public_key(Some(&serde_json::json!({ "endpoints": [] }))), None);
        assert_eq!(public_key(Some(&serde_json::json!({ "pubkey": 7 }))), None);
    }

    #[test]
    fn a_look_on_request_waits_and_an_automatic_one_skips() {
        use std::sync::mpsc;
        use std::time::Duration;
        static GATE: Gate = Gate::new();
        let running = GATE.enter(false).expect("the first look enters");
        assert!(GATE.enter(false).is_none(), "an automatic look skips while one runs");
        let (tx, rx) = mpsc::channel();
        let asked = std::thread::spawn(move || {
            let _look = GATE.enter(true).expect("a look on request always runs");
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "it waits for the running look");
        drop(running);
        rx.recv_timeout(crate::PATIENCE).expect("then it runs");
        asked.join().unwrap();
        assert!(GATE.enter(false).is_some(), "and the gate is free again");
    }

    #[test]
    fn installs_wait_for_work_a_restart_would_lose() {
        use bridge::snapshot::{Snapshot, Work};
        let snapshot = |work: Vec<Work>| Snapshot { work, ..testing::snapshot() };
        assert!(idle(&View::of(&snapshot(Vec::new())), false));
        for work in [Work::Downloading, Work::Opening, Work::Indexing, Work::Analysing] {
            assert!(!idle(&View::of(&snapshot(vec![work])), false), "{work:?}");
        }
        assert!(!idle(&View::of(&snapshot(Vec::new())), true), "a Stockfish install");
    }

    #[test]
    fn a_view_is_idle_unless_busy() {
        // The states themselves no longer decide: the bridge's work does.
        let all = [State::Ready, State::Missing, State::CloudOnly, State::Unreadable, State::Unsupported];
        assert!(idle(&view(&all), false));
        assert!(!idle(&View { busy: true, ..view(&[State::Ready]) }, false));
    }

    #[test]
    fn the_start_after_an_update_says_so_once() {
        let dir = std::env::temp_dir().join(format!("bridge-app-updates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(updated(&dir, "0.2.0"), None, "no note");

        note(&dir, "0.2.0").unwrap();
        assert_eq!(updated(&dir, "0.2.0").as_deref(), Some("0.2.0"));
        assert_eq!(updated(&dir, "0.2.0"), None, "said once");

        note(&dir, "0.3.0").unwrap();
        assert_eq!(updated(&dir, "0.2.0"), None, "the installer did not run");
        assert!(!dir.join(NOTE).exists());

        note(&dir, "0.3.0").unwrap();
        forget(&dir);
        // A first start, with no record of the last one: the forgotten note
        // alone would say it.
        std::fs::remove_file(dir.join(LAST)).unwrap();
        assert_eq!(updated(&dir, "0.3.0"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The start of a newer version says it was updated without any note, as
    /// after an install the Store made while the bridge was closed (#265);
    /// once, and never for the first start, an older version or a version
    /// that is no dotted number.
    #[test]
    fn the_start_of_a_newer_version_says_so_once() {
        let dir = std::env::temp_dir().join(format!("bridge-app-last-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(updated(&dir, "1.3.2"), None, "the first start");
        assert_eq!(updated(&dir, "1.3.3").as_deref(), Some("1.3.3"));
        assert_eq!(updated(&dir, "1.3.3"), None, "once");
        assert_eq!(updated(&dir, "1.3.2"), None, "an older version");
        assert_eq!(updated(&dir, "1.10.0").as_deref(), Some("1.10.0"), "compared as numbers, not text");
        std::fs::write(dir.join(LAST), "garbled").unwrap();
        assert_eq!(updated(&dir, "1.11.0"), None, "a record that is no version");
        assert_eq!(updated(&dir, "1.12.0-rc.1"), None, "a version that is no dotted number");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(newer_than("1.3.3", "1.3.2"));
        assert!(newer_than("1.10.0", "1.9.9"));
        assert!(newer_than("2.0.0", "1.99.99"));
        assert!(!newer_than("1.3.2", "1.3.2"));
        assert!(!newer_than("1.3.1", "1.3.2"));
        assert!(!newer_than("", "1.3.2"));
        assert!(!newer_than("1.3.3", ""));
        assert!(!newer_than("1.3.x", "1.3.2"));
    }

    /// A note that cannot be written fails with an error the updater logs as
    /// it is: it names the note, and neither its folder nor any above it.
    #[test]
    fn a_note_that_fails_names_no_folder() {
        let top = std::env::temp_dir().join(format!("bridge-app-note-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let dir = top.join("Jane Doe").join("oschess-bridge");
        std::fs::create_dir_all(dir.join(NOTE)).unwrap();
        let e = note(&dir, "0.2.0").unwrap_err();
        assert!(e.starts_with("updating-to: "), "{e}");
        let logged = format!("update: {e}");
        for folder in dir.ancestors().filter(|a| a.parent().is_some()) {
            assert!(!logged.contains(folder.to_str().unwrap()), "{} in {logged}", folder.display());
        }
        for name in ["Jane Doe", "oschess-bridge", top.file_name().unwrap().to_str().unwrap()] {
            assert!(!logged.contains(name), "{name} in {logged}");
        }
        assert!(dir.join(NOTE).is_dir(), "the folder in its way is left");
        let _ = std::fs::remove_dir_all(&top);
    }

    /// What a Store look does: an update installs silently whenever Windows
    /// allows it, asked or not and mandatory or not; otherwise a look on
    /// request asks Windows to install it through its own dialog (#232), and
    /// an automatic one tells, saying whether the update is required.
    #[test]
    fn a_store_look_installs_quietly_whenever_windows_allows_it() {
        use StoreStep::*;
        // found, silent, mandatory, asked → step
        let cases = [
            (false, true, false, true, Latest),
            (false, false, true, false, Latest),
            (false, false, false, true, Latest),
            (true, true, false, false, InstallQuietly),
            (true, true, true, true, InstallQuietly),
            (true, false, false, true, RequestInstall),
            (true, false, true, true, RequestInstall),
            (true, false, false, false, Tell { mandatory: false }),
            (true, false, true, false, Tell { mandatory: true }),
        ];
        for (found, silent, mandatory, asked, step) in cases {
            assert_eq!(store_step(found, silent, mandatory, asked), step, "{found} {silent} {mandatory} {asked}");
        }
    }

    fn idle_view(busy: bool) -> View {
        View { busy, ..view(&[]) }
    }

    /// A Store install waits for idle and for the minute Windows needs before
    /// it restarts a process it closed.
    #[test]
    fn a_store_install_waits_until_windows_would_restart_the_bridge() {
        assert!(!store_ready(&idle_view(false), false, Duration::from_secs(60)));
        assert!(store_ready(&idle_view(false), false, RESTARTABLE_AFTER));
        assert!(!store_ready(&idle_view(true), false, Duration::from_secs(3600)));
        assert!(!store_ready(&idle_view(false), true, Duration::from_secs(3600)));
    }

    struct FakeStore<'a> {
        events: &'a std::cell::RefCell<Vec<String>>,
        dir: &'a Path,
        downloaded: StoreOutcome,
    }

    impl StoreInstall for FakeStore<'_> {
        fn download(&self) -> Result<StoreOutcome, String> {
            self.events.borrow_mut().push("download".into());
            Ok(self.downloaded)
        }
        fn register_restart(&self) -> Result<(), String> {
            self.events.borrow_mut().push("register".into());
            Ok(())
        }
        fn install(&self) -> Result<StoreOutcome, String> {
            let noted = std::fs::read_to_string(self.dir.join(FROM)).unwrap_or_default();
            self.events.borrow_mut().push(format!("install from {noted}"));
            Ok(StoreOutcome::Incomplete)
        }
    }

    /// A Store install downloads before it waits for the bridge to be ready,
    /// so work begun during the download is never cut off; it notes where it
    /// started from before installing, and a process still running afterwards
    /// forgets the note. A download that did not complete installs nothing.
    #[test]
    fn a_store_install_downloads_then_waits_then_installs() {
        let dir = std::env::temp_dir().join(format!("bridge-app-store-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let events = std::cell::RefCell::new(Vec::new());
        let store = FakeStore { events: &events, dir: &dir, downloaded: StoreOutcome::Completed };
        let result = install_store_update(&store, &dir, "1.1.0", || events.borrow_mut().push("ready".into()));
        assert_eq!(result, Err("the Store install did not complete".into()));
        assert_eq!(*events.borrow(), ["download", "ready", "register", "install from 1.1.0"]);
        assert!(!dir.join(FROM).exists(), "a process still running forgets the note");

        events.borrow_mut().clear();
        let store = FakeStore { events: &events, dir: &dir, downloaded: StoreOutcome::Incomplete };
        assert!(install_store_update(&store, &dir, "1.1.0", || events.borrow_mut().push("ready".into())).is_err());
        assert_eq!(*events.borrow(), ["download"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request through Windows' dialogs (#232) keeps the silent install's
    /// order: the download, which asks the user, then ready, note, restart
    /// and the install, which asks again, so work begun during the download
    /// is not cut off. A user who declines either dialog is no failure; a
    /// request that could not run is, so that the Store page opens instead.
    /// A declined or failed download installs nothing and neither writes nor
    /// forgets a note; a process still running after the install forgets its
    /// own.
    #[test]
    fn a_store_request_downloads_then_waits_then_installs_and_declining_is_no_failure() {
        use StoreOutcome::*;
        let dir = std::env::temp_dir().join(format!("bridge-app-store-request-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let events = std::cell::RefCell::new(Vec::new());
        let request = |download: Result<StoreOutcome, String>, install: Result<StoreOutcome, String>| {
            events.borrow_mut().clear();
            let store = StoreCalls {
                download: || {
                    events.borrow_mut().push("download".to_string());
                    download.clone()
                },
                register_restart: || {
                    events.borrow_mut().push("register".to_string());
                    Ok(())
                },
                install: || {
                    let noted = std::fs::read_to_string(dir.join(FROM)).unwrap_or_default();
                    events.borrow_mut().push(format!("install from {noted}"));
                    install.clone()
                },
            };
            install_store_update(&store, &dir, "1.2.0", || events.borrow_mut().push("ready".into()))
        };
        // The error of a request started off the UI thread.
        let failed = "0x80070578".to_string();

        for (installed, result) in [
            (Ok(Completed), Ok(())),
            (Ok(Declined), Ok(())),
            (Ok(Incomplete), Err("the Store install did not complete".to_string())),
            (Err(failed.clone()), Err(failed.clone())),
        ] {
            assert_eq!(request(Ok(Completed), installed.clone()), result, "install {installed:?}");
            assert_eq!(*events.borrow(), ["download", "ready", "register", "install from 1.2.0"], "{installed:?}");
            assert!(!dir.join(FROM).exists(), "a process still running forgets the note");
        }

        note_from(&dir, "1.0.0").unwrap();
        for (downloaded, result) in [
            (Ok(Declined), Ok(())),
            (Ok(Incomplete), Err("the Store download did not complete".to_string())),
            (Err(failed.clone()), Err(failed.clone())),
        ] {
            assert_eq!(request(downloaded.clone(), Ok(Completed)), result, "download {downloaded:?}");
            assert_eq!(*events.borrow(), ["download"], "{downloaded:?}: no wait, no restart, no install");
            assert_eq!(
                std::fs::read_to_string(dir.join(FROM)).unwrap(),
                "1.0.0",
                "{downloaded:?}: the note is untouched"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A job handed to another thread runs there, and what it returns comes
    /// back; a dispatch that fails, or drops the job without running it, is an
    /// error rather than a wait without end (#232).
    #[test]
    fn a_job_run_on_another_thread_answers_or_fails() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let here = std::thread::current().id();
        let spawn = |job: Box<dyn FnOnce() + Send>| {
            std::thread::spawn(job);
            Ok(())
        };
        assert_eq!(run_on(spawn, move || Ok(std::thread::current().id() != here)), Ok(true));
        assert_eq!(run_on(spawn, || Err::<(), _>("0x80070578".to_string())), Err("0x80070578".to_string()));

        let ran = Arc::new(AtomicBool::new(false));
        let job = |ran: &Arc<AtomicBool>| {
            let ran = Arc::clone(ran);
            move || {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            }
        };
        assert_eq!(run_on(|_| Err("no event loop".to_string()), job(&ran)), Err("no event loop".to_string()));
        let dropped = |job: Box<dyn FnOnce() + Send>| {
            drop(job);
            Ok(())
        };
        assert_eq!(run_on(dropped, job(&ran)), Err("the job was dropped without running".to_string()));
        assert!(!ran.load(Ordering::SeqCst), "neither job ran");
    }

    /// The start after a Store install says it was updated when it runs
    /// another version than the one the install started from, and nothing
    /// when the install failed and the same version starts again; once.
    #[test]
    fn the_start_after_a_store_install_says_so_once() {
        let dir = std::env::temp_dir().join(format!("bridge-app-store-from-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        note_from(&dir, "1.1.0").unwrap();
        assert_eq!(updated(&dir, "1.2.0"), Some("1.2.0".to_string()));
        assert_eq!(updated(&dir, "1.2.0"), None, "once");
        note_from(&dir, "1.2.0").unwrap();
        assert_eq!(updated(&dir, "1.2.0"), None, "the same version: the install did not happen");
        assert!(!dir.join(FROM).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
