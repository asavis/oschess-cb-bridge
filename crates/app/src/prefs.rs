//! The app's own preferences, kept in `app.json` next to `bridge.toml`: what
//! the bridge server does not read.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Deserializer, Serialize};

use crate::i18n::Lang;

const FILE: &str = "app.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    /// An explicit choice; older installations follow the Windows language.
    #[serde(deserialize_with = "language_or_default")]
    pub language: Option<Lang>,
    /// Install new versions of the bridge by themselves.
    pub auto_update: bool,
    /// The bridge version whose Stockfish offer the user put off with
    /// «Пізніше»; the next bridge version offers again.
    pub stockfish_offer_dismissed: Option<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { language: None, auto_update: true, stockfish_offer_dismissed: None }
    }
}

// An unknown saved language must not discard unrelated preferences.
fn language_or_default<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Lang>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().and_then(Lang::from_code))
}

impl Prefs {
    pub fn language(&self, display_language: u16) -> Lang {
        self.language.unwrap_or_else(|| Lang::from_langid(display_language))
    }
}

/// The preferences in `dir`; the defaults when the file is missing or unreadable.
pub fn load(dir: &Path) -> Prefs {
    std::fs::read_to_string(dir.join(FILE)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Each writer reads the latest preferences under the same lock, so a
/// concurrent Stockfish dismissal cannot overwrite a language choice.
pub fn update(dir: &Path, change: impl FnOnce(&mut Prefs)) -> Result<(), String> {
    static CHANGING: Mutex<()> = Mutex::new(());
    let _guard = CHANGING.lock().unwrap_or_else(|e| e.into_inner());
    let mut prefs = load(dir);
    change(&mut prefs);
    save(dir, &prefs)
}

pub fn save(dir: &Path, prefs: &Prefs) -> Result<(), String> {
    let text = serde_json::to_string_pretty(prefs).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Replaced whole: a cut file would load as the defaults and silently turn
    // automatic updates back on (#62).
    bridge::files::write_atomic(&dir.join(FILE), (text + "\n").as_bytes())
        .map_err(|e| format!("{}: {e}", dir.join(FILE).display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_unknown_language_keep_the_windows_fallback_and_other_choices() {
        for language in ["", r#", "language": "fr""#, r#", "language": null"#, r#", "language": 42"#] {
            let prefs: Prefs = serde_json::from_str(&format!(
                r#"{{"autoUpdate": false, "stockfishOfferDismissed": "1.2.3"{language}}}"#
            ))
            .unwrap();
            assert!(!prefs.auto_update);
            assert_eq!(prefs.stockfish_offer_dismissed.as_deref(), Some("1.2.3"));
            assert_eq!(prefs.language(0x0422), Lang::Uk);
            assert_eq!(prefs.language(0x0419), Lang::Uk);
            assert_eq!(prefs.language(0x0409), Lang::En);
        }
        for (code, lang) in [("uk", Lang::Uk), ("en", Lang::En)] {
            let prefs: Prefs = serde_json::from_str(&format!(r#"{{"language":"{code}"}}"#)).unwrap();
            assert_eq!(prefs.language(0x0422), lang);
            assert_eq!(prefs.language(0x0409), lang);
        }
    }

    #[test]
    fn concurrent_preference_changes_keep_each_others_choices() {
        let dir = std::env::temp_dir().join(format!("bridge-app-language-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let start = &std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let dir = &dir;
            scope.spawn(move || {
                start.wait();
                update(dir, |p| p.language = Some(Lang::En)).unwrap();
            });
            scope.spawn(move || {
                start.wait();
                update(dir, |p| p.auto_update = false).unwrap();
            });
            start.wait();
            update(dir, |p| p.stockfish_offer_dismissed = Some("1.2.3".into())).unwrap();
        });
        let saved = load(&dir);
        assert_eq!(saved.language, Some(Lang::En));
        assert!(!saved.auto_update);
        assert_eq!(saved.stockfish_offer_dismissed.as_deref(), Some("1.2.3"));
        update(&dir, |p| p.language = Some(Lang::Uk)).unwrap();
        assert_eq!(load(&dir), Prefs { language: Some(Lang::Uk), ..saved });
        // A path which is a file cannot hold app.json: the write reports failure.
        assert!(update(&dir.join(FILE), |p| p.language = Some(Lang::En)).is_err());
        assert_eq!(load(&dir).language, Some(Lang::Uk));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn defaults_then_what_was_saved() {
        let dir = std::env::temp_dir().join(format!("bridge-app-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), Prefs::default());
        let saved =
            Prefs { language: Some(Lang::Uk), auto_update: false, stockfish_offer_dismissed: Some("0.2.0".into()) };
        save(&dir, &saved).unwrap();
        assert_eq!(load(&dir), saved);
        // Replaced whole, with nothing left beside it (#62).
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [FILE]);
        std::fs::write(dir.join(FILE), "{ not json").unwrap();
        assert_eq!(load(&dir), Prefs::default());
        std::fs::write(dir.join(FILE), "{\"other\": 1}").unwrap();
        assert_eq!(load(&dir), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
