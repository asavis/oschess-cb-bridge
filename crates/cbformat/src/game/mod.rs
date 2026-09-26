//! The game model both database formats read into (#65): a header record's
//! fields, the players and the tournament, where the game starts, and its
//! annotations. [`crate::v2`] and [`crate::cbh`] each read their own files
//! into these types; [`crate::pgn`] writes them, and [`crate::pgnfile`],
//! [`crate::dbitems`] and [`crate::view`] use them as they are.

mod annotations;
mod entities;
mod fields;
mod head;
mod start;

pub(crate) use annotations::decode_text as annotations_text;
pub use annotations::{
    Annotation, Arrow, Block, GAME_POSITION, GameAnnotations, Quotation, QuotedPlayer, Square, Unknown, language,
    timing,
};
pub use entities::{Player, Tournament};
pub use fields::{Date, Eco, GameResult, ROUND_TEXT_BYTES, RecordKind, round_text};
pub use head::Head;
pub use start::{Setup, Start};

/// Most records one read returns, in either format and in a PGN file's
/// index: 12 MiB of 2CBH headers.
pub const MAX_BATCH_RECORDS: u32 = 1 << 16;
