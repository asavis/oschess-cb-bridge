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
    /// The result byte of a game record, as both formats store it.
    pub fn from_field(v: u8) -> GameResult {
        match v {
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

/// The ECO field of a game record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Eco {
    #[default]
    None,
    /// `code` 0-499 is A00-E99; `sub` is ChessBase's sub-code.
    Code { code: u16, sub: u8 },
    /// A Chess960 start position, 0-959.
    Chess960(u16),
    /// A value that is none of the above.
    Invalid(u16),
}

impl Eco {
    /// The ECO field of a game record, as both formats store it.
    pub fn from_field(v: u16) -> Eco {
        match v {
            0 => Eco::None,
            v @ 128..=64127 => Eco::Code { code: v / 128 - 1, sub: (v % 128) as u8 },
            v @ 64576.. => Eco::Chess960(v - 64576),
            v => Eco::Invalid(v),
        }
    }

    /// The opening code, `A00`..`E99`, when the field holds one; the text the
    /// game list shows, the search matches and the PGN `ECO` tag holds (#68).
    /// Allocation-free, for searches over millions of records.
    pub fn code_text(self) -> Option<[u8; 3]> {
        match self {
            Eco::Code { code, .. } => {
                Some([b'A' + (code / 100) as u8, b'0' + (code / 10 % 10) as u8, b'0' + (code % 10) as u8])
            }
            _ => None,
        }
    }

    /// The PGN `ECO` tag value, for an opening code: [`Eco::code_text`].
    pub fn pgn(self) -> Option<String> {
        self.code_text().map(|t| t.iter().map(|&b| char::from(b)).collect())
    }
}

/// A packed ChessBase date; any part may be 0, meaning unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    /// The date as PGN writes it, `YYYY.MM.DD` with `?` for an unknown part:
    /// the text the game list shows, the search matches and the PGN `Date` tag
    /// holds (#68). Allocation-free, for searches over millions of records.
    pub fn text(self) -> [u8; 10] {
        let mut out = *b"????.??.??";
        let mut put = |at: usize, width: usize, v: u32| {
            if v != 0 {
                let mut v = v;
                for i in (0..width).rev() {
                    out[at + i] = b'0' + (v % 10) as u8;
                    v /= 10;
                }
            }
        };
        put(0, 4, u32::from(self.year()));
        put(5, 2, u32::from(self.month()));
        put(8, 2, u32::from(self.day()));
        out
    }

    /// The date as a PGN `Date` tag value: [`Date::text`].
    pub fn pgn(self) -> String {
        self.text().iter().map(|&b| char::from(b)).collect()
    }
}

/// The longest [`round_text`]: two 10-digit numbers and the parentheses.
pub const ROUND_TEXT_BYTES: usize = 22;

/// A round and sub-round as the game list shows them, the search matches them
/// and the PGN `Round` tag holds them (#68): `5`, `5(2)` with a sub-round, or
/// empty when there is no round. A round of 0 or less is no round, and a
/// sub-round of 0 or less no sub-round: 2CBH stores both signed, and a
/// negative value is not a round. The PGN writer writes `?` for empty.
/// Written into `buf`, without allocating.
pub fn round_text(round: i32, sub: i32, buf: &mut [u8; ROUND_TEXT_BYTES]) -> &str {
    let mut len = 0;
    if round > 0 {
        len = put_number(buf, len, round.unsigned_abs());
        if sub > 0 {
            buf[len] = b'(';
            len = put_number(buf, len + 1, sub.unsigned_abs());
            buf[len] = b')';
            len += 1;
        }
    }
    std::str::from_utf8(&buf[..len]).unwrap_or("")
}

/// Writes `v` in decimal into `buf` at `at`; where it ends.
fn put_number(buf: &mut [u8], at: usize, v: u32) -> usize {
    let mut digits = [0u8; 10];
    let (mut v, mut n) = (v, 0);
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for (i, &d) in digits[..n].iter().rev().enumerate() {
        buf[at + i] = d;
    }
    at + n
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
    fn round_text_shows_no_round_for_zero_or_less() {
        let text = |n, s| round_text(n, s, &mut [0; ROUND_TEXT_BYTES]).to_string();
        assert_eq!(text(5, 0), "5");
        assert_eq!(text(5, 2), "5(2)");
        assert_eq!(text(5, -1), "5");
        assert_eq!(text(0, 3), "");
        assert_eq!(text(-1, 0), "");
        assert_eq!(text(i32::MIN, 5), "");
        assert_eq!(text(i32::MAX, i32::MAX), "2147483647(2147483647)");
        assert_eq!(text(i32::MAX, i32::MAX).len(), ROUND_TEXT_BYTES);
    }

    #[test]
    fn date_pgn() {
        assert_eq!(Date((2020 << 9) | (2 << 5) | 15).pgn(), "2020.02.15");
        assert_eq!(Date(1998 << 9).pgn(), "1998.??.??");
        assert_eq!(Date(0).pgn(), "????.??.??");
    }
}
