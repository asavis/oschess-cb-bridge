//! Readers for ChessBase database files.
//!
//! [`v2`] reads the `.2cbh` family written by ChessBase 17 and later.
//! [`replay`] plays decoded moves on a [`chesscore::Board`], checking each
//! move word against the position.

use std::fmt;
use std::path::PathBuf;

#[cfg(feature = "fixture")]
pub mod fixture;
pub mod movetable;
pub mod pgn;
pub mod replay;
pub mod v2;

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, std::io::Error),
    Format(String),
    NoSuchGame(u32),
    Move { ply: u32, reason: replay::MoveError },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Error::Format(msg) => write!(f, "format error: {msg}"),
            Error::NoSuchGame(id) => write!(f, "no game with id {id}"),
            Error::Move { ply, reason } => write!(f, "move {ply}: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(_, e) => Some(e),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_and_source() {
        let io = Error::Io(PathBuf::from("db.2cbh"), std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert_eq!(io.to_string(), "db.2cbh: missing");
        assert!(std::error::Error::source(&io).is_some());
        assert_eq!(Error::Format("x".into()).to_string(), "format error: x");
        assert_eq!(Error::NoSuchGame(7).to_string(), "no game with id 7");
        let null = replay::MoveError::NullMove;
        assert_eq!(Error::Move { ply: 3, reason: null }.to_string(), "move 3: null move");
        assert!(std::error::Error::source(&Error::NoSuchGame(1)).is_none());
    }
}
