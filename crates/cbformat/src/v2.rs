//! Reader for the ChessBase 2 database format (`.2cbh` family, ChessBase 17+).
//!
//! Files are read at positions and never mapped into memory: a mapped file
//! cannot be extended or truncated by another process on Windows, and the
//! bridge reads databases that ChessBase may be writing. Single records are one
//! positional read each; [`Database::batch`] reads a run of records and their
//! moves in two large reads for full scans.

use std::borrow::Cow;
use std::fs::File;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::movetable::{self, Color, Piece, Sq, from_cb_square};
use crate::{Error, Result};

pub const HEADER_RECORD_SIZE: usize = 192;

/// The extensions of the files that make up a database.
pub const EXTENSIONS: [&str; 6] = [".2cbh", ".2cbg", ".2cba", ".2lid", ".2lgd", ".2lcd"];
const RECORD_MAGIC: [u8; 8] = [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
/// Size of a move record's frame before its content.
const FRAME_HEADER: usize = 0x1a;
/// Largest content or spare area accepted in one move record. The largest in
/// a Mega Database is about 1.2 MB, a guiding text.
const MAX_FRAME_PART: usize = 64 << 20;
/// Largest span of `.2cbg` read for one batch; a batch whose moves lie wider
/// apart reads each move record on its own.
const MAX_BATCH_SPAN: u64 = 256 << 20;
/// Most records read by one [`Database::records`] or [`Database::batch`]:
/// 12 MiB of headers.
pub const MAX_BATCH_RECORDS: u32 = 1 << 16;

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

/// One file of a database, read at positions.
struct DbFile {
    file: File,
    path: PathBuf,
}

impl DbFile {
    fn open(path: PathBuf) -> Result<DbFile> {
        let file = File::open(&path).map_err(|e| Error::Io(path.clone(), e))?;
        Ok(DbFile { file, path })
    }

    fn len(&self) -> Result<u64> {
        self.file.metadata().map(|m| m.len()).map_err(|e| Error::Io(self.path.clone(), e))
    }

    /// Fills `buf` from `offset`; a short file is an error.
    fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        read_exact_at(&self.file, buf, offset).map_err(|e| Error::Io(self.path.clone(), e))
    }

    /// `len` bytes from `offset`. Callers bound `len` first.
    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];
        self.read_into(offset, &mut buf)?;
        Ok(buf)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// An open 2CBH database: game headers, moves and entities.
pub struct Database {
    stem: PathBuf,
    headers: DbFile,
    moves: DbFile,
    entities: Entities,
    records: u32,
    format_version: u8,
}

