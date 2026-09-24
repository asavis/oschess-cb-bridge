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

use serde_json::Value;

use crate::status::View;

/// The file in the data folder that names the version being installed, so
/// that the start after the installer can say the bridge was updated.
const NOTE: &str = "updating-to";

/// The updater's public key in the `updater` section of the plugins'
/// configuration, when it is a real one; `None` when there is no section or
/// no key, or the key is the placeholder or anything else that is not a key.
pub fn public_key(updater: Option<&Value>) -> Option<&str> {
    let key = updater?.get("pubkey")?.as_str()?.trim();
    is_public_key(key).then_some(key)
}

/// Whether `key` is what `tauri signer generate` prints as the public key: a
/// minisign public key file in base64, whose second line is, in base64 again,
/// an Ed25519 key: `Ed`, an 8-byte key id and the 32 bytes of the key.
fn is_public_key(key: &str) -> bool {
    let Some(text) = base64(key).and_then(|bytes| String::from_utf8(bytes).ok()) else { return false };
    let mut lines = text.lines();
    let comment = lines.next().is_some_and(|line| line.starts_with("untrusted comment:"));
    let key = lines.next().and_then(|line| base64(line.trim()));
    comment && key.is_some_and(|k| k.len() == 42 && k.starts_with(b"Ed"))
}

/// Standard padded base64; `None` for anything else.
fn base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let digit = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let (groups, _) = bytes.as_chunks::<4>();
    let last = groups.len() - 1;
    let mut out = Vec::with_capacity(groups.len() * 3);
    for (i, group) in groups.iter().enumerate() {
        let pad = group.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i != last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &group[..4 - pad] {
            n = n << 6 | digit(c)?;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

/// Whether an update may be installed now: no database is downloading or
/// opening. The installer restarts the bridge, which would start those over.
pub fn idle(view: &View) -> bool {
    !view.databases.iter().any(|d| d.state == "downloading" || d.state == "opening")
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

    #[test]
    fn base64_decodes_what_it_should_and_nothing_else() {
        for bytes in [&b"f"[..], b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0, 255, 128, 1]] {
            assert_eq!(base64(&encode(bytes)).as_deref(), Some(bytes), "{bytes:?}");
        }
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("Zm9vYg=="), Some(b"foob".to_vec()));
        for bad in ["", "Zm9", "Zm9vY===", "Zg==Zm9v", "Z=9v", "Zm9v YmFy", "Zm9v\nYmFy", "Zm9-", "Zm9vYmFy="] {
            assert_eq!(base64(bad), None, "{bad:?}");
        }
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
        let other_algorithm = tauri_key(&[&b"ED"[..], &ed25519()[2..]].concat());
        let no_comment = encode(format!("{}\n", encode(&ed25519())).as_bytes());
        let bare_line = encode(&ed25519());
        for bad in ["", "REPLACE-ME", &short, &other_algorithm, &no_comment, &bare_line, &good[1..]] {
            assert_eq!(public_key(Some(&updater(bad))), None, "{bad:?}");
        }
        assert_eq!(public_key(None), None);
        assert_eq!(public_key(Some(&serde_json::json!({ "endpoints": [] }))), None);
        assert_eq!(public_key(Some(&serde_json::json!({ "pubkey": 7 }))), None);
    }

    #[test]
    fn installs_wait_for_downloads_and_openings() {
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
        };
        assert!(idle(&view(&[])));
        assert!(idle(&view(&["ready", "missing", "cloudOnly", "unreadable", "unsupported"])));
        assert!(!idle(&view(&["ready", "downloading"])));
        assert!(!idle(&view(&["opening"])));
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
