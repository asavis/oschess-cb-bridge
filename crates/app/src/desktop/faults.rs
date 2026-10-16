//! Failures to cause by hand in a debug build, to see what a user would see
//! and what the log keeps (#332):
//!
//! ```text
//! set OSCHESS_BRIDGE_FAULT=build-hangs,show-fails
//! ```
//!
//! - `build-hangs`: a window's build never finishes;
//! - `build-fails`: a window's build fails;
//! - `page-not-ready`: no page says it is ready;
//! - `show-fails`: the settings window cannot be brought forward;
//! - `view-blocks`: the command `view`, which the flyout calls as it loads,
//!   holds the event loop for 20 s.
//!
//! A release build has none of them.

/// Whether the fault `name` is asked for.
pub fn on(name: &str) -> bool {
    cfg!(debug_assertions)
        && std::env::var("OSCHESS_BRIDGE_FAULT").is_ok_and(|faults| faults.split(',').any(|f| f.trim() == name))
}
