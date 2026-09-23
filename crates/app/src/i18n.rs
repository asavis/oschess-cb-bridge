//! The app's words in Ukrainian and English, one dictionary per language in
//! `ui/i18n`, shared by the Rust side (tray, menu, notifications) and the
//! windows. The language follows the Windows display language: Ukrainian for
//! Ukrainian, English for any other.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Uk,
    En,
}

/// The primary language of a Windows language identifier (LANGID) for Ukrainian.
const LANG_UKRAINIAN: u16 = 0x22;

impl Lang {
    /// The language for a Windows display-language identifier.
    pub fn from_langid(langid: u16) -> Lang {
        if langid & 0x3ff == LANG_UKRAINIAN { Lang::Uk } else { Lang::En }
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
    /// `plural(…)` calls, and every key the Rust side names, is in the dictionary.
    #[test]
    fn every_key_in_use_exists() {
        let known = keys(Lang::Uk);
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
                for call in ["data-i18n=\"", "data-i18n-title=\"", "data-i18n-label=\"", "t('"] {
                    used.extend(quoted_after(&text, call));
                }
                for call in ["strings.get(\"", "strings.fill(\""] {
                    used.extend(quoted_after(&text, call));
                }
                for call in ["plural('", "strings.plural(\""] {
                    for base in quoted_after(&text, call) {
                        used.extend(["one", "few", "many"].map(|f| format!("{base}.{f}")));
                    }
                }
            }
        }
        let missing: Vec<_> = used.iter().filter(|k| !known.contains(*k)).collect();
        assert!(missing.is_empty(), "keys in use but not in the dictionary: {missing:?}");
        assert!(used.len() > 40, "the scan found only {} keys", used.len());
    }

    /// The text between `start` and the next quote, for every `start` in `text`
    /// that begins a word and is followed by a plain dotted key.
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
                let key = &rest[..rest.find(end)?];
                let plain = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
                (plain && key.contains('.')).then(|| key.to_string())
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
        assert_eq!(Lang::from_langid(0x0422), Lang::Uk);
        assert_eq!(Lang::from_langid(0x0409), Lang::En);
        assert_eq!(Lang::from_langid(0x0419), Lang::En);
        assert_eq!(Lang::from_langid(0x0415), Lang::En);
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
