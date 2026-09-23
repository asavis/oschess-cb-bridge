//! Reader for the ChessBase 2 database format (`.2cbh` family, ChessBase 17+).
//!
//! The files are memory-mapped and read in place. Records are exposed as views
//! over the mapped bytes; nothing is copied until a caller asks for a string.

use std::fs::File;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::movetable::{self, Color, Piece, Sq, from_cb_square};
use crate::{Error, Result};

pub const HEADER_RECORD_SIZE: usize = 192;
const RECORD_MAGIC: [u8; 8] = [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];

fn le_i16(b: &[u8], o: usize) -> i16 {
    i16::from_le_bytes([b[o], b[o + 1]])
}
fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le_i64(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn be_i32(b: &[u8], o: usize) -> i32 {
    i32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be_i64(b: &[u8], o: usize) -> i64 {
    i64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}
fn be_u64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

fn map(path: &Path) -> Result<Mmap> {
    let f = File::open(path).map_err(|e| Error::Io(path.to_owned(), e))?;
    // SAFETY: the database is opened read-only and treated as untrusted bytes;
    // every access is bounds-checked. A concurrent writer (ChessBase appending
    // games) can at worst make a record read inconsistent, which the framing
    // checks report as a format error.
    unsafe { Mmap::map(&f) }.map_err(|e| Error::Io(path.to_owned(), e))
}

/// An open 2CBH database: game headers, moves and entities.
pub struct Database {
    stem: PathBuf,
    headers: Mmap,
    moves: Mmap,
    entities: Entities,
}

impl Database {
    /// Opens the database whose files share `path`'s stem. `path` may name the
    /// `.2cbh` file or the bare stem.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let stem = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("2cbh")) {
            path.with_extension("")
        } else {
            path.to_owned()
        };
        let with = |ext: &str| {
            let mut s = stem.clone().into_os_string();
            s.push(ext);
            PathBuf::from(s)
        };
        let headers = map(&with(".2cbh"))?;
        if headers.len() < HEADER_RECORD_SIZE || headers.len() % HEADER_RECORD_SIZE != 0 {
            return Err(Error::Format(format!(".2cbh size {} is not a multiple of 192", headers.len())));
        }
        let record_size = le_i16(&headers, 0x0a);
        if record_size as usize != HEADER_RECORD_SIZE {
            return Err(Error::Format(format!(".2cbh record size {record_size}, expected 192")));
        }
        let moves = map(&with(".2cbg"))?;
        let entities = Entities::new(map(&with(".2lid"))?)?;
        Ok(Database { stem, headers, moves, entities })
    }

    pub fn stem(&self) -> &Path {
        &self.stem
    }

    /// Number of records, including deleted games, texts and analyses.
    pub fn record_count(&self) -> u32 {
        (self.headers.len() / HEADER_RECORD_SIZE - 1) as u32
    }

    /// The format version byte of the header file.
    pub fn format_version(&self) -> u8 {
        self.headers[0x0d]
    }

    /// The record for 1-based game id `id`.
    pub fn record(&self, id: u32) -> Result<Record<'_>> {
        if id == 0 || id > self.record_count() {
            return Err(Error::NoSuchGame(id));
        }
        let o = id as usize * HEADER_RECORD_SIZE;
        Ok(Record { id, b: &self.headers[o..o + HEADER_RECORD_SIZE] })
    }

    pub fn entities(&self) -> &Entities {
        &self.entities
    }

    /// The move record a game or analysis header points at.
    pub fn moves_of(&self, record: &Record<'_>) -> Result<GameMoves<'_>> {
        let off = record.moves_offset();
        let (tag, content) = framed_record(&self.moves, off, true)?;
        GameMoves::parse(tag, content)
    }
}

/// Reads one framed record from `.2cbg` or `.2cba`: magic, sizes, checksum,
/// tag, content, spare area and trailing length.
fn framed_record(file: &[u8], offset: i64, verify_checksum: bool) -> Result<(u16, &[u8])> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    if offset < 0 || offset as usize + 0x1a > file.len() {
        return Err(bad("offset out of range"));
    }
    let o = offset as usize;
    if file[o..o + 8] != RECORD_MAGIC {
        return Err(bad("bad magic"));
    }
    let a = le_i32(file, o + 8);
    let b = le_i32(file, o + 12);
    if a < 0 || b < 0 {
        return Err(bad("negative size"));
    }
    let (a, b) = (a as usize, b as usize);
    let end = o + 0x1a + a + b + 8;
    if end > file.len() {
        return Err(bad("runs past end of file"));
    }
    let tail = le_i64(file, o + 0x1a + a + b);
    if tail != (a + b + 34) as i64 {
        return Err(bad("trailing length mismatch"));
    }
    let content = &file[o + 0x1a..o + 0x1a + a];
    if verify_checksum && be_u64(file, o + 0x10) != checksum(content) {
        return Err(bad("checksum mismatch"));
    }
    Ok((le_u16(file, o + 0x18), content))
}