impl Database {
    /// Opens the database whose files share `path`'s stem. `path` may name the
    /// `.2cbh` file or the bare stem. The record count is taken now; games
    /// added later are seen by opening the database again.
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
        let headers = DbFile::open(with(".2cbh"))?;
        let len = headers.len()?;
        if len < HEADER_RECORD_SIZE as u64 || !len.is_multiple_of(HEADER_RECORD_SIZE as u64) {
            return Err(Error::Format(format!(".2cbh size {len} is not a multiple of 192")));
        }
        let records = u32::try_from(len / HEADER_RECORD_SIZE as u64 - 1)
            .map_err(|_| Error::Format(format!(".2cbh size {len} holds more than 2^32 records")))?;
        let header = headers.read(0, HEADER_RECORD_SIZE)?;
        let record_size = le_i16(&header, 0x0a);
        if record_size as usize != HEADER_RECORD_SIZE {
            return Err(Error::Format(format!(".2cbh record size {record_size}, expected 192")));
        }
        let moves = DbFile::open(with(".2cbg"))?;
        let entities = Entities::new(DbFile::open(with(".2lid"))?)?;
        Ok(Database { stem, headers, moves, entities, records, format_version: header[0x0d] })
    }

    pub fn stem(&self) -> &Path {
        &self.stem
    }

    /// The paths of every file of the database, whether or not each exists.
    pub fn file_paths(&self) -> Vec<PathBuf> {
        EXTENSIONS
            .iter()
            .map(|ext| {
                let mut s = self.stem.clone().into_os_string();
                s.push(ext);
                PathBuf::from(s)
            })
            .collect()
    }

    /// Number of records, including deleted games, texts and analyses.
    pub fn record_count(&self) -> u32 {
        self.records
    }

    /// The format version byte of the header file.
    pub fn format_version(&self) -> u8 {
        self.format_version
    }

    /// The record for 1-based game id `id`.
    pub fn record(&self, id: u32) -> Result<Record> {
        if id == 0 || id > self.records {
            return Err(Error::NoSuchGame(id));
        }
        let mut b = [0; HEADER_RECORD_SIZE];
        self.headers.read_into(u64::from(id) * HEADER_RECORD_SIZE as u64, &mut b)?;
        Ok(Record { id, b })
    }

    pub fn entities(&self) -> &Entities {
        &self.entities
    }

    /// The move record a game or analysis header points at.
    pub fn moves_of(&self, record: &Record) -> Result<MoveData<'static>> {
        let offset = record.moves_offset();
        let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
        let at = u64::try_from(offset).map_err(|_| bad("negative offset"))?;
        let file_len = self.moves.len()?;
        if at.checked_add(FRAME_HEADER as u64).is_none_or(|end| end > file_len) {
            return Err(bad("offset out of range"));
        }
        let head = self.moves.read(at, FRAME_HEADER)?;
        let (a, b) = frame_sizes(&head, offset)?;
        let whole = (FRAME_HEADER + a + b + 8) as u64;
        if at + whole > file_len {
            return Err(bad("runs past end of file"));
        }
        let frame = self.moves.read(at, whole as usize)?;
        let (tag, content) = parse_frame(&frame, offset, true)?;
        let content = content.to_vec();
        Ok(MoveData { tag, content: Cow::Owned(content) })
    }

    /// Records `first..=last`, clamped to the database and to
    /// [`MAX_BATCH_RECORDS`] records, in one read. Each carries its id; fewer
    /// records than asked may come back.
    pub fn records(&self, first: u32, last: u32) -> Result<Vec<Record>> {
        let (first, last) = self.clamp(first, last);
        if first > last {
            return Ok(Vec::new());
        }
        let headers = self.read_headers(first, last)?;
        Ok(headers
            .as_chunks::<HEADER_RECORD_SIZE>()
            .0
            .iter()
            .zip(first..=last)
            .map(|(b, id)| Record { id, b: *b })
            .collect())
    }

    /// Records `first..=last`, clamped to the database and to
    /// [`MAX_BATCH_RECORDS`] records, with their move records, read in two
    /// large reads, for scanning many games. [`Batch::ids`] gives the ids read.
    pub fn batch(&self, first: u32, last: u32) -> Result<Batch<'_>> {
        let (first, last) = self.clamp(first, last);
        if first > last {
            return Ok(Batch { db: self, first, last, headers: Vec::new(), span_at: 0, span: Vec::new() });
        }
        // One record past the batch, when there is one: its move record starts
        // where the batch's last one ends, since move records are stored back
        // to back in id order.
        let upto = last.saturating_add(1).min(self.records);
        let headers = self.read_headers(first, upto)?;
        let offsets: Vec<u64> = headers
            .as_chunks::<HEADER_RECORD_SIZE>()
            .0
            .iter()
            .filter_map(|r| u64::try_from(le_i64(r, 0x08)).ok())
            .filter(|&o| o >= 12)
            .collect();
        let file_len = self.moves.len()?;
        let span_at = offsets.iter().copied().min().unwrap_or(0).min(file_len);
        let span_end = if upto > last { offsets.last().copied().unwrap_or(file_len) } else { file_len };
        let span_end = span_end.max(offsets.iter().copied().max().unwrap_or(0)).min(file_len);
        let span = if span_end > span_at && span_end - span_at <= MAX_BATCH_SPAN {
            self.moves.read(span_at, (span_end - span_at) as usize)?
        } else {
            Vec::new()
        };
        Ok(Batch { db: self, first, last, headers, span_at, span })
    }

    /// `first..=last` within the database and at most [`MAX_BATCH_RECORDS`]
    /// long; empty when `first > last`, with `first` at least 1.
    fn clamp(&self, first: u32, last: u32) -> (u32, u32) {
        let first = first.max(1);
        (first, last.min(self.records).min(first.saturating_add(MAX_BATCH_RECORDS - 1)))
    }

    /// The header records `first..=last`, at most [`MAX_BATCH_RECORDS`] + 1.
    fn read_headers(&self, first: u32, last: u32) -> Result<Vec<u8>> {
        let count = (last - first + 1) as usize;
        debug_assert!(count <= MAX_BATCH_RECORDS as usize + 1);
        self.headers.read(u64::from(first) * HEADER_RECORD_SIZE as u64, count * HEADER_RECORD_SIZE)
    }
}

