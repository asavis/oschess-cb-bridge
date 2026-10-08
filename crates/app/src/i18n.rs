//! The app's words in Ukrainian and English, one dictionary per language in
//! `ui/i18n`, shared by the Rust side (tray, menu, notifications) and the
//! windows. The language follows the Windows display language: Ukrainian for
//! Ukrainian or Russian, English for any other.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Uk,
    En,
}

/// Primary language identifiers in a Windows language identifier (LANGID).
const LANG_UKRAINIAN: u16 = 0x22;
const LANG_RUSSIAN: u16 = 0x19;

impl Lang {
    /// The language for a Windows display-language identifier.
    pub fn from_langid(langid: u16) -> Lang {
        match langid & 0x3ff {
            LANG_UKRAINIAN | LANG_RUSSIAN => Lang::Uk,
            _ => Lang::En,
        }
    }

    /// The dictionary's file name in `ui/i18n`, and the windows' `lang` parameter.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Uk => "uk",
            Lang::En => "en",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Lang::Uk => include_str!("../ui/i18n/uk.json"),
            Lang::En => include_str!("../ui/i18n/en.json"),
        }
    }
}

pub struct Strings {
    lang: Lang,
    words: HashMap<String, String>,
}

impl Strings {
    pub fn new(lang: Lang) -> Strings {
        // The dictionaries are part of the program and tested to parse.
        let words = serde_json::from_str(lang.source()).unwrap_or_default();
        Strings { lang, words }
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// The text of `key`, or the key itself when it is missing.
    pub fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.words.get(key).map_or(key, String::as_str)
    }