/// The record checksum: byte *i* of the value is the sum, modulo 256, of the
/// *i*-th of eight equal runs of the content; a trailing remainder is ignored.
pub fn checksum(content: &[u8]) -> u64 {
    let m = content.len() / 8;
    if m == 0 {
        let mut buf = [0u8; 8];
        buf[..content.len()].copy_from_slice(content);
        return u64::from_le_bytes(buf);
    }
    let mut v = 0u64;
    for i in 0..8 {
        let s = content[i * m..(i + 1) * m].iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
        v |= (s as u64) << (8 * i);
    }
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Game,
    Text,
    Analysis,
    Unknown(u8),
}

/// A 192-byte `.2cbh` record.
#[derive(Clone, Copy)]
pub struct Record<'a> {
    id: u32,
    b: &'a [u8],
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

impl<'a> Record<'a> {
    pub fn id(&self) -> u32 {
        self.id
    }
    pub fn bytes(&self) -> &'a [u8] {
        self.b
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
        le_i64(self.b, 0x08)
    }
    pub fn annotations_offset(&self) -> i64 {
        le_i64(self.b, 0x10)
    }
    pub fn white(&self) -> i64 {
        le_i64(self.b, 0x18)
    }
    pub fn black(&self) -> i64 {
        le_i64(self.b, 0x20)
    }
    pub fn tournament(&self) -> i64 {
        le_i64(self.b, 0x28)
    }
    pub fn annotator(&self) -> i64 {
        le_i64(self.b, 0x30)
    }
    pub fn source(&self) -> i64 {
        le_i64(self.b, 0x38)
    }
    pub fn white_team(&self) -> i64 {
        le_i64(self.b, 0x40)
    }
    pub fn black_team(&self) -> i64 {
        le_i64(self.b, 0x48)
    }
    pub fn game_tag(&self) -> i64 {
        le_i64(self.b, 0x50)
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
        le_i16(self.b, 0x5a)
    }
    pub fn subround(&self) -> i16 {
        le_i16(self.b, 0x5c)
    }
    pub fn board(&self) -> i16 {
        le_i16(self.b, 0x5e)
    }
    pub fn white_elo(&self) -> i16 {
        le_i16(self.b, 0x60)
    }
    pub fn black_elo(&self) -> i16 {
        le_i16(self.b, 0x70)
    }
    /// ECO code 0-499 (A00-E99) and sub-code, when the field holds one.
    pub fn eco(&self) -> Option<(u16, u8)> {
        let v = le_u16(self.b, 0x80);
        if v == 0 || v >= 64576 { None } else { Some((v / 128 - 1, (v % 128) as u8)) }
    }
    pub fn flags(&self) -> u32 {
        le_u32(self.b, 0x84)
    }
    /// Number of full moves in the main line.
    pub fn move_count(&self) -> i16 {
        le_i16(self.b, 0x8a)
    }
    pub fn played_date(&self) -> Date {
        Date(le_i32(self.b, 0xbc))
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

// ------------------------------------------------------------------- moves

/// Where a game starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Standard,
    Chess960(u16),
    Setup(Setup),
}

/// A set-up start position, decoded from the start-position section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    pub chess960: bool,
    pub move_number: u16,
    pub side_to_move: Color,
    /// Castling rights from the high byte of the second word. Bits: 1 white
    /// O-O-O, 2 white O-O, 4 black O-O-O, 8 black O-O.
    pub castling: u8,
    /// En passant file 0-7 (`a`-`h`), when the third word names one.
    pub en_passant_file: Option<u8>,
    /// The third word itself. Zero in every set-up position examined; read as
    /// an en passant file 1-8 when it holds one, which is **unconfirmed**.
    pub en_passant_raw: u16,
    pub pieces: Vec<(Sq, Color, Piece)>,
}

