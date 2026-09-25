//! Reader for the ChessBase 2 database format (`.2cbh` family, ChessBase 17+).
//!
//! Files are read at positions and never mapped into memory: a mapped file
//! cannot be extended or truncated by another process on Windows, and the
//! bridge reads databases that ChessBase may be writing. Single records are one
//! positional read each; [`Database::batch`] reads a run of records, their
//! moves and their annotations in three large reads for full scans.

use std::borrow::Cow;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

mod annotations;
mod bytes;
mod entities;
mod frame;
mod moves;
mod record;
mod window;

pub use annotations::ANNOTATION_TAG;
use bytes::{le_i16, le_i64};
pub use entities::{Entities, GAME_TAG, PLAYER, SOURCE, TEAM, TOURNAMENT};
pub use frame::checksum;
use frame::{FRAME_HEADER, MAX_FRAME_PART, frame_sizes, parse_frame};
pub use moves::{GameMoves, Token};
pub use record::Record;
pub use window::MoveWindow;

use crate::file::{self, DbFile};
use crate::game::{GameAnnotations, MAX_BATCH_RECORDS};

pub const HEADER_RECORD_SIZE: usize = 192;

/// The extensions of the files that make up a database.
pub const EXTENSIONS: [&str; 6] = [".2cbh", ".2cbg", ".2cba", ".2lid", ".2lgd", ".2lcd"];
/// Files that sit beside a database under its name without being needed to
/// read it: its settings and the opening key files. An export must not
/// overwrite them either.
pub const BESIDE: [&str; 3] = [".ini", ".cko", ".cpo"];
/// Largest span of `.2cbg` read for one batch; a batch whose moves lie wider
/// apart reads each move record on its own.
const MAX_BATCH_SPAN: u64 = 256 << 20;

/// An open 2CBH database: game headers, moves, annotations and entities.
pub struct Database {
    stem: PathBuf,
    headers: DbFile,
    moves: DbFile,
    /// `None` when the database has no `.2cba` file.
    annotations: Option<DbFile>,
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
        let annotations = match DbFile::open(with(".2cba")) {
            Ok(f) => Some(f),
            Err(Error::Io(_, e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let entities = Entities::new(DbFile::open(with(".2lid"))?)?;
        Ok(Database { stem, headers, moves, annotations, entities, records, format_version: header[0x0d] })
    }

    pub fn stem(&self) -> &Path {
        &self.stem
    }

    /// The paths of every file of the database and of the files beside it
    /// ([`BESIDE`]), whether or not each exists.
    pub fn file_paths(&self) -> Vec<PathBuf> {
        file::with_extensions(&self.stem, EXTENSIONS.iter().chain(&BESIDE))
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
        self.moves_of_within(record, MAX_FRAME_PART)
    }

    /// [`Database::moves_of`], refusing before it is read a move record whose
    /// content or spare area is larger than `limit` bytes: a caller that must
    /// bound its work per game (a server) chooses the limit.
    pub fn moves_of_within(&self, record: &Record, limit: usize) -> Result<MoveData<'static>> {
        let (tag, content) = read_frame(&self.moves, record.moves_offset(), limit, "move record")?;
        Ok(MoveData { tag, content: Cow::Owned(content) })
    }

    /// Whether the database has an annotation file. Without one, every game
    /// reads as having no annotations.
    pub fn has_annotations(&self) -> bool {
        self.annotations.is_some()
    }

    /// The annotations of a game or analysis, or `None` when the database has
    /// no `.2cba` file.
    pub fn annotations_of(&self, record: &Record) -> Result<Option<GameAnnotations>> {
        self.annotations_of_within(record, MAX_FRAME_PART)
    }

    /// [`Database::annotations_of`], refusing before it is read an annotation
    /// record whose content or spare area is larger than `limit` bytes.
    pub fn annotations_of_within(&self, record: &Record, limit: usize) -> Result<Option<GameAnnotations>> {
        let Some(file) = &self.annotations else { return Ok(None) };
        let offset = record.annotations_offset();
        let (tag, content) = read_frame(file, offset, limit, "annotation record")?;
        annotation_content(tag, &content, offset).map(Some)
    }

    /// Reads the header records from `first` into `buf`, as many as it holds
    /// (192 bytes each) up to the last record, in one read, and returns how
    /// many it read: none when `first` is 0 or past the end. It allocates
    /// nothing, so a caller that scans many records reuses one buffer whose
    /// memory it has accounted for; [`Record::from_bytes`] makes the records.
    pub fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        if first == 0 || first > self.records {
            return Ok(0);
        }
        let fits = u32::try_from(buf.len() / HEADER_RECORD_SIZE).unwrap_or(u32::MAX);
        let count = fits.min(self.records - first + 1);
        let bytes = count as usize * HEADER_RECORD_SIZE;
        self.headers.read_into(u64::from(first) * HEADER_RECORD_SIZE as u64, &mut buf[..bytes])?;
        Ok(count)
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
            return Ok(Batch {
                db: self,
                first,
                last,
                headers: Vec::new(),
                moves: Span::EMPTY,
                annotations: Span::EMPTY,
            });
        }
        // One record past the batch, when there is one: its move record starts
        // where the batch's last one ends, since move records are stored back
        // to back in id order.
        let upto = last.saturating_add(1).min(self.records);
        let headers = self.read_headers(first, upto)?;
        let next = upto > last;
        let moves = Span::read(&self.moves, &headers, 0x08, next)?;
        let annotations = match &self.annotations {
            Some(file) => Span::read(file, &headers, 0x10, next)?,
            None => Span::EMPTY,
        };
        Ok(Batch { db: self, first, last, headers, moves, annotations })
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
    moves: Span,
    annotations: Span,
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
        match self.moves.frame(record.moves_offset()) {
            Some(found) => {
                let (tag, content) = found?;
                Ok(MoveData { tag, content: Cow::Borrowed(content) })
            }
            None => self.db.moves_of(record),
        }
    }

    /// The annotations of `record`, as [`Database::annotations_of`], from the
    /// batch's buffer when the record lies inside it.
    pub fn annotations_of(&self, record: &Record) -> Result<Option<GameAnnotations>> {
        if !self.db.has_annotations() {
            return Ok(None);
        }
        let offset = record.annotations_offset();
        match self.annotations.frame(offset) {
            Some(found) => {
                let (tag, content) = found?;
                annotation_content(tag, content, offset).map(Some)
            }
            None => self.db.annotations_of(record),
        }
    }
}

