//! A header record in 36 bytes (#106): what search, sort and suggestions read
//! from it, answered through [`Head`] as the record itself answers.
//!
//! Row layout, little-endian: white (a text's title key) `i32` at 0, black (a
//! text's author) `i32` at 4, tournament `i32` at 8, annotator `i32` at 12,
//! played date `i32` at 16, ratings `i16` at 20 and 22, round and sub-round
//! `i16` at 24 and 26, moves `i16` at 28, the ECO field `u16` at 30, the kind
//! at 32 (bit 7 deleted, bit 6 a record with a layout of its own), a kind
//! the formats do not name at 33, the result field at 34; 35 is zero.

use cbformat::game::{Date, Eco, GameResult, Head, RecordKind};

/// Bytes of one row.
pub const ROW: usize = 36;

const DELETED: u8 = 0x80;
const OTHER: u8 = 0x40;

/// One record's row, with its number.
#[derive(Clone, Copy)]
pub struct Slim {
    id: u32,
    b: [u8; ROW],
}

impl Slim {
    /// Record `id` from its row.
    pub fn new(id: u32, row: &[u8]) -> Slim {
        let mut b = [0u8; ROW];
        b.copy_from_slice(&row[..ROW]);
        Slim { id, b }
    }

    /// `r` as a row, when every value a pass reads fits in it and reads back as
    /// `r` answers it; `None` for a record that does not, whose database then
    /// keeps its full records.
    pub fn encode(r: &impl Head) -> Option<[u8; ROW]> {
        let mut b = [0u8; ROW];
        let (kind, unknown) = match r.kind() {
            RecordKind::Game => (0, 0),
            RecordKind::Text => (1, 0),
            RecordKind::Analysis => (2, 0),
            RecordKind::Unknown(k) => (3, k),
        };
        b[32] = kind | if r.is_deleted() { DELETED } else { 0 };
        b[33] = unknown;
        match r.other() {
            Some((title, author)) => {
                b[32] |= OTHER;
                put_i32(&mut b, 0, title)?;
                put_i32(&mut b, 4, author)?;
            }
            None => {
                put_i32(&mut b, 0, r.white())?;
                put_i32(&mut b, 4, r.black())?;
                put_i32(&mut b, 8, r.tournament())?;
                put_i32(&mut b, 12, r.annotator())?;
                b[16..20].copy_from_slice(&r.played_date().0.to_le_bytes());
                let ((white, black), (round, sub)) = (r.elo(), r.round());
                put_i16(&mut b, 20, white)?;
                put_i16(&mut b, 22, black)?;
                put_i16(&mut b, 24, round)?;
                put_i16(&mut b, 26, sub)?;
                put_i16(&mut b, 28, r.move_count())?;
                b[30..32].copy_from_slice(&r.eco().field().to_le_bytes());
                b[34] = r.result().field();
            }
        }
        same(r, &Slim::new(r.id(), &b)).then_some(b)
    }

    fn i32_at(&self, at: usize) -> i32 {
        i32::from_le_bytes(self.b[at..at + 4].try_into().unwrap())
    }

    fn i16_at(&self, at: usize) -> i32 {
        i32::from(i16::from_le_bytes(self.b[at..at + 2].try_into().unwrap()))
    }
}

fn put_i32(b: &mut [u8; ROW], at: usize, v: i64) -> Option<()> {
    b[at..at + 4].copy_from_slice(&i32::try_from(v).ok()?.to_le_bytes());
    Some(())
}

fn put_i16(b: &mut [u8; ROW], at: usize, v: i32) -> Option<()> {
    b[at..at + 2].copy_from_slice(&i16::try_from(v).ok()?.to_le_bytes());
    Some(())
}

/// Whether `b` answers every method a pass calls as `a` does. A record with a
/// layout of its own is read through `other` only (`Head::other`).
pub fn same(a: &impl Head, b: &impl Head) -> bool {
    if a.id() != b.id() || a.kind() != b.kind() || a.is_deleted() != b.is_deleted() || a.other() != b.other() {
        return false;
    }
    a.other().is_some()
        || (a.white() == b.white()
            && a.black() == b.black()
            && a.tournament() == b.tournament()
            && a.annotator() == b.annotator()
            && a.played_date().0 == b.played_date().0
            && a.elo() == b.elo()
            && a.round() == b.round()
            && a.move_count() == b.move_count()
            && a.eco() == b.eco()
            && a.result() == b.result())
}

impl Head for Slim {
    fn id(&self) -> u32 {
        self.id
    }
    fn kind(&self) -> RecordKind {
        match self.b[32] & 3 {
            0 => RecordKind::Game,
            1 => RecordKind::Text,
            2 => RecordKind::Analysis,
            _ => RecordKind::Unknown(self.b[33]),
        }
    }
    fn is_deleted(&self) -> bool {
        self.b[32] & DELETED != 0
    }
    fn white(&self) -> i64 {
        i64::from(self.i32_at(0))
    }
    fn black(&self) -> i64 {
        i64::from(self.i32_at(4))
    }
    fn tournament(&self) -> i64 {
        i64::from(self.i32_at(8))
    }
    fn annotator(&self) -> i64 {
        i64::from(self.i32_at(12))
    }
    fn other(&self) -> Option<(i64, i64)> {
        (self.b[32] & OTHER != 0).then(|| (i64::from(self.i32_at(0)), i64::from(self.i32_at(4))))
    }
    fn result(&self) -> GameResult {
        GameResult::from_field(self.b[34])
    }
    fn eco(&self) -> Eco {
        Eco::from_field(u16::from_le_bytes([self.b[30], self.b[31]]))
    }
    fn played_date(&self) -> Date {
        Date(self.i32_at(16))
    }
    fn round(&self) -> (i32, i32) {
        (self.i16_at(24), self.i16_at(26))
    }
    fn elo(&self) -> (i32, i32) {
        (self.i16_at(20), self.i16_at(22))
    }
    fn move_count(&self) -> i32 {
        self.i16_at(28)
    }
    fn bytes(&self) -> &[u8] {
        &self.b
    }
}
