//! The oschess bridge: serves the ChessBase databases of this machine to the
//! oschess web app over HTTP on the loopback interface, as `docs/api.md`
//! specifies.

pub mod access;
pub mod api;
pub mod budget;
pub mod catalog;
pub mod config;
pub mod documents;
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
pub mod token;
