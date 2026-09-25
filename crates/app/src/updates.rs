//! Updates: whether this build looks for them at all, when one may be
//! installed, and the note that lets the next start say it was updated.
//!
//! The updater reads the `latest.json` of this repository's newest GitHub
//! release and installs only an installer signed with the key whose public
//! half is `plugins.updater.pubkey` in `tauri.conf.json`. Until the owner puts
//! the real key there ([docs/release.md]), that value is a placeholder, and the
//! app neither registers the updater nor looks for updates.
//!
//! [docs/release.md]: https://github.com/asavis/oschess-cb-bridge/blob/main/docs/release.md

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use serde_json::Value;

use crate::status::View;

/// The file in the data folder that names the version being installed, so
/// that the start after the installer can say the bridge was updated.
const NOTE: &str = "updating-to";

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

/// Notes in `dir` that `version` is being installed.
pub fn note(dir: &Path, version: &str) -> Result<(), String> {
    std::fs::write(dir.join(NOTE), version).map_err(|e| format!("{}: {e}", dir.join(NOTE).display()))
}

/// Removes the note, after an installer that did not start.
pub fn forget(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(NOTE));
}

/// The version this start was updated to: the one the note names, when it is
/// the version now `running`. The note is removed either way, so that an
/// installer that failed says nothing and a later start says nothing twice.
pub fn updated(dir: &Path, running: &str) -> Option<String> {
    let path = dir.join(NOTE);
    let noted = std::fs::read_to_string(&path).ok();
    forget(dir);
    noted.filter(|v| v.trim() == running).map(|_| running.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// With the placeholder that ships until the owner has a key, this build
    /// registers no updater and never looks for updates: the app starts as
    /// before. A key put in its place must be one, or this fails. The release
    /// workflow tells the two apart by the same `PLACEHOLDER` prefix.
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
        let snapshot =
            |work: Vec<Work>| Snapshot { version: "0.1.0", port: 39581, stopped: None, databases: Vec::new(), work };
        assert!(idle(&View::of(&snapshot(Vec::new())), false));
        for work in [Work::Downloading, Work::Opening, Work::Indexing, Work::Analysing] {
            assert!(!idle(&View::of(&snapshot(vec![work])), false), "{work:?}");
        }
        assert!(!idle(&View::of(&snapshot(Vec::new())), true), "a Stockfish install");
    }

    #[test]
    fn a_view_is_idle_unless_busy() {
        let view = |states: &[&str]| View {
            version: "0.1.0".into(),
            port: 39581,
            problem: None,
            databases: states
                .iter()
                .map(|s| DatabaseView {
                    id: "0".into(),
                    name: "Base".into(),
                    format: "2cbh".into(),
                    state: s.to_string(),
                    records: None,
                    size: None,
                    progress: None,
                })
                .collect(),
            mark: "",
            busy: false,
        };
        // The states themselves no longer decide: the bridge's work does.
        assert!(idle(&view(&["ready", "missing", "cloudOnly", "unreadable", "unsupported"]), false));
        assert!(!idle(&View { busy: true, ..view(&["ready"]) }, false));
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
}
