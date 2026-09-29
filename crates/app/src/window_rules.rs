//! On Windows, building a WebView on the thread that runs the event loop
//! deadlocks, and synchronous commands, menu handlers and tray handlers all
//! run there. The desktop code builds every window in one function, called
//! only from a worker thread. The desktop code compiles on Windows only, so
//! these tests read its source, and they run on every system.
//!
//! They also tie together the lists kept by hand around that code, which
//! would otherwise disagree only at run time on Windows: the commands, which
//! `build.rs`, the handler, the command functions and each window's
//! capability all name, and the tray marks built into the app.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::status::{Theme, Tray, icon_file, icon_size};

/// A file of the app's crate, by its path from the crate's folder, with
/// its lines ending in `\n`: a Windows checkout ends them in `\r\n`.
fn app_file(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).expect(path).replace("\r\n", "\n")
}

/// `text` without its comment lines.
fn code(text: &str) -> String {
    text.lines().filter(|l| !l.trim_start().starts_with("//")).map(|l| format!("{l}\n")).collect()
}

/// The desktop sources by file name, without their comment lines.
fn sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/desktop");
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), code(&std::fs::read_to_string(e.path()).unwrap())))
        .collect();
    files.sort();
    assert!(files.len() >= 5, "the desktop sources were not found");
    files
}

/// The desktop source `name` in `files`.
fn source<'a>(files: &'a [(String, String)], name: &str) -> &'a str {
    &files.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("desktop/{name} was not found")).1
}

/// The body of `fn name` in `text`, braces included; `None` when there is none.
/// Comments and string and character literals are skipped, so that neither a
/// brace nor a `fn name(` in them counts.
fn body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let code = blanked(text);
    let at = code.find(&format!("fn {name}("))?;
    let open = at + code[at..].find('{')?;
    let mut depth = 0;
    for (i, c) in code[open..].bytes().enumerate() {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open..=open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `text` with its comments and its string and character literals blanked
/// out byte for byte, so that every offset in it is an offset in `text`.
fn blanked(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let len = match rest {
            [b'/', b'/', ..] => rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len()),
            [b'/', b'*', ..] => block_comment(rest),
            [b'"', ..] => string(rest, raw_hashes(&bytes[..at])),
            // An escape: '\n', '\'', '\u{7d}'.
            [b'\'', b'\\', ..] => rest.iter().skip(3).position(|&c| c == b'\'').map_or(rest.len(), |n| n + 4),
            // One character of one to four bytes; else a lifetime or a label.
            [b'\'', _, ..] => {
                let width = 1 + rest[2..].iter().take_while(|&&c| c & 0xc0 == 0x80).count();
                if rest.get(1 + width) == Some(&b'\'') { width + 2 } else { 0 }
            }
            _ => 0,
        };
        out[at..at + len].fill(b' ');
        at += len.max(1);
    }
    String::from_utf8(out).expect("only whole characters are blanked")
}

