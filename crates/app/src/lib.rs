//! The oschess bridge for Windows: a tray app around the bridge server, with a
//! status flyout, a settings window and a first-run window drawn in HTML.
//!
//! What does not touch the desktop lives in the modules below and is tested on
//! every system. `desktop` is the Tauri app and builds on Windows only.

pub mod i18n;
pub mod prefs;
pub mod settings;
pub mod status;

#[cfg(windows)]
pub mod desktop;
