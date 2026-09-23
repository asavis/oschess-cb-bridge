//! On Windows, building a WebView on the thread that runs the event loop
//! deadlocks, and synchronous commands, menu handlers and tray handlers all
//! run there. The desktop code builds every window in one function, called
//! only from a worker thread. The desktop code compiles on Windows only, so
//! these tests read its source, and they run on every system.

use std::path::Path;

/// The desktop sources by file name, without their comment lines.
fn sources() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/desktop");
    let code = |text: String| -> String {
        text.lines().filter(|l| !l.trim_start().starts_with("//")).map(|l| format!("{l}\n")).collect()
    };
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), code(std::fs::read_to_string(e.path()).unwrap())))
        .collect();
    files.sort();
    assert!(files.len() >= 5, "the desktop sources were not found");
    files
}

/// The body of `fn name` in `text`, braces included; `None` when there is none.
fn body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let at = text.find(&format!("fn {name}("))?;
    let open = at + text[at..].find('{')?;
    let mut depth = 0;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
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

fn count(files: &[(String, String)], needle: &str) -> Vec<(String, usize)> {
    files.iter().map(|(n, t)| (n.clone(), t.matches(needle).count())).filter(|(_, c)| *c > 0).collect()
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
    for (at, _) in commands.match_indices("#[tauri::command]") {
        let rest = &commands[at..];
        let signature = &rest[..rest.find('{').unwrap()];
        let name_at = signature.find("fn ").unwrap() + 3;
        let name = &signature[name_at..name_at + signature[name_at..].find('(').unwrap()];
        let opens = body(rest, name).unwrap().contains("windows::open_");
        if opens {
            assert!(signature.contains("async fn"), "the command {name} opens a window but is not async");
            checked += 1;
        }
    }
    assert!(checked >= 1, "no command that opens a window was found");
}
