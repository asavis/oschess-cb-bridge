//! The oschess bridge: serves the ChessBase databases of this machine to the
//! oschess web app over HTTP on the loopback interface, as `docs/api.md`
//! specifies.

pub mod access;

/// The stack of every thread the bridge starts: 1 MiB, half the standard
/// library's default. Nothing a thread runs recurses with the data (move
/// trees, PGN and index merges keep their depth on the heap), and each
/// thread's stack is address space, which a capped process runs short of.
pub const THREAD_STACK: usize = 1 << 20;

pub mod api;
pub mod budget;
pub mod catalog;
pub mod config;
pub mod documents;
pub mod explorer;
pub mod fetch;
pub mod http;
pub mod json;
pub mod pairing;
pub mod reply;
pub mod search;
pub mod server;
pub mod snapshot;
pub mod sources;
pub mod start;
pub mod store;
pub mod token;
