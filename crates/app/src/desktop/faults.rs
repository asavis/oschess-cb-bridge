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
//! - `view-blocks`: the command `view`, which every page calls as it loads,
//!   holds the event loop for 20 s;
//! - `view-fails`: the command `view` fails, and with it each page's start,
//!   as a script error would.
//!
//! A release build has none of them.

/// Whether the fault `name` is asked for.
#[cfg(debug_assertions)]
pub fn on(name: &str) -> bool {
    std::env::var("OSCHESS_BRIDGE_FAULT").is_ok_and(|faults| faults.split(',').any(|f| f.trim() == name))
}

#[cfg(not(debug_assertions))]
pub fn on(_name: &str) -> bool {
    false
}