    /// The text of `key` with each `{name}` replaced by its value.
    pub fn fill(&self, key: &str, values: &[(&str, &str)]) -> String {
        let mut text = self.get(key).to_string();
        for (name, value) in values {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }

    /// The form of `key` for the number `n` (`key.one`, `key.few` or
    /// `key.many`), with `{n}` and the other values filled in.
    pub fn plural(&self, key: &str, n: u64, values: &[(&str, &str)]) -> String {
        let form = format!("{key}.{}", plural_form(self.lang, n));
        let n = n.to_string();
        let mut all = vec![("n", n.as_str())];
        all.extend_from_slice(values);
        self.fill(&form, &all)
    }
}

/// Ukrainian has three forms: 1, 21 … (one); 2–4, 22–24 … (few); the rest
/// (many). English has two, kept as `one` and `many`.
pub fn plural_form(lang: Lang, n: u64) -> &'static str {
    match lang {
        Lang::En => {
            if n == 1 {
                "one"
            } else {
                "many"
            }
        }
        Lang::Uk => match (n % 10, n % 100) {
            (1, r) if r != 11 => "one",
            (2..=4, r) if !(12..=14).contains(&r) => "few",
            _ => "many",
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn keys(lang: Lang) -> BTreeSet<String> {
        let words: HashMap<String, String> = serde_json::from_str(lang.source()).expect("the dictionary parses");
        words.into_keys().collect()
    }

    #[test]
    fn both_languages_have_the_same_words() {
        let (uk, en) = (keys(Lang::Uk), keys(Lang::En));
        assert!(!uk.is_empty());
        assert_eq!(uk.difference(&en).collect::<Vec<_>>(), Vec::<&String>::new(), "only in uk.json");
        assert_eq!(en.difference(&uk).collect::<Vec<_>>(), Vec::<&String>::new(), "only in en.json");
    }

    #[test]
    fn every_plural_has_its_three_forms() {
        for key in keys(Lang::Uk) {
            if let Some(base) = key.strip_suffix(".one") {
                for form in ["few", "many"] {
                    assert!(keys(Lang::Uk).contains(&format!("{base}.{form}")), "{base}.{form}");
                }
            }
        }
    }

    /// Every key the windows name, in `data-i18n*` attributes and `t(…)` or
    /// `plural(…)` calls, and every key the Rust side names, in the calls that
    /// translate it and in the failures the commands answer (#186), is in
    /// both dictionaries, however it is spelled.
    #[test]
    fn every_key_in_use_exists() {
        let known: BTreeSet<String> = keys(Lang::Uk).intersection(&keys(Lang::En)).cloned().collect();
        let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        let mut used = BTreeSet::new();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for dir in [ui, src.clone(), src.join("desktop")] {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
                if !["html", "js", "rs"].contains(&ext) || path.ends_with("i18n.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                used.extend(keys_named(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display())));
            }
        }
        let missing: Vec<_> = used.iter().filter(|k| !known.contains(*k)).collect();
        assert!(missing.is_empty(), "keys in use but not in the dictionary: {missing:?}");
        assert!(used.len() > 40, "the scan found only {} keys", used.len());
        assert!(used.contains("settings.port.error"), "the scan reads the failures the commands answer");
    }

    /// The scan checks a key however it is spelled, and refuses a key it
    /// cannot read where it is translated: a failure's key misspelled, or a
    /// key held in a variable, passed unchecked before (#186).
    #[test]
    fn the_key_scan_misses_no_spelling() {
        let known = keys(Lang::Uk);
        let missing = |source: &str| -> Result<Vec<String>, String> {
            Ok(keys_named(source)?.into_iter().filter(|k| !known.contains(k)).collect())
        };
        assert_eq!(missing(r#"Err(Failure::new("settings.engine.refused"))"#), Ok(vec![]));
        for (source, key) in [
            (r#"Err(Failure::new("settings.engine.re_fused"))"#, "settings.engine.re_fused"),
            (r#"Failure::with("settings.error ", e.to_string())"#, "settings.error "),
            (r#"strings.get("taost.update.waiting.title")"#, "taost.update.waiting.title"),
            (r#"strings.fill("tray.port-busy", &[])"#, "tray.port-busy"),
            (r#"strings.plural("tray.re_ady", n, &[])"#, "tray.re_ady.few"),
            ("t('settings.engine.re_fused')", "settings.engine.re_fused"),
            ("plural('db.re_cords', n)", "db.re_cords.one"),
            (r#"<span data-i18n="settings.nav.en gine">"#, "settings.nav.en gine"),
        ] {
            assert!(missing(source).unwrap().iter().any(|k| k == key), "{source}");
        }
        let held = r#"let title = if mandatory { "taost.update.waiting.title" } else { "toast.update.waiting.title" };
            notify(app, strings.get(title).to_string());"#;
        assert!(keys_named(held).is_err(), "a key held in a variable is not read");
        assert!(keys_named("Failure::new(KEY)").is_err());
    }

    /// The keys `source`, a window's page or script or a Rust file, names
    /// where it translates them, as they are spelled. `Err` names a Rust call
    /// that translates a key given as anything but a literal: every key the
    /// Rust side uses is named where it is translated, so that this scan reads
    /// them all.
    fn keys_named(source: &str) -> Result<BTreeSet<String>, String> {
        let mut keys = BTreeSet::new();
        for call in ["data-i18n=\"", "data-i18n-title=\"", "data-i18n-label=\"", "t('"] {
            keys.extend(quoted_after(source, call));
        }
        for call in ["strings.get(", "strings.fill(", "Failure::new(", "Failure::with("] {
            keys.extend(literals_of(source, call)?);
        }
        for base in quoted_after(source, "plural('").into_iter().chain(literals_of(source, "strings.plural(")?) {
            keys.extend(["one", "few", "many"].map(|f| format!("{base}.{f}")));
        }
        Ok(keys)
    }

    /// The literal first argument of every Rust `call` in `source`; `Err`
    /// when one takes anything else.
    fn literals_of(source: &str, call: &str) -> Result<Vec<String>, String> {
        source
            .match_indices(call)
            .map(|(i, _)| {
                let rest = source[i + call.len()..].trim_start();
                let literal = rest.strip_prefix('"').and_then(|r| r.find('"').map(|end| r[..end].to_string()));
                literal.ok_or_else(|| {
                    let given: String = rest.chars().take_while(|&c| c != ')' && c != '\n').collect();
                    format!("{call}{given}): name the key where it is translated, as a literal")
                })
            })
            .collect()
    }

    /// The text between `start` and the next quote, for every `start` in
    /// `text` that begins a word, whatever that text is.
    fn quoted_after(text: &str, start: &str) -> Vec<String> {
        let end = if start.ends_with('\'') { '\'' } else { '"' };
        // `t(` and `plural(` are the windows' functions only as whole words.
        let word = matches!(start, "t('" | "plural('");
        text.match_indices(start)
            .filter(|(i, _)| {
                let before = text[..*i].chars().next_back();
                !word || !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            })
            .filter_map(|(i, _)| {
                let rest = &text[i + start.len()..];
                Some(rest[..rest.find(end)?].to_string())
            })
            .collect()
    }

    #[test]
    fn plural_forms() {
        let uk = |n| plural_form(Lang::Uk, n);
        assert_eq!([1, 21, 101].map(uk), ["one"; 3]);
        assert_eq!([2, 3, 4, 22, 104].map(uk), ["few"; 5]);
        assert_eq!([0, 5, 11, 12, 14, 25, 111].map(uk), ["many"; 7]);
        assert_eq!([1, 0, 2, 21].map(|n| plural_form(Lang::En, n)), ["one", "many", "many", "many"]);
    }

    #[test]
    fn language_by_display_language() {
        for langid in [0x0022, 0x0422, 0x0019, 0x0419, 0x0819] {
            assert_eq!(Lang::from_langid(langid), Lang::Uk, "LANGID {langid:#06x}");
        }
        for langid in [0x0000, 0x0409, 0x0809, 0x0407, 0x040a, 0x040c, 0x0415, 0xffff] {
            assert_eq!(Lang::from_langid(langid), Lang::En, "LANGID {langid:#06x}");
        }
    }

    #[test]
    fn russian_windows_use_ukrainian_strings() {
        for langid in [0x0419, 0x0819] {
            let strings = Strings::new(Lang::from_langid(langid));
            assert_eq!(strings.lang().code(), "uk");
            assert_eq!(strings.get("window.settings"), "Налаштування — oschess міст");
            assert_eq!(strings.get("menu.quit"), "Вийти");
            assert_eq!(strings.get("toast.stopped.title"), "Міст не працює");
            assert_eq!(strings.plural("tray.ready", 22, &[]), "oschess міст — 22 бази готові");
        }
    }

    #[test]
    fn filling() {
        let s = Strings::new(Lang::Uk);
        assert_eq!(s.plural("db.records", 3, &[]), "3 партії");
        assert_eq!(s.fill("tray.portBusy", &[("port", "39581")]), "oschess міст — порт 39581 зайнятий");
        assert_eq!(s.get("no.such.key"), "no.such.key");
        let e = Strings::new(Lang::En);
        assert_eq!(e.plural("db.records", 1, &[]), "1 game");
        assert_eq!(e.plural("db.records", 12, &[]), "12 games");
    }
}