/// A run of records read together by [`Database::batch`].
pub struct Batch<'db> {
    db: &'db Database,
    first: u32,
    last: u32,
    /// The records of the batch, and possibly the one after it.
    headers: Vec<u8>,
    span_at: u64,
    span: Vec<u8>,
}

impl<'db> Batch<'db> {
    /// The ids the batch covers.
    pub fn ids(&self) -> RangeInclusive<u32> {
        self.first..=self.last
    }

    pub fn record(&self, id: u32) -> Result<Record> {
        if !self.ids().contains(&id) {
            return self.db.record(id);
        }
        let o = (id - self.first) as usize * HEADER_RECORD_SIZE;
        let mut b = [0; HEADER_RECORD_SIZE];
        b.copy_from_slice(&self.headers[o..o + HEADER_RECORD_SIZE]);
        Ok(Record { id, b })
    }

    /// The move record of `record`, from the batch's buffer when it lies
    /// inside it and read on its own otherwise.
    pub fn moves_of(&self, record: &Record) -> Result<MoveData<'_>> {
        let offset = record.moves_offset();
        // A position that does not fit in `usize` (on a 32-bit target) lies
        // outside the span and is read on its own.
        let inside = u64::try_from(offset)
            .ok()
            .and_then(|at| at.checked_sub(self.span_at))
            .and_then(|rel| usize::try_from(rel).ok());
        if let Some(rel) = inside.filter(|&rel| rel.saturating_add(FRAME_HEADER) <= self.span.len())
            && let Ok((a, b)) = frame_sizes(&self.span[rel..], offset)
            && rel + FRAME_HEADER + a + b + 8 <= self.span.len()
        {
            let (tag, content) = parse_frame(&self.span[rel..], offset, true)?;
            return Ok(MoveData { tag, content: Cow::Borrowed(content) });
        }
        self.db.moves_of(record)
    }
}

/// A move record's tag and content, borrowed from a batch or owned.
pub struct MoveData<'a> {
    tag: u16,
    content: Cow<'a, [u8]>,
}

impl MoveData<'_> {
    pub fn tag(&self) -> u16 {
        self.tag
    }

    pub fn moves(&self) -> Result<GameMoves<'_>> {
        GameMoves::parse(self.tag, &self.content)
    }
}

/// The content and spare sizes of a frame, from its first bytes.
fn frame_sizes(frame: &[u8], offset: i64) -> Result<(usize, usize)> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    if frame.len() < FRAME_HEADER {
        return Err(bad("offset out of range"));
    }
    if frame[..8] != RECORD_MAGIC {
        return Err(bad("bad magic"));
    }
    let a = usize::try_from(le_i32(frame, 8)).map_err(|_| bad("negative size"))?;
    let b = usize::try_from(le_i32(frame, 12)).map_err(|_| bad("negative size"))?;
    if a > MAX_FRAME_PART || b > MAX_FRAME_PART {
        return Err(bad("record larger than 64 MiB"));
    }
    Ok((a, b))
}

