//! The first-run window hears a connection that arrives while it starts
//! (#130). No JavaScript runs in these tests, so they pin the two orderings
//! the handoff needs:
//!
//! - the window subscribes to `view` before it reads the view;
//! - the app stores each view before it announces it.
//!
//! Reversing either loses a connection that lands in between, and the window
//! then spins until it is closed.

use std::path::Path;

fn source(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).expect(path)
}

fn position(text: &str, needle: &str) -> usize {
    let at = text.find(needle).unwrap_or_else(|| panic!("{needle:?} is missing"));
    assert_eq!(text.matches(needle).count(), 1, "{needle:?} appears more than once");
    at
}

#[test]
fn the_window_subscribes_before_it_reads_the_view() {
    let js = source("ui/first-run.js");
    assert!(position(&js, "await on('view', showConnected);") < position(&js, "showConnected(await call('view'));"));
}

#[test]
fn the_app_stores_a_view_before_it_announces_it() {
    let rust = source("src/desktop/mod.rs");
    assert!(position(&rust, "shared.set_view(view.clone());") < position(&rust, "app.emit(\"view\", &view);"));
}