/// A stretch of `.2cbg` or `.2cba` read for a batch: from the lowest record
/// offset of the batch to the offset of the record after it, since records are
/// stored back to back in id order.
struct Span {
    at: u64,
    bytes: Vec<u8>,
}

impl Span {
    const EMPTY: Span = Span { at: 0, bytes: Vec::new() };

    /// The span for the offsets at `field` of `headers`; `next` says whether
    /// the last header is the record after the batch.
    fn read(file: &DbFile, headers: &[u8], field: usize, next: bool) -> Result<Span> {
        let offsets: Vec<u64> = headers
            .as_chunks::<HEADER_RECORD_SIZE>()
            .0
            .iter()
            .filter_map(|r| u64::try_from(le_i64(r, field)).ok())
            .filter(|&o| o >= 12)
            .collect();
        let file_len = file.len()?;
        let at = offsets.iter().copied().min().unwrap_or(0).min(file_len);
        let end = if next { offsets.last().copied().unwrap_or(file_len) } else { file_len };
        let end = end.max(offsets.iter().copied().max().unwrap_or(0)).min(file_len);
        if end > at && end - at <= MAX_BATCH_SPAN {
            Ok(Span { at, bytes: file.read(at, (end - at) as usize)? })
        } else {
            Ok(Span::EMPTY)
        }
    }

    /// The tag and content of the frame at `offset`, or `None` when the frame
    /// does not lie wholly inside the span.
    fn frame(&self, offset: i64) -> Option<Result<(u16, &[u8])>> {
        // A position that does not fit in `usize` (on a 32-bit target) lies
        // outside the span and is read on its own.
        let rel = u64::try_from(offset).ok()?.checked_sub(self.at).and_then(|rel| usize::try_from(rel).ok())?;
        if rel.saturating_add(FRAME_HEADER) > self.bytes.len() {
            return None;
        }
        let (a, b) = frame_sizes(&self.bytes[rel..], offset).ok()?;
        if rel + FRAME_HEADER + a + b + 8 > self.bytes.len() {
            return None;
        }
        Some(parse_frame(&self.bytes[rel..], offset, true))
    }
}

/// Reads and checks the framed record at `offset` of `file`: two reads, the
/// frame header and then the whole frame. A content or spare area over
/// `limit` bytes is refused before the frame is read; `kind` names the record
/// in that error.
fn read_frame(file: &DbFile, offset: i64, limit: usize, kind: &str) -> Result<(u16, Vec<u8>)> {
    let bad = |what: &str| Error::Format(format!("record at {offset:#x}: {what}"));
    let at = u64::try_from(offset).map_err(|_| bad("negative offset"))?;
    let file_len = file.len()?;
    if at.checked_add(FRAME_HEADER as u64).is_none_or(|end| end > file_len) {
        return Err(bad("offset out of range"));
    }
    let head = file.read(at, FRAME_HEADER)?;
    let (a, b) = frame_sizes(&head, offset)?;
    if a > limit || b > limit {
        return Err(bad(&format!("{kind} of {} bytes, over the {limit}-byte limit", a.max(b))));
    }
    let whole = (FRAME_HEADER + a + b + 8) as u64;
    if at + whole > file_len {
        return Err(bad("runs past end of file"));
    }
    let frame = file.read(at, whole as usize)?;
    let (tag, content) = parse_frame(&frame, offset, true)?;
    Ok((tag, content.to_vec()))
}

fn annotation_content(tag: u16, content: &[u8], offset: i64) -> Result<GameAnnotations> {
    if tag != ANNOTATION_TAG {
        return Err(Error::Format(format!("annotation record at {offset:#x}: tag {tag:#06x}")));
    }
    GameAnnotations::parse(content)
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

    /// The moves of a record whose last word may be cut short, without that
    /// byte: for reading the start of a game, which the cut does not reach.
    /// The tree walk still reports the damage if it gets that far.
    pub fn prefix_moves(&self) -> Result<GameMoves<'_>> {
        GameMoves::parse(self.tag, &self.content[..self.content.len() & !1])
    }
}
