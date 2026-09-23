//! The oschess bridge: serves the ChessBase databases of this machine to the
//! oschess web app over HTTP on the loopback interface, as `docs/api.md`
//! specifies.

pub mod access;
pub mod api;
pub mod autostart;
#[cfg(windows)]
pub mod browser;
pub mod budget;
pub mod catalog;
pub mod config;
pub mod http;
pub mod instance;
pub mod json;
pub mod pairing;
pub mod reply;
pub mod server;
pub mod snapshot;
pub mod start;
pub mod token;

/// `s` as a NUL-terminated UTF-16 string, as Win32 functions take text.
#[cfg(windows)]
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