/// The length of the block comment `rest` opens with, nested ones included.
fn block_comment(rest: &[u8]) -> usize {
    let (mut depth, mut i) = (0, 0);
    while i < rest.len() {
        if rest[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if rest[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    rest.len()
}

/// How many `#` open the raw string whose quote follows `before`; `None` for
/// a string that is not raw.
fn raw_hashes(before: &[u8]) -> Option<usize> {
    let hashes = before.iter().rev().take_while(|&&c| c == b'#').count();
    (before.len() > hashes && before[before.len() - hashes - 1] == b'r').then_some(hashes)
}

/// The length of the string literal `rest` opens with, from its first quote.
fn string(rest: &[u8], raw_hashes: Option<usize>) -> usize {
    if let Some(hashes) = raw_hashes {
        let close = [&b"\""[..], &b"#".repeat(hashes)].concat();
        return rest[1..].windows(close.len()).position(|w| w == close).map_or(rest.len(), |n| 1 + n + close.len());
    }
    let mut i = 1;
    while i < rest.len() {
        match rest[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    rest.len()
}

fn count(files: &[(String, String)], needle: &str) -> Vec<(String, usize)> {
    files.iter().map(|(n, t)| (n.clone(), t.matches(needle).count())).filter(|(_, c)| *c > 0).collect()
}

/// The `#[tauri::command]` functions in `text`: each one's name, its
/// signature, and the text from its attribute on.
fn command_fns(text: &str) -> Vec<(&str, &str, &str)> {
    text.match_indices("#[tauri::command")
        .map(|(at, _)| {
            let rest = &text[at..];
            let signature = &rest[..rest.find('{').unwrap()];
            let name_at = signature.find("fn ").unwrap() + 3;
            let name = &signature[name_at..name_at + signature[name_at..].find('(').unwrap()];
            (name, signature, rest)
        })
        .collect()
}

/// The text between `start` and the first `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text.find(start).unwrap_or_else(|| panic!("{start:?} was not found")) + start.len();
    let to = text[from..].find(end).unwrap_or_else(|| panic!("{end:?} does not follow {start:?}"));
    &text[from..from + to]
}

/// The double-quoted strings in `text`, which has no escaped quote.
fn quoted(text: &str) -> Vec<String> {
    text.split('"').skip(1).step_by(2).map(String::from).collect()
}

/// `list` sorted, after checking that it names each thing once.
#[track_caller]
fn once(what: &str, mut list: Vec<String>) -> Vec<String> {
    list.sort();
    let twice: Vec<&String> = list.windows(2).filter(|w| w[0] == w[1]).map(|w| &w[0]).collect();
    assert!(twice.is_empty(), "{what} names {twice:?} twice");
    list
}

/// Checks that two named lists name the same things, each once, in any order.
#[track_caller]
fn assert_same(left: (&str, Vec<String>), right: (&str, Vec<String>)) {
    let (a, b) = (once(left.0, left.1), once(right.0, right.1));
    let only = |x: &[String], y: &[String]| -> Vec<String> { x.iter().filter(|n| !y.contains(n)).cloned().collect() };
    assert!(
        a == b,
        "{} and {} differ: {:?} only in the first, {:?} only in the second",
        left.0,
        right.0,
        only(&a, &b),
        only(&b, &a)
    );
}

/// Each capability file's window, and the app's commands it allows: its
/// `allow-<command>` permissions, where Tauri writes the command's `_` as `-`.
fn capabilities() -> Vec<(String, Vec<String>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
    let mut found: Vec<(String, Vec<String>)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| {
            let file: Value = serde_json::from_str(&std::fs::read_to_string(e.path()).unwrap()).unwrap();
            let windows: Vec<&str> = file["windows"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
            let [window] = windows[..] else { panic!("{:?} is not for one window", e.file_name()) };
            let permissions = file["permissions"].as_array().unwrap().iter().filter_map(Value::as_str);
            let commands = permissions
                .filter(|p| !p.contains(':'))
                .filter_map(|p| p.strip_prefix("allow-"))
                .map(|c| c.replace('-', "_"))
                .collect();
            (window.to_string(), commands)
        })
        .collect();
    found.sort();
    assert!(found.len() >= 3, "the capabilities were not found");
    found
}

#[test]
fn windows_are_built_in_one_place() {
    let files = sources();
    assert_eq!(count(&files, "WebviewWindowBuilder::"), [("windows.rs".to_string(), 1)]);
    let windows = &files.iter().find(|(n, _)| n == "windows.rs").unwrap().1;
    assert!(body(windows, "build_window").unwrap().contains("WebviewWindowBuilder::new("));
}

#[test]
fn windows_are_built_off_the_event_loop() {
    let files = sources();
    // The definition and one call.
    assert_eq!(count(&files, "build_window("), [("windows.rs".to_string(), 2)]);
    let windows = &files.iter().find(|(n, _)| n == "windows.rs").unwrap().1;
    let spawn = body(windows, "spawn_window").unwrap();
    let task = &spawn[spawn.find("tauri::async_runtime::spawn(").expect("spawn_window uses a worker")..];
    assert!(task.contains("build_window("), "build_window is called inside the spawned task");
}

/// A command that may open a window is asynchronous, so it does not run on the
/// event loop's thread in the first place.
#[test]
fn commands_that_open_windows_are_async() {
    let files = sources();
    let commands = &files.iter().find(|(n, _)| n == "commands.rs").unwrap().1;
    let mut checked = 0;
    for (name, signature, rest) in command_fns(commands) {
        let opens = body(rest, name).unwrap().contains("windows::open_");
        if opens {
            assert!(signature.contains("async fn"), "the command {name} opens a window but is not async");
            checked += 1;
        }
    }
    assert!(checked >= 1, "no command that opens a window was found");
}

/// Tauri makes a permission only for a command in `COMMANDS` in `build.rs`,
/// the handler in `desktop/mod.rs` runs only the functions it names, and a
/// window may call only what its capability allows. A command left out of one
/// of them fails only when a window calls it, so they all name the same ones.
#[test]
fn every_command_is_built_handled_and_allowed() {
    let files = sources();
    let built = quoted(between(&code(&app_file("build.rs")), "const COMMANDS: &[&str] = &[", "];"));
    let handled: Vec<String> = between(source(&files, "mod.rs"), "tauri::generate_handler![", "]")
        .split(',')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(String::from)
        .collect();
    let mut defined = Vec::new();
    for (file, text) in &files {
        let module = file.trim_end_matches(".rs");
        defined.extend(command_fns(text).into_iter().map(|(name, _, _)| format!("{module}::{name}")));
    }
    assert_same(("the handler in desktop/mod.rs", handled.clone()), ("the #[tauri::command] functions", defined));
    let handled = handled.iter().map(|path| path.rsplit("::").next().unwrap().to_string()).collect();
    assert_same(("COMMANDS in build.rs", built.clone()), ("the handler in desktop/mod.rs", handled));
    let allowed: BTreeSet<String> = capabilities().into_iter().flat_map(|(_, commands)| commands).collect();
    assert_same(("COMMANDS in build.rs", built), ("the capabilities together", allowed.into_iter().collect()));
}

/// Each window's capability allows exactly the commands its page calls: a
/// command it lacks fails when the page calls it, and one the page does not
/// call only widens what the window may do.
#[test]
fn each_window_is_allowed_the_commands_its_page_calls() {
    for (window, allowed) in capabilities() {
        // Each window shows `ui/<its label>.html` (desktop/windows.rs).
        let page = format!("ui/{window}.html");
        let mut called = BTreeSet::new();
        for script in app_file(&page).split("<script src=\"").skip(1).filter_map(|s| s.split('"').next()) {
            let js = app_file(&format!("ui/{script}"));
            let written_out = js.matches("call('").count() + js.matches("function call(").count();
            assert_eq!(js.matches("call(").count(), written_out, "ui/{script} calls a command it does not name");
            called.extend(js.split("call('").skip(1).filter_map(|call| call.split('\'').next()).map(String::from));
        }
        let capability = format!("the capability of the {window} window");
        assert_same((&capability, allowed), (&format!("the calls in {page}"), called.into_iter().collect()));
    }
}

/// The tray marks built into the app, `marks!` in `desktop/tray.rs`, are one
/// for each theme, state and size: the files `status::icon_file` names for the
/// sizes `status::icon_size` picks, and the ones `icons/generate.py` draws. A
/// mark left out leaves the tray without an icon in that state.
#[test]
fn a_mark_is_built_in_for_every_theme_state_and_size() {
    let files = sources();
    let built = quoted(between(source(&files, "tray.rs"), "marks!(", ");"));
    // Matched, so that a new theme or state does not compile until it is
    // listed here.
    let themes = [Theme::Light, Theme::Dark].map(|t| match t {
        Theme::Light | Theme::Dark => t,
    });
    let states = [Tray::Ready, Tray::Attention, Tray::Problem].map(|t| match t {
        Tray::Ready | Tray::Attention | Tray::Problem => t,
    });
    // The display scales Windows offers, 100 % to 500 %, by 5 %.
    let sizes: BTreeSet<u32> = (20..=100).map(|n| icon_size(f64::from(n) / 20.0)).collect();
    let mut named = Vec::new();
    for theme in themes {
        for state in states {
            named.extend(sizes.iter().map(|&size| icon_file(theme, state, size)));
        }
    }
    assert_same(("marks! in desktop/tray.rs", built), ("the files status.rs names", named.clone()));

    let python = app_file("icons/generate.py");
    let drawn_sizes: Vec<&str> = between(&python, "\nTRAY_SIZES = (", ")").split(',').map(str::trim).collect();
    let mut drawn = Vec::new();
    for line in between(&python, "\nTRAY = {\n", "\n}").lines() {
        // "light": {"ready": "#1c1c1c", "attention": "#b86e00", ...},
        let words = quoted(line);
        let (theme, states_and_colours) = words.split_first().expect("a theme on each line");
        for state in states_and_colours.iter().step_by(2) {
            drawn.extend(drawn_sizes.iter().map(|size| format!("{theme}-{state}-{size}.png")));
        }
    }
    assert_same(("the files status.rs names", named), ("the marks icons/generate.py draws", drawn));
}

/// A brace in a comment, a string or a character does not end a body, and a
/// function named in a comment or a string is not the function.
#[test]
fn a_body_skips_comments_and_literals() {
    let text = r##"// fn f() {}
const S: &str = "fn f() {";
fn f(x: &'static str) -> char {
    let _ = ("}\"}", r#"}"}"#, b'}', '\'', '\u{7d}', "\\", '«'); // }
    'l: loop { break 'l; }
    /* } /* nested } */ } */
    '}'
}
fn g() {}
"##;
    let start = text.find("{\n    let").unwrap();
    assert_eq!(body(text, "f"), Some(&text[start..text.find("\nfn g").unwrap()]));
    assert_eq!(body(text, "g"), Some("{}"));
    assert_eq!(body(text, "h"), None);
}
