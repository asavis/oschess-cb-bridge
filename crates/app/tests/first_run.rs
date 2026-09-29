//! The first-run wizard (#201) hears a connection that arrives while it starts
//! (#130), and opens oschess only from its last screen. No JavaScript runs in
//! these tests, so they pin what the sources must keep:
//!
//! - the window subscribes to `view` before it reads the view;
//! - the app stores each view before it announces it;
//! - the start opens no browser on the first run, and the wizard opens the
//!   pairing link only from its last screen.
//!
//! Reversing either of the first two loses a connection that lands in
//! between, and the window then spins until it is closed. Opening the browser
//! at the start would put oschess over the wizard's first step.

use std::path::Path;

/// A file of the app's crate with its lines ending in `\n`: a Windows checkout
/// ends them in `\r\n`.
fn source(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).expect(path).replace("\r\n", "\n")
}

fn position(text: &str, needle: &str) -> usize {
    let at = text.find(needle).unwrap_or_else(|| panic!("{needle:?} is missing"));
    assert_eq!(text.matches(needle).count(), 1, "{needle:?} appears more than once");
    at
}

/// The body of the JavaScript function that `start` opens, up to its closing
/// brace at the start of a line.
fn function<'a>(js: &'a str, start: &str) -> &'a str {
    let rest = &js[position(js, start)..];
    &rest[..rest.find("\n}\n").unwrap_or_else(|| panic!("the end of {start:?}"))]
}

#[test]
fn the_window_subscribes_before_it_reads_the_view() {
    let js = source("ui/first-run.js");
    assert!(position(&js, "await on('view', showView);") < position(&js, "showView(await call('view'));"));
}

#[test]
fn the_app_stores_a_view_before_it_announces_it() {
    let rust = source("src/desktop/mod.rs");
    assert!(position(&rust, "shared.set_view(view.clone());") < position(&rust, "app.emit(\"view\", &view);"));
}

#[test]
fn the_first_run_pairs_from_the_wizard_s_last_screen() {
    // Only a new code or a new port pairs at the start.
    position(&source("src/desktop/server.rs"), "Started { shared, first_run, pair: asked }");
    let js = source("ui/first-run.js");
    assert!(function(&js, "function pair() {").contains("call('open_pairing')"), "the last screen opens the link");
    // That, and the last screen's «Open oschess» button; no step opens it.
    assert_eq!(js.matches("call('open_pairing')").count(), 2);
    assert!(function(&js, "function show(name) {").contains("if (last) pair();"), "pair() runs on the last screen");
    assert_eq!(js.matches("pair();").count(), 1, "and only there");
}

/// Nothing that changes the engine works before the engine folders' first
/// answer: that answer, landing after a choice made meanwhile, would replace
/// it, and «Next» would then save the found engine over the chosen one.
#[test]
fn the_engine_controls_wait_for_the_engine_folders() {
    let js = source("ui/first-run.js");
    let controls = function(&js, "function renderEngineControls() {");
    assert!(controls.contains("const locked = busy || Boolean(enginesRead);"));
    for control in ["#engines input", "'install-stockfish'", "'pick-other'", "'pick-engine'"] {
        assert!(controls.contains(control), "{control} waits too");
    }
    assert!(function(&js, "function engineRow(engine) {").contains("radio.disabled = busy || Boolean(enginesRead);"));
    let main = function(&js, "async function main() {");
    let settled = main.find("enginesRead = null;").expect("the answer settles");
    assert!(main[settled..].contains("renderEngineControls();"), "the controls open once it settled");
}

/// «Skip setup» saves what the steps would have: it goes through the same
/// completion as «Done», which waits for the engine folders first (#201).
#[test]
fn skipping_applies_the_defaults() {
    let js = source("ui/first-run.js");
    position(&js, "getElementById('skip').addEventListener('click', () => work(complete));");
    let complete = function(&js, "async function complete() {");
    let waits = complete.find("await enginesRead;").expect("complete() waits for the engine folders");
    assert!(waits < complete.find("await chooseMarked();").expect("then saves the marked engine"));
    assert!(complete.contains("await applyStartup();") && complete.ends_with("show('done');"));
}
