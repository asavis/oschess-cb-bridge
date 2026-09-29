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
    /// Windows would not install silently, and the user asked: the bridge's
    /// page in the Microsoft Store opens, where «Update» installs it.
    OpenStore,
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

/// What installing a Store update needs from Windows, so that its order is
/// tested without Windows (#153).
pub trait StoreInstall {
    /// Downloads the update without installing it; whether it completed.
    fn download(&self) -> Result<bool, String>;
    /// Asks Windows to start the bridge again after closing it.
    fn register_restart(&self) -> Result<(), String>;
    /// Installs the downloaded update; Windows closes the bridge for it, so
    /// this returns only when the install did not take place: whether it
    /// reported completion anyway.
    fn install(&self) -> Result<bool, String>;
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
    D: Fn() -> Result<bool, String>,
    R: Fn() -> Result<(), String>,
    I: Fn() -> Result<bool, String>,
{
    fn download(&self) -> Result<bool, String> {
        (self.download)()
    }
    fn register_restart(&self) -> Result<(), String> {
        (self.register_restart)()
    }
    fn install(&self) -> Result<bool, String> {
        (self.install)()
    }
}

/// Installs a Store update silently: it downloads first, then waits until
/// `ready` (idle and restartable, [`store_ready`]) so that no work begun
/// during the download is lost, notes where it started from in `dir`, asks
/// for the restart and installs. A process still running after the install
/// forgets the note.
pub fn install_quietly(
    store: &impl StoreInstall,
    dir: &Path,
    running: &str,
    wait_ready: impl FnOnce(),
) -> Result<(), String> {
    if !store.download()? {
        return Err("the Store download did not complete".into());
    }
    wait_ready();
    note_from(dir, running)?;
    store.register_restart()?;
    let installed = store.install();
    forget(dir);
    match installed? {
        true => Ok(()),
        false => Err("the Store install did not complete".into()),
    }
}

/// The step for a look that `found` an update or not, when Windows allows
/// `silent` installs, the update is `mandatory`, and the user `asked`.
pub fn store_step(found: bool, silent: bool, mandatory: bool, asked: bool) -> StoreStep {
    match (found, silent, asked) {
        (false, _, _) => StoreStep::Latest,
        (true, true, _) => StoreStep::InstallQuietly,
        (true, false, true) => StoreStep::OpenStore,
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
/// names, when it is the version now `running`, or the one `running` after a
/// Store install that started from another version. The notes are removed
/// either way, so that an install that failed says nothing and a later start
/// says nothing twice.
pub fn updated(dir: &Path, running: &str) -> Option<String> {
    let to = std::fs::read_to_string(dir.join(NOTE)).ok();
    let from = std::fs::read_to_string(dir.join(FROM)).ok();
    forget(dir);
    let reached = to.is_some_and(|v| v.trim() == running);
    let moved = from.is_some_and(|v| !v.trim().is_empty() && v.trim() != running);
    (reached || moved).then(|| running.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge::catalog::State;

    use crate::status::DatabaseView;

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
        rx.recv_timeout(Duration::from_secs(10)).expect("then it runs");
        asked.join().unwrap();
        assert!(GATE.enter(false).is_some(), "and the gate is free again");
    }

    #[test]
    fn installs_wait_for_work_a_restart_would_lose() {
        use bridge::snapshot::{Snapshot, Work};
        let snapshot = |work: Vec<Work>| Snapshot {
            version: "0.1.0",
            port: 39581,
            stopped: None,
            databases: Vec::new(),
            work,
            served: false,
        };
        assert!(idle(&View::of(&snapshot(Vec::new())), false));
        for work in [Work::Downloading, Work::Opening, Work::Indexing, Work::Analysing] {
            assert!(!idle(&View::of(&snapshot(vec![work])), false), "{work:?}");
        }
        assert!(!idle(&View::of(&snapshot(Vec::new())), true), "a Stockfish install");
    }

    #[test]
    fn a_view_is_idle_unless_busy() {
        let view = |states: &[State]| View {
            version: "0.1.0".into(),
            port: 39581,
            problem: None,
            databases: states
                .iter()
                .map(|s| DatabaseView {
                    id: "0".into(),
                    name: "Base".into(),
                    format: "2cbh".into(),
                    state: *s,
                    records: None,
                    size: None,
                    progress: None,
                })
                .collect(),
            connected: false,
            mark: "",
            busy: false,
        };
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
        assert_eq!(updated(&dir, "0.3.0"), None);
        let _ = std::fs::remove_dir_all(&dir);
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
    /// request opens the Store, and an automatic one tells, saying whether the
    /// update is required.
    #[test]
    fn a_store_look_installs_quietly_whenever_windows_allows_it() {
        use StoreStep::*;
        // found, silent, mandatory, asked → step
        let cases = [
            (false, true, false, true, Latest),
            (false, false, true, false, Latest),
            (true, true, false, false, InstallQuietly),
            (true, true, true, true, InstallQuietly),
            (true, false, false, true, OpenStore),
            (true, false, true, true, OpenStore),
            (true, false, false, false, Tell { mandatory: false }),
            (true, false, true, false, Tell { mandatory: true }),
        ];
        for (found, silent, mandatory, asked, step) in cases {
            assert_eq!(store_step(found, silent, mandatory, asked), step, "{found} {silent} {mandatory} {asked}");
        }
    }

    fn idle_view(busy: bool) -> View {
        View {
            version: "1.1.0".into(),
            port: 39581,
            problem: None,
            databases: Vec::new(),
            connected: false,
            mark: "",
            busy,
        }
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
        downloaded: bool,
    }

    impl StoreInstall for FakeStore<'_> {
        fn download(&self) -> Result<bool, String> {
            self.events.borrow_mut().push("download".into());
            Ok(self.downloaded)
        }
        fn register_restart(&self) -> Result<(), String> {
            self.events.borrow_mut().push("register".into());
            Ok(())
        }
        fn install(&self) -> Result<bool, String> {
            let noted = std::fs::read_to_string(self.dir.join(FROM)).unwrap_or_default();
            self.events.borrow_mut().push(format!("install from {noted}"));
            Ok(false)
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
        let store = FakeStore { events: &events, dir: &dir, downloaded: true };
        let result = install_quietly(&store, &dir, "1.1.0", || events.borrow_mut().push("ready".into()));
        assert_eq!(result, Err("the Store install did not complete".into()));
        assert_eq!(*events.borrow(), ["download", "ready", "register", "install from 1.1.0"]);
        assert!(!dir.join(FROM).exists(), "a process still running forgets the note");

        events.borrow_mut().clear();
        let store = FakeStore { events: &events, dir: &dir, downloaded: false };
        assert!(install_quietly(&store, &dir, "1.1.0", || events.borrow_mut().push("ready".into())).is_err());
        assert_eq!(*events.borrow(), ["download"]);
        let _ = std::fs::remove_dir_all(&dir);
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
