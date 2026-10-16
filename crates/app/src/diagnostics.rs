//! The log lines of failures that used to vanish (#332): panics, which the
//! tray app's missing standard error swallowed, and errors in the windows'
//! scripts, which reached no log at all. The log names no file of the user's
//! (#117), so text that may carry a path has it replaced by `<path>`.

use std::collections::HashMap;
use std::sync::Mutex;

/// What stands in for a path in the log.
const PATH: &str = "<path>";

/// The longest message of a page's script the log keeps, in characters.
const MESSAGE_CHARS: usize = 300;

/// How many script errors of one window a run logs.
pub const PAGE_ERRORS: u32 = 20;

/// `text` with every path in it replaced by `<path>`: from a drive (`C:\`,
/// `C:/`), a UNC or escaped start (`\\`), or a `/` that opens a word, to the
/// next colon, quote or end of the line. A path may hold spaces and
/// brackets, so they do not end it; a Windows path holds no colon but its
/// drive's, so the reason after `path: ` stays.
pub fn without_paths(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if path_starts(&chars, i) {
            out.push_str(PATH);
            let start = i;
            let ends = |at: usize, c: char| matches!(c, '"' | '\'' | '`' | '\n') || (c == ':' && at != start + 1);
            while i < chars.len() && !ends(i, chars[i]) {
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn path_starts(chars: &[char], i: usize) -> bool {
    let at = |n: usize| chars.get(i + n).copied();
    let word_start = i == 0 || !chars[i - 1].is_alphanumeric();
    let drive = chars[i].is_ascii_alphabetic() && at(1) == Some(':') && matches!(at(2), Some('\\' | '/'));
    let unc = chars[i] == '\\' && at(1) == Some('\\');
    let rooted = chars[i] == '/' && at(1).is_some_and(|c| c.is_alphanumeric() || c == '.') && word_start;
    (drive && word_start) || unc || rooted
}

/// A source file as a panic names it, short enough to name no folder of the
/// machine that built it: a dependency's from its crate's folder on, this
/// workspace's as it is (relative), any other by its last two parts.
pub fn source_file(file: &str) -> String {
    let parts: Vec<&str> = file.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    // `…/registry/src/<index>/<crate>-<version>/src/lib.rs`
    if let Some(at) = parts.windows(2).position(|w| w == ["registry", "src"])
        && parts.len() > at + 3
    {
        return parts[at + 3..].join("/");
    }
    // `…/git/checkouts/<repository>-<hash>/<revision>/…`
    if let Some(at) = parts.windows(2).position(|w| w == ["git", "checkouts"])
        && parts.len() > at + 4
    {
        return parts[at + 2..].join("/");
    }
    let absolute = file.starts_with(['/', '\\']) || file.as_bytes().get(1) == Some(&b':');
    if absolute { parts[parts.len().saturating_sub(2)..].join("/") } else { parts.join("/") }
}

/// The log line of a panic in the thread `thread` at `location` (file and
/// line), with its message.
pub fn panic_line(thread: Option<&str>, location: Option<(&str, u32)>, message: &str) -> String {
    let thread = thread.unwrap_or("unnamed");
    let at = location.map_or_else(|| "an unknown place".to_string(), |(f, l)| format!("{}:{l}", source_file(f)));
    // Paths go line by line, before the lines are joined: one runs to its line's end.
    format!("panic in the thread {thread} at {at}: {}", one_line(&without_paths(message)))
}

/// The log line of an error in the script of the window `label`: its
/// message, shortened, and the script and line it names.
pub fn page_error_line(label: &str, message: &str, source: Option<&str>, line: Option<u32>) -> String {
    let message: String = one_line(&without_paths(message)).chars().take(MESSAGE_CHARS).collect();
    // A script is named by its file: `http://tauri.localhost/settings.js` is `settings.js`.
    let script = source.and_then(|s| s.rsplit('/').next()).filter(|s| !s.is_empty());
    let at = match (script, line) {
        (Some(s), Some(l)) => format!(" at {}:{l}", without_paths(s)),
        (Some(s), None) => format!(" in {}", without_paths(s)),
        _ => String::new(),
    };
    format!("the {label} page: {message}{at}")
}

fn one_line(text: &str) -> String {
    text.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" / ")
}

/// Whether a window's script error is logged: a page that throws in a loop
/// must not fill the log, whose size is capped.
#[derive(Debug, PartialEq, Eq)]
pub enum Admit {
    Yes,
    /// The last one logged; the log says the rest are left out.
    Last,
    No,
}

/// The script errors each window logged in this run.
#[derive(Default)]
pub struct PageErrors {
    counts: Mutex<HashMap<String, u32>>,
}

impl PageErrors {
    pub fn admit(&self, label: &str) -> Admit {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.entry(label.to_string()).or_default();
        *count = count.saturating_add(1);
        match (*count).cmp(&PAGE_ERRORS) {
            std::cmp::Ordering::Less => Admit::Yes,
            std::cmp::Ordering::Equal => Admit::Last,
            std::cmp::Ordering::Greater => Admit::No,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_leave_the_text() {
        let cases = [
            (r"cannot open C:\Users\Олена Петренко\Documents\ChessBase\Mega 2024.cbh", "cannot open <path>"),
            (r#"Os { message: "C:\\Users\\Olena\\bridge.toml" } at once"#, r#"Os { message: "<path>" } at once"#),
            (r"read \\server\share\db.pgn: denied", "read <path>: denied"),
            (
                r"cannot open C:\x\Mega 2024.cbh: Access is denied. (os error 5)",
                "cannot open <path>: Access is denied. (os error 5)",
            ),
            ("no file /home/olena/.config/oschess-bridge/bridge.toml", "no file <path>"),
            (r"c:/Program Files (x86)/ChessBase/Mega.cbh", "<path>"),
            ("line one C:\\a\nline two", "line one <path>\nline two"),
        ];
        for (text, expected) in cases {
            assert_eq!(without_paths(text), expected, "{text}");
        }
    }

    #[test]
    fn text_without_paths_stays() {
        for text in [
            "index out of bounds: the len is 3 but the index is 7",
            "called `Result::unwrap()` on an `Err` value: 3/4 done, ratio 1:2",
            "TypeError: Cannot read properties of null (reading 'append')",
        ] {
            assert_eq!(without_paths(text), text);
        }
    }

    #[test]
    fn a_panic_names_no_folder_of_the_build_machine() {
        let dependency = r"C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\tao-0.34.5\src\platform_impl\windows\event_loop.rs";
        assert_eq!(source_file(dependency), "tao-0.34.5/src/platform_impl/windows/event_loop.rs");
        assert_eq!(source_file(r"crates\app\src\desktop\windows.rs"), "crates/app/src/desktop/windows.rs");
        assert_eq!(source_file("/rustc/abc/library/core/src/option.rs"), "src/option.rs");
        let git = "/home/u/.cargo/git/checkouts/wry-1a2b3c/0d9e8f7/src/lib.rs";
        assert_eq!(source_file(git), "wry-1a2b3c/0d9e8f7/src/lib.rs");
    }

    #[test]
    fn the_panic_line() {
        let line = panic_line(
            Some("tokio-runtime-worker"),
            Some((r"crates\app\src\desktop\windows.rs", 71)),
            "cannot read C:\\Users\\Olena\\app.json\nsecond line",
        );
        assert_eq!(
            line,
            "panic in the thread tokio-runtime-worker at crates/app/src/desktop/windows.rs:71: cannot read <path> / second line"
        );
        assert_eq!(panic_line(None, None, "boom"), "panic in the thread unnamed at an unknown place: boom");
    }

    #[test]
    fn the_page_error_line() {
        let line =
            page_error_line("settings", "TypeError: x is null", Some("http://tauri.localhost/settings.js"), Some(48));
        assert_eq!(line, "the settings page: TypeError: x is null at settings.js:48");
        assert_eq!(
            page_error_line("flyout", "failure settings.error", None, None),
            "the flyout page: failure settings.error"
        );
        let long = "x".repeat(1000);
        assert_eq!(page_error_line("flyout", &long, None, None).len(), "the flyout page: ".len() + MESSAGE_CHARS);
    }

    #[test]
    fn a_page_logs_a_bounded_number_of_errors() {
        let errors = PageErrors::default();
        let admitted: Vec<Admit> = (0..PAGE_ERRORS + 2).map(|_| errors.admit("settings")).collect();
        assert!(admitted[..PAGE_ERRORS as usize - 1].iter().all(|a| *a == Admit::Yes));
        assert_eq!(admitted[PAGE_ERRORS as usize - 1], Admit::Last);
        assert!(admitted[PAGE_ERRORS as usize..].iter().all(|a| *a == Admit::No));
        assert_eq!(errors.admit("flyout"), Admit::Yes, "each window has its own count");
    }
}