/// Checks one framed record from `.2cbg` or `.2cba` held from its first byte
/// in `frame`: magic, sizes, trailing length and checksum. Returns its tag and
/// content.
fn parse_frame(frame: &[u8], offset: i64, verify_checksum: bool) -> Result<(u16, &[u8])> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    let (a, b) = frame_sizes(frame, offset)?;
    if FRAME_HEADER + a + b + 8 > frame.len() {
        return Err(bad("runs past end of file"));
    }
    let tail = le_i64(frame, FRAME_HEADER + a + b);
    if tail != (a + b + 34) as i64 {
        return Err(bad("trailing length mismatch"));
    }
    let content = &frame[FRAME_HEADER..FRAME_HEADER + a];
    if verify_checksum && be_u64(frame, 0x10) != checksum(content) {
        return Err(bad("checksum mismatch"));
    }
    Ok((le_u16(frame, 0x18), content))
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
pub struct Record {
    id: u32,
    b: [u8; HEADER_RECORD_SIZE],
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

/// Largest container size accepted; the real ones are at most 1,120 bytes.
const MAX_CONTAINER: i32 = 1 << 20;
/// Largest entity-file header accepted; the real ones are at most 236 bytes.
const MAX_ENTITY_HEADER: usize = 64 << 10;

pub const PLAYER: usize = 0;
pub const TOURNAMENT: usize = 1;
pub const SOURCE: usize = 2;
pub const TEAM: usize = 4;
pub const GAME_TAG: usize = 5;

/// The `.2lid` entity file.
pub struct Entities {
    file: DbFile,
    /// The file's length when opened; entities are read within it.
    len: u64,
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
    fn new(file: DbFile) -> Result<Self> {
        let len = file.len()?;
        if len < 8 {
            return Err(Error::Format(".2lid too short".into()));
        }
        let bad = |what: String| Error::Format(format!(".2lid header: {what}"));
        let head = file.read(0, 8)?;
        let (header_size, ntypes) = (be_i32(&head, 0), be_i32(&head, 4));
        if !(1..=32).contains(&ntypes) {
            return Err(bad(format!("{ntypes} entity types")));
        }
        let ntypes = ntypes as usize;
        let header_size = usize::try_from(header_size).map_err(|_| bad(format!("size {header_size}")))?;
        if 8 + 20 * ntypes > header_size || header_size as u64 > len || header_size > MAX_ENTITY_HEADER {
            return Err(bad(format!("size {header_size} for {ntypes} types in a {len}-byte file")));
        }
        let d = file.read(0, header_size)?;
        let mut types = Vec::with_capacity(ntypes);
        let mut container_offset = Vec::with_capacity(ntypes);
        let mut block_size = 0usize;
        for i in 0..ntypes {
            let o = 8 + 20 * i;
            let size = be_i32(&d, o);
            if !(0..=MAX_CONTAINER).contains(&size) {
                return Err(bad(format!("type {i} container size {size}")));
            }
            let count = be_i64(&d, o + 4);
            if count < 0 {
                return Err(bad(format!("type {i} count {count}")));
            }
            types.push((size as usize, count, be_i64(&d, o + 12)));
            container_offset.push(block_size);
            block_size += size as usize; // at most 32 · MAX_CONTAINER
        }
        Ok(Entities { file, len, header_size, types, container_offset, block_size })
    }

    pub fn count(&self, typ: usize) -> i64 {
        self.types.get(typ).map_or(0, |t| t.1)
    }

    /// The record bytes after the length field, or `None` for an unused id or
    /// one past the end of the file as it was when opened. One positional read
    /// of the container; a container inside that length that cannot be read
    /// now, because the file was truncated or failed, is an error.
    pub fn raw(&self, typ: usize, id: i64) -> Result<Option<Vec<u8>>> {
        let Some(&(size, count, _)) = self.types.get(typ) else { return Ok(None) };
        if id < 0 || id >= count {
            return Ok(None);
        }
        let Some(o) = u64::try_from(id)
            .ok()
            .and_then(|id| id.checked_mul(self.block_size as u64))
            .and_then(|o| o.checked_add(self.header_size as u64))
            .and_then(|o| o.checked_add(self.container_offset[typ] as u64))
        else {
            return Ok(None);
        };
        let want = (size as u64).min(self.len.saturating_sub(o)) as usize;
        if want < 4 {
            return Ok(None);
        }
        let mut buf = self.file.read(o, want)?;
        let Some(n) = usize::try_from(le_i32(&buf, 0)).ok().filter(|&n| n != 0 && n <= want - 4) else {
            return Ok(None);
        };
        buf.truncate(4 + n);
        buf.drain(..4);
        Ok(Some(buf))
    }

    /// The player `id`, or `None` for an unused or unreadable entry; errors
    /// as for [`Entities::raw`].
    pub fn player(&self, id: i64) -> Result<Option<Player>> {
        Ok(self.raw(PLAYER, id)?.and_then(|r| {
            let mut c = Cursor(&r, 0);
            Some(Player { last: c.string()?, first: c.string()? })
        }))
    }

    /// The tournament `id`, or `None` for an unused or unreadable entry;
    /// errors as for [`Entities::raw`].
    pub fn tournament(&self, id: i64) -> Result<Option<Tournament>> {
        Ok(self.raw(TOURNAMENT, id)?.and_then(|r| {
            let mut c = Cursor(&r, 0);
            let place = c.string()?;
            let title = c.string()?;
            let start = Date(c.i32()?);
            Some(Tournament { title, place, start })
        }))
    }
}

struct Cursor<'a>(&'a [u8], usize);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let v = self.0.get(self.1..self.1.checked_add(n)?)?;
        self.1 += n;
        Some(v)
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).map(|v| i32::from_le_bytes(v.try_into().unwrap()))
    }
    fn string(&mut self) -> Option<String> {
        let n = usize::try_from(self.i32()?).ok()?;
        self.take(n).map(|v| String::from_utf8_lossy(v).into_owned())
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
