//! The oschess bridge for Windows: a tray app around the bridge server, with a
//! status flyout, a settings window and a first-run window drawn in HTML.
//!
//! What does not touch the desktop lives in the modules below and is tested on
//! every system. `desktop` is the Tauri app and builds on Windows only.

pub mod channel;
pub mod choices;
pub mod i18n;
pub mod prefs;
pub mod settings;
pub mod status;
pub mod toast;
pub mod updates;

#[cfg(windows)]
pub mod desktop;

#[cfg(test)]
mod window_rules;

/// How long this crate's tests wait for what must come, such as another
/// thread's answer, before they fail: far longer than a loaded machine stalls
/// a thread, so that only what never comes, such as a deadlock, fails, and
/// fails by name (#238). The bridge's tests wait as long.
#[cfg(test)]
pub(crate) const PATIENCE: std::time::Duration = std::time::Duration::from_secs(300);
