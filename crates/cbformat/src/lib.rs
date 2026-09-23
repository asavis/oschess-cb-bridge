//! Readers for ChessBase database files.
//!
//! [`v2`] reads the `.2cbh` family written by ChessBase 17 and later.
//! [`replay`] plays decoded moves on a [`cozy_chess::Board`], checking each
//! move word against the position.

use std::path::PathBuf;

pub mod movetable;
pub mod pgn;
pub mod replay;
pub mod v2;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("format error: {0}")]
    Format(String),
    #[error("no game with id {0}")]
    NoSuchGame(u32),
    #[error("move {ply}: {reason}")]
    Move { ply: u32, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;