/// The move record of one game: its variant tag, start and move-tree words.
#[derive(Clone, Copy)]
pub struct GameMoves<'a> {
    pub tag: u16,
    start: &'a [u8],
    tree: &'a [u8],
}

/// One token of the move tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// A move word, including the null move; decode with [`movetable::decode`].
    Move(u16),
    /// The move just given has a further alternative, stored later.
    Alternative,
    /// End of the current line.
    EndOfLine,
}

const PIECE_ORDER: [Piece; 5] = [Piece::King, Piece::Queen, Piece::Knight, Piece::Bishop, Piece::Rook];

impl<'a> GameMoves<'a> {
    /// Splits a move record's content into its start section and move tree.
    pub fn parse(tag: u16, content: &'a [u8]) -> Result<Self> {
        if !content.len().is_multiple_of(2) {
            return Err(Error::Format("odd move stream length".into()));
        }
        let word = |i: usize| le_u16(content, 2 * i);
        let n = content.len() / 2;
        if n == 0 {
            return Err(Error::Format("empty move stream".into()));
        }
        let mut i = 0;
        let start_from = 0;
        if word(0) == movetable::START_POSITION {
            i = 1;
            while i < n && word(i) != movetable::MOVES {
                i += 1;
            }
        }
        if i >= n || word(i) != movetable::MOVES {
            return Err(Error::Format("no move section".into()));
        }
        Ok(GameMoves { tag, start: &content[start_from..2 * i], tree: &content[2 * (i + 1)..] })
    }

    /// The variant from the record tag: 1 normal chess, 2 Chess960.
    pub fn is_chess960(&self) -> bool {
        self.tag & 0xff == 2
    }

    pub fn start(&self) -> Result<Start> {
        if self.start.is_empty() {
            return Ok(Start::Standard);
        }
        let w: Vec<u16> = self.start[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        if self.is_chess960() && w.len() == 1 {
            return Ok(Start::Chess960(w[0]));
        }
        let (chess960, s) =
            if self.is_chess960() && w.first() == Some(&1000) { (true, &w[1..]) } else { (self.is_chess960(), &w[..]) };
        if s.len() < 3 {
            return Err(Error::Format("short set-up section".into()));
        }
        let mut pieces = Vec::with_capacity(s.len() - 3);
        for &p in &s[3..] {
            let (color, piece, cb) = match p {
                0xc02d..=0xc16c => (Color::White, PIECE_ORDER[((p - 0xc02d) / 64) as usize], (p - 0xc02d) % 64),
                0xc16d..=0xc2ac => (Color::Black, PIECE_ORDER[((p - 0xc16d) / 64) as usize], (p - 0xc16d) % 64),
                0xc2ad..=0xc2dc => {
                    let i = p - 0xc2ad;
                    (Color::White, Piece::Pawn, (i / 6) * 8 + i % 6 + 1)
                }
                0xc2dd..=0xc30c => {
                    let i = p - 0xc2dd;
                    (Color::Black, Piece::Pawn, (i / 6) * 8 + i % 6 + 1)
                }
                _ => return Err(Error::Format(format!("bad piece word {p:#06x}"))),
            };
            pieces.push((from_cb_square(cb as u8), color, piece));
        }
        Ok(Start::Setup(Setup {
            chess960,
            move_number: s[0],
            side_to_move: if s[1] & 0xff == 0 { Color::White } else { Color::Black },
            castling: (s[1] >> 8) as u8,
            en_passant_file: if (1..=8).contains(&s[2]) { Some(s[2] as u8 - 1) } else { None },
            en_passant_raw: s[2],
            pieces,
        }))
    }

    /// The move tree in stored order.
    pub fn tokens(&self) -> impl Iterator<Item = Token> + 'a {
        self.tree.as_chunks::<2>().0.iter().map(|c| match u16::from_le_bytes(*c) {
            movetable::ALTERNATIVE => Token::Alternative,
            movetable::END_OF_LINE => Token::EndOfLine,
            w => Token::Move(w),
        })
    }

    /// The main line: the move words up to the first end of line.
    pub fn main_line(&self) -> impl Iterator<Item = u16> + 'a {
        self.tokens()
            .take_while(|t| *t != Token::EndOfLine)
            .filter_map(|t| if let Token::Move(w) = t { Some(w) } else { None })
    }
}

