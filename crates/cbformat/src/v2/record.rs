//! `.2cbh` records: games, guiding texts and analyses. The fields they share
//! with classic records are in [`crate::game`].

use super::HEADER_RECORD_SIZE;
use super::bytes::{le_i16, le_i32, le_i64, le_u16, le_u32};
use crate::game::{Date, Eco, GameResult, Head, RecordKind};

/// A 192-byte `.2cbh` record.
#[derive(Clone, Copy)]
pub struct Record {
    pub(super) id: u32,
    pub(super) b: [u8; HEADER_RECORD_SIZE],
}

impl Record {
    /// The record numbered `id` from its header bytes, as
    /// [`crate::v2::Database::read_records`] reads them.
    pub fn from_bytes(id: u32, b: &[u8; HEADER_RECORD_SIZE]) -> Record {
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
    pub fn kind(&self) -> RecordKind {
        match self.b[2] {
            1 if self.b[0] & 2 != 0 => RecordKind::Text,
            1 => RecordKind::Game,
            2 => RecordKind::Analysis,
            k => RecordKind::Unknown(k),
        }
    }
    /// Offset of the moves in `.2cbg` (games and analyses) or of the text body
    /// (guiding texts).
    pub fn moves_offset(&self) -> i64 {
        le_i64(&self.b, 0x08)
    }
    pub fn annotations_offset(&self) -> i64 {
        le_i64(&self.b, 0x10)
    }
    pub fn white(&self) -> i64 {
        le_i64(&self.b, 0x18)
    }
    pub fn black(&self) -> i64 {
        le_i64(&self.b, 0x20)
    }
    pub fn tournament(&self) -> i64 {
        le_i64(&self.b, 0x28)
    }
    pub fn annotator(&self) -> i64 {
        le_i64(&self.b, 0x30)
    }
    pub fn source(&self) -> i64 {
        le_i64(&self.b, 0x38)
    }
    pub fn white_team(&self) -> i64 {
        le_i64(&self.b, 0x40)
    }
    pub fn black_team(&self) -> i64 {
        le_i64(&self.b, 0x48)
    }
    pub fn game_tag(&self) -> i64 {
        le_i64(&self.b, 0x50)
    }
    /// A guiding text's author, a player id. Guiding texts share only their
    /// first eight bytes with games; the game accessors do not apply to them.
    pub fn text_author(&self) -> i64 {
        le_i64(&self.b, 0x20)
    }
    /// The game tag holding a guiding text's title; see [`crate::v2::Entities::title`].
    pub fn text_title(&self) -> i64 {
        le_i64(&self.b, 0x28)
    }
    /// The game tag holding an analysis's title.
    pub fn analysis_title(&self) -> i64 {
        le_i64(&self.b, 0x18)
    }
    /// An analysis's author, a player id.
    pub fn analysis_author(&self) -> i64 {
        le_i64(&self.b, 0x28)
    }
    pub fn result(&self) -> GameResult {
        GameResult::from_field(self.b[0x58])
    }
    pub fn round(&self) -> i16 {
        le_i16(&self.b, 0x5a)
    }
    pub fn subround(&self) -> i16 {
        le_i16(&self.b, 0x5c)
    }
    pub fn board(&self) -> i16 {
        le_i16(&self.b, 0x5e)
    }
    pub fn white_elo(&self) -> i16 {
        le_i16(&self.b, 0x60)
    }
    pub fn black_elo(&self) -> i16 {
        le_i16(&self.b, 0x70)
    }
    /// The ECO field: an opening code, a Chess960 start position, or nothing.
    pub fn eco(&self) -> Eco {
        Eco::from_field(le_u16(&self.b, 0x80))
    }
    pub fn flags(&self) -> u32 {
        le_u32(&self.b, 0x84)
    }
    /// Number of full moves in the main line.
    pub fn move_count(&self) -> i16 {
        le_i16(&self.b, 0x8a)
    }
    pub fn played_date(&self) -> Date {
        Date(le_i32(&self.b, 0xbc))
    }
}

impl Head for Record {
    fn id(&self) -> u32 {
        Record::id(self)
    }
    fn kind(&self) -> RecordKind {
        Record::kind(self)
    }
    fn is_deleted(&self) -> bool {
        Record::is_deleted(self)
    }
    fn white(&self) -> i64 {
        Record::white(self)
    }
    fn black(&self) -> i64 {
        Record::black(self)
    }
    fn tournament(&self) -> i64 {
        Record::tournament(self)
    }
    fn annotator(&self) -> i64 {
        Record::annotator(self)
    }
    fn other(&self) -> Option<(i64, i64)> {
        match Record::kind(self) {
            RecordKind::Game => None,
            RecordKind::Text => Some((self.text_title(), self.text_author())),
            RecordKind::Analysis => Some((self.analysis_title(), self.analysis_author())),
            RecordKind::Unknown(_) => Some((-1, -1)),
        }
    }
    fn result(&self) -> GameResult {
        Record::result(self)
    }
    fn eco(&self) -> Eco {
        Record::eco(self)
    }
    fn played_date(&self) -> Date {
        Record::played_date(self)
    }
    fn round(&self) -> (i32, i32) {
        (i32::from(Record::round(self)), i32::from(self.subround()))
    }
    fn elo(&self) -> (i32, i32) {
        (i32::from(self.white_elo()), i32::from(self.black_elo()))
    }
    fn move_count(&self) -> i32 {
        i32::from(Record::move_count(self))
    }
    fn bytes(&self) -> &[u8] {
        Record::bytes(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ECO field is read at 0x80; [`Eco::from_field`] decodes it.
    #[test]
    fn eco_field() {
        let mut b = [0u8; HEADER_RECORD_SIZE];
        b[0x80..0x82].copy_from_slice(&(500u16 * 128 + 5).to_le_bytes());
        assert_eq!(Record { id: 1, b }.eco(), Eco::Code { code: 499, sub: 5 });
    }
}
