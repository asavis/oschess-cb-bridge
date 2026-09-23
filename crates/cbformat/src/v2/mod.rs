//! Reader for the ChessBase 2 database format (`.2cbh` family, ChessBase 17+).
//!
//! Files are read at positions and never mapped into memory: a mapped file
//! cannot be extended or truncated by another process on Windows, and the
//! bridge reads databases that ChessBase may be writing. Single records are one
//! positional read each; [`Database::batch`] reads a run of records and their
//! moves in two large reads for full scans.

use std::borrow::Cow;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

mod bytes;
mod entities;
mod file;
mod frame;
mod moves;
mod record;

use bytes::{le_i16, le_i64};
pub use entities::{Entities, GAME_TAG, PLAYER, Player, SOURCE, TEAM, TOURNAMENT, Tournament};
use file::DbFile;
pub use frame::checksum;
use frame::{FRAME_HEADER, frame_sizes, parse_frame};
pub use moves::{GameMoves, Setup, Start, Token};
pub use record::{Date, Eco, GameResult, Record, RecordKind};

pub const HEADER_RECORD_SIZE: usize = 192;

/// The extensions of the files that make up a database.
pub const EXTENSIONS: [&str; 6] = [".2cbh", ".2cbg", ".2cba", ".2lid", ".2lgd", ".2lcd"];
/// Largest span of `.2cbg` read for one batch; a batch whose moves lie wider
/// apart reads each move record on its own.
const MAX_BATCH_SPAN: u64 = 256 << 20;
/// Most records read by one [`Database::records`] or [`Database::batch`]:
/// 12 MiB of headers.
pub const MAX_BATCH_RECORDS: u32 = 1 << 16;

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
