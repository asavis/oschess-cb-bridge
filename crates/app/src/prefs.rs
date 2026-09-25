//! The app's own preferences, kept in `app.json` next to `bridge.toml`: what
//! the bridge server does not read.

use std::path::Path;

use serde::{Deserialize, Serialize};

const FILE: &str = "app.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    /// Install new versions of the bridge by themselves.
    pub auto_update: bool,
    /// The bridge version whose Stockfish offer the user put off with
    /// «Пізніше»; the next bridge version offers again.
    pub stockfish_offer_dismissed: Option<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { auto_update: true, stockfish_offer_dismissed: None }
    }
}

/// The preferences in `dir`; the defaults when the file is missing or unreadable.
pub fn load(dir: &Path) -> Prefs {
    std::fs::read_to_string(dir.join(FILE)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
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
    fn defaults_then_what_was_saved() {
        let dir = std::env::temp_dir().join(format!("bridge-app-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir), Prefs { auto_update: true, stockfish_offer_dismissed: None });
        let saved = Prefs { auto_update: false, stockfish_offer_dismissed: Some("0.2.0".into()) };
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
