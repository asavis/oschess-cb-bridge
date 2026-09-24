//! `.cbh` records: the 46-byte headers of games and guiding texts.
//!
//! Integers are big-endian. Field encodings shared with 2CBH (result, ECO,
//! date) decode to the `v2` types, so both formats read the same way.

use super::bytes::{be_u16, be_u24, be_u32};
use crate::v2::{Date, Eco, GameResult, RecordKind};

/// Size of a `.cbh` record, and of the file header before the first one.
pub const RECORD_SIZE: usize = 46;

/// A 46-byte `.cbh` record.
#[derive(Clone, Copy)]
pub struct Record {
    pub(super) id: u32,
    pub(super) b: [u8; RECORD_SIZE],
}

impl Record {
    /// The record `id` whose 46 bytes are `b`, as [`super::Database::read_records`]
    /// reads them.
    pub fn from_bytes(id: u32, b: &[u8; RECORD_SIZE]) -> Record {
        Record { id, b: *b }
    }
    pub fn id(&self) -> u32 {
        self.id
    }
    pub fn bytes(&self) -> &[u8] {
        &self.b
    }
    pub fn is_deleted(&self) -> bool {
        self.b[0] & 0x80 != 0
    }
    /// Games and guiding texts; a record without bit 0 lies past the last
    /// game and is reported as unknown. The classic format has no analyses.
    pub fn kind(&self) -> RecordKind {
        match self.b[0] {
            t if t & 1 == 0 => RecordKind::Unknown(t),
            t if t & 2 != 0 => RecordKind::Text,
            _ => RecordKind::Game,
        }
    }
    fn is_text(&self) -> bool {
        self.b[0] & 2 != 0
    }
    /// Offset of the moves (games) or of the text body (guiding texts) in `.cbg`.
    pub fn moves_offset(&self) -> u32 {
        be_u32(&self.b, 0x01)
    }
    /// Offset of the annotations in `.cba`; 0 when the game has none.
    pub fn annotations_offset(&self) -> u32 {
        if self.is_text() { 0 } else { be_u32(&self.b, 0x05) }
    }
    pub fn white(&self) -> u32 {
        if self.is_text() { 0 } else { be_u24(&self.b, 0x09) }
    }
    pub fn black(&self) -> u32 {
        if self.is_text() { 0 } else { be_u24(&self.b, 0x0c) }
    }
    pub fn tournament(&self) -> u32 {
        be_u24(&self.b, if self.is_text() { 0x07 } else { 0x0f })
    }
    pub fn annotator(&self) -> u32 {
        be_u24(&self.b, if self.is_text() { 0x0d } else { 0x12 })
    }
    pub fn source(&self) -> u32 {
        be_u24(&self.b, if self.is_text() { 0x0a } else { 0x15 })
    }
    pub fn played_date(&self) -> Date {
        Date(if self.is_text() { 0 } else { be_u24(&self.b, 0x18) as i32 })
    }
    pub fn result(&self) -> GameResult {
        GameResult::from_field(self.b[0x1b])
    }
    /// The evaluation glyph of an unfinished game (result *line*), else 0.
    pub fn line_evaluation(&self) -> u8 {
        self.b[0x1c]
    }
    pub fn round(&self) -> u8 {
        self.b[if self.is_text() { 0x10 } else { 0x1d }]
    }
    pub fn subround(&self) -> u8 {
        self.b[if self.is_text() { 0x11 } else { 0x1e }]
    }
    pub fn white_elo(&self) -> u16 {
        be_u16(&self.b, 0x1f)
    }
    pub fn black_elo(&self) -> u16 {
        be_u16(&self.b, 0x21)
    }
    /// The ECO field: an opening code, a Chess960 start position, or nothing.
    pub fn eco(&self) -> Eco {
        Eco::from_field(be_u16(&self.b, 0x23))
    }
    pub fn medals(&self) -> u16 {
        be_u16(&self.b, 0x25)
    }
    pub fn flags(&self) -> u32 {
        be_u32(&self.b, if self.is_text() { 0x12 } else { 0x27 })
    }
    /// Number of moves in the main line, capped at 255.
    pub fn move_count(&self) -> u8 {
        self.b[0x2d]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(fields: &[(usize, &[u8])]) -> Record {
        let mut b = [0u8; RECORD_SIZE];
        b[0] = 1;
        for (o, v) in fields {
            b[*o..*o + v.len()].copy_from_slice(v);
        }
        Record { id: 1, b }
    }

    #[test]
    fn game_fields() {
        let r = game(&[
            (0x01, &[0, 0, 0x01, 0x00]),
            (0x09, &[0, 0, 7]),
            (0x0c, &[0, 1, 0]),
            (0x18, &((2020 << 9) | (2 << 5) | 15u32).to_be_bytes()[1..]),
            (0x1b, &[2]),
            (0x1f, &2750u16.to_be_bytes()),
            (0x23, &(64576u16 + 518).to_be_bytes()),
        ]);
        assert_eq!(r.kind(), RecordKind::Game);
        assert_eq!(r.moves_offset(), 256);
        assert_eq!((r.white(), r.black()), (7, 256));
        assert_eq!(r.played_date().pgn(), "2020.02.15");
        assert_eq!(r.result(), GameResult::WhiteWins);
        assert_eq!(r.white_elo(), 2750);
        assert_eq!(r.eco(), Eco::Chess960(518));
    }

    #[test]
    fn kinds() {
        let with = |t: u8| {
            let mut r = game(&[]);
            r.b[0] = t;
            r
        };
        assert_eq!(with(0x03).kind(), RecordKind::Text);
        assert_eq!(with(0x81).kind(), RecordKind::Game);
        assert!(with(0x81).is_deleted());
        assert_eq!(with(0).kind(), RecordKind::Unknown(0));
        // A guiding text's fields sit at other offsets.
        let mut t = with(0x03);
        t.b[0x07..0x0a].copy_from_slice(&[0, 0, 5]);
        assert_eq!(t.tournament(), 5);
        assert_eq!(t.white(), 0);
    }
}
