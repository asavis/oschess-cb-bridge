//! `.2cbh` records: games, guiding texts and analyses, and the encodings of
//! their fields.

use super::HEADER_RECORD_SIZE;
use super::bytes::{le_i16, le_i32, le_i64, le_u16, le_u32};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Game,
    Text,
    Analysis,
    Unknown(u8),
}

/// A 192-byte `.2cbh` record.
#[derive(Clone, Copy)]
pub struct Record {
    pub(super) id: u32,
    pub(super) b: [u8; HEADER_RECORD_SIZE],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameResult {
    BlackWins,
    Draw,
    WhiteWins,
    Line,
    BlackWinsForfeit,
    DrawForfeit,
    WhiteWinsForfeit,
    BothLost,
    Unknown(u8),
}

impl GameResult {
    pub fn pgn(self) -> &'static str {
        match self {
            GameResult::BlackWins | GameResult::BlackWinsForfeit => "0-1",
            GameResult::Draw | GameResult::DrawForfeit => "1/2-1/2",
            GameResult::WhiteWins | GameResult::WhiteWinsForfeit => "1-0",
            GameResult::BothLost => "0-0",
            GameResult::Line | GameResult::Unknown(_) => "*",
        }
    }
}

impl Record {
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
        match self.b[0x58] {
            0 => GameResult::BlackWins,
            1 => GameResult::Draw,
            2 => GameResult::WhiteWins,
            3 => GameResult::Line,
            4 => GameResult::BlackWinsForfeit,
            5 => GameResult::DrawForfeit,
            6 => GameResult::WhiteWinsForfeit,
            7 => GameResult::BothLost,
            r => GameResult::Unknown(r),
        }
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
        match le_u16(&self.b, 0x80) {
            0 => Eco::None,
            v @ 128..=64127 => Eco::Code { code: v / 128 - 1, sub: (v % 128) as u8 },
            v @ 64576.. => Eco::Chess960(v - 64576),
            v => Eco::Invalid(v),
        }
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

/// The ECO field of a game record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eco {
    None,
    /// `code` 0-499 is A00-E99; `sub` is ChessBase's sub-code.
    Code {
        code: u16,
        sub: u8,
    },
    /// A Chess960 start position, 0-959.
    Chess960(u16),
    /// A value that is none of the above.
    Invalid(u16),
}

impl Eco {
    /// The PGN `ECO` tag value, for an opening code.
    pub fn pgn(self) -> Option<String> {
        match self {
            Eco::Code { code, .. } => Some(format!("{}{:02}", (b'A' + (code / 100) as u8) as char, code % 100)),
            _ => None,
        }
    }
}

/// A packed ChessBase date; any part may be 0, meaning unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date(pub i32);

impl Date {
    pub fn day(self) -> u8 {
        (self.0 & 31) as u8
    }
    pub fn month(self) -> u8 {
        ((self.0 >> 5) & 15) as u8
    }
    pub fn year(self) -> u16 {
        ((self.0 >> 9) & 0xfff) as u16
    }
    /// The date as a PGN `Date` tag value.
    pub fn pgn(self) -> String {
        let part = |v: u32, w: usize| if v == 0 { "?".repeat(w) } else { format!("{v:0w$}") };
        format!("{}.{}.{}", part(self.year() as u32, 4), part(self.month() as u32, 2), part(self.day() as u32, 2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_with_eco(v: u16) -> [u8; HEADER_RECORD_SIZE] {
        let mut b = [0u8; HEADER_RECORD_SIZE];
        b[0x80..0x82].copy_from_slice(&v.to_le_bytes());
        b
    }

    #[test]
    fn eco_field() {
        let eco = |v| Record { id: 1, b: record_with_eco(v) }.eco();
        assert_eq!(eco(0), Eco::None);
        assert_eq!(eco(128), Eco::Code { code: 0, sub: 0 });
        assert_eq!(eco(128).pgn().as_deref(), Some("A00"));
        assert_eq!(eco(500 * 128 + 5).pgn().as_deref(), Some("E99"));
        assert_eq!(eco(64576 + 518), Eco::Chess960(518));
        for v in [1, 127, 64128, 64575] {
            assert_eq!(eco(v), Eco::Invalid(v), "{v}");
            assert_eq!(eco(v).pgn(), None);
        }
    }

    #[test]
    fn date_pgn() {
        assert_eq!(Date((2020 << 9) | (2 << 5) | 15).pgn(), "2020.02.15");
        assert_eq!(Date(1998 << 9).pgn(), "1998.??.??");
        assert_eq!(Date(0).pgn(), "????.??.??");
    }
}
