//! A header record's fields as every format reads them (#66): what the game
//! list, the search, the sort and the PGN take from a header, in one shape.

use super::{Date, Eco, GameResult, RecordKind};

/// A header record: a game, a guiding text or an analysis. Entity ids are
/// those of the database's own tables, which differ between formats; every
/// other field reads the same in both. Each format implements this once, next
/// to its record, so a field is mapped in one place.
pub trait Head: Copy + Send + Sync {
    fn id(&self) -> u32;
    fn kind(&self) -> RecordKind;
    fn is_deleted(&self) -> bool;
    fn white(&self) -> i64;
    fn black(&self) -> i64;
    fn tournament(&self) -> i64;
    /// A game's annotator.
    fn annotator(&self) -> i64;
    /// For a record that is not a game, its title's key and its author; -1
    /// where it has none. Guiding texts and analyses have header layouts of
    /// their own.
    fn other(&self) -> Option<(i64, i64)>;
    fn result(&self) -> GameResult;
    fn eco(&self) -> Eco;
    fn played_date(&self) -> Date;
    /// Round and sub-round; 0 or less when there is none.
    fn round(&self) -> (i32, i32);
    /// White's and black's ratings; 0 or less when unknown.
    fn elo(&self) -> (i32, i32);
    /// Moves in the main line, as the header stores them: the classic format
    /// caps them at 255, and a PGN file's are counted as written.
    fn move_count(&self) -> i32;
    /// The record as stored.
    fn bytes(&self) -> &[u8];
}