// ---------------------------------------------------------------- entities

pub const PLAYER: usize = 0;
pub const TOURNAMENT: usize = 1;
pub const SOURCE: usize = 2;
pub const TEAM: usize = 4;
pub const GAME_TAG: usize = 5;

/// The `.2lid` entity file.
pub struct Entities {
    d: Mmap,
    header_size: usize,
    /// (container size, count, first deleted id) per type
    types: Vec<(usize, i64, i64)>,
    container_offset: Vec<usize>,
    block_size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    pub last: String,
    pub first: String,
}

impl Player {
    pub fn pgn(&self) -> String {
        if self.first.is_empty() { self.last.clone() } else { format!("{}, {}", self.last, self.first) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tournament {
    pub title: String,
    pub place: String,
    pub start: Date,
}

impl Entities {
    fn new(d: Mmap) -> Result<Self> {
        if d.len() < 8 {
            return Err(Error::Format(".2lid too short".into()));
        }
        let header_size = be_i32(&d, 0) as usize;
        let ntypes = be_i32(&d, 4) as usize;
        if ntypes == 0 || ntypes > 32 || 8 + 20 * ntypes > header_size || header_size > d.len() {
            return Err(Error::Format(format!(".2lid header size {header_size}, {ntypes} types")));
        }
        let mut types = Vec::with_capacity(ntypes);
        let mut container_offset = Vec::with_capacity(ntypes);
        let mut block_size = 0;
        for i in 0..ntypes {
            let o = 8 + 20 * i;
            let size = be_i32(&d, o) as usize;
            types.push((size, be_i64(&d, o + 4), be_i64(&d, o + 12)));
            container_offset.push(block_size);
            block_size += size;
        }
        Ok(Entities { d, header_size, types, container_offset, block_size })
    }

    pub fn count(&self, typ: usize) -> i64 {
        self.types.get(typ).map_or(0, |t| t.1)
    }

    /// The record bytes after the length field, or `None` for an unused id.
    pub fn raw(&self, typ: usize, id: i64) -> Option<&[u8]> {
        let (size, count, _) = *self.types.get(typ)?;
        if id < 0 || id >= count {
            return None;
        }
        let o = self.header_size + id as usize * self.block_size + self.container_offset[typ];
        if o + 4 > self.d.len() {
            return None;
        }
        let n = le_i32(&self.d, o);
        if n <= 0 || n as usize + 4 > size || o + 4 + n as usize > self.d.len() {
            return None;
        }
        Some(&self.d[o + 4..o + 4 + n as usize])
    }

    pub fn player(&self, id: i64) -> Option<Player> {
        let r = self.raw(PLAYER, id)?;
        let mut c = Cursor(r, 0);
        Some(Player { last: c.string()?, first: c.string()? })
    }

    pub fn tournament(&self, id: i64) -> Option<Tournament> {
        let r = self.raw(TOURNAMENT, id)?;
        let mut c = Cursor(r, 0);
        let place = c.string()?;
        let title = c.string()?;
        let start = Date(c.i32()?);
        Some(Tournament { title, place, start })
    }
}

struct Cursor<'a>(&'a [u8], usize);

impl Cursor<'_> {
    fn i32(&mut self) -> Option<i32> {
        let v = self.0.get(self.1..self.1 + 4)?;
        self.1 += 4;
        Some(i32::from_le_bytes(v.try_into().unwrap()))
    }
    fn string(&mut self) -> Option<String> {
        let n = self.i32()?;
        if n < 0 {
            return None;
        }
        let v = self.0.get(self.1..self.1 + n as usize)?;
        self.1 += n as usize;
        Some(String::from_utf8_lossy(v).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_runs() {
        assert_eq!(checksum(&[1, 2, 3]), 0x030201);
        let content: Vec<u8> = (0..17).collect();
        // m = 2: runs (0,1) (2,3) ... (14,15); byte 16 ignored
        let expect: u64 = (0..8).map(|i| ((2 * i + 2 * i + 1) as u64) << (8 * i)).sum();
        assert_eq!(checksum(&content), expect);
    }

    #[test]
    fn date_pgn() {
        assert_eq!(Date((2020 << 9) | (2 << 5) | 15).pgn(), "2020.02.15");
        assert_eq!(Date(1998 << 9).pgn(), "1998.??.??");
        assert_eq!(Date(0).pgn(), "????.??.??");
    }
}
