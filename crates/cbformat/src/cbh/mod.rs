//! Reader for the classic ChessBase database format (`.cbh` family), from the
//! description in Morphy's `format/v1`.
//!
//! The files are read at positions like the 2CBH ones. Headers are 46-byte
//! big-endian records; each points at its game's move record in `.cbg` and
//! names its players, tournament, annotator and source by entity id. Moves
//! are decoded while the tree is walked ([`walk`]), since the compact encoding
//! names a move relative to the position it is played in.

use std::borrow::Cow;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::v2::file::{self, DbFile};
use crate::v2::{GameAnnotations, MAX_BATCH_RECORDS};
use crate::{Error, Result};

pub mod annotations;
mod bytes;
mod decode;
mod entities;
mod moves;
mod pieces;
mod record;
pub(crate) mod tables;
mod text;
mod wide;
mod window;

use bytes::{be_u16, be_u24};
pub use decode::{MAX_VARIATION_DEPTH, start_as_played, walk};
pub use entities::Entities;
pub use moves::GameMoves;
pub use record::{RECORD_SIZE, Record};
pub use window::MoveWindow;

/// The extensions of every file of the classic format: the ones the reader
/// uses, and the optional ones ChessBase adds or rebuilds (media manifest,
/// search boosters and the like).
pub const EXTENSIONS: [&str; 19] = [
    ".cbh", ".cbg", ".cba", ".cbp", ".cbt", ".cbc", ".cbs", ".cbj", ".cbe", ".cbl", ".cbtt", ".flags", ".cbm", ".cit",
    ".cib", ".cit2", ".cib2", ".cbb", ".cbgi",
];
/// Files that sit beside a classic database under its name without being part
/// of the format: settings, icon, and the opening key files. An export must
/// not overwrite them either.
pub const BESIDE: [&str; 13] =
    [".ini", ".ico", ".pgi", ".ckn", ".cko", ".ck1", ".ck2", ".ck3", ".cpn", ".cpo", ".cp1", ".cp2", ".cp3"];
/// Largest annotation record read. The largest in the databases examined is
/// about 45 KB; a record's head may claim up to 4 GiB, which would otherwise
/// be allocated before the record is found to be damaged.
pub const MAX_ANNOTATION_RECORD: usize = 16 << 20;
/// The smallest file header of `.cbg` and `.cba`, where the first record may
/// start: 26 bytes, or 10 in databases made by old versions.
const MIN_FILE_HEADER: u64 = 10;
/// Largest span of `.cbg` read for one batch; a batch whose moves lie wider
/// apart reads each move record on its own.
const MAX_BATCH_SPAN: u64 = 256 << 20;

/// An open classic database: game headers, moves and entities.
pub struct Database {
    stem: PathBuf,
    headers: DbFile,
    moves: DbFile,
    /// `None` when the database has no `.cba` file.
    annotations: Option<DbFile>,
    /// The 64-bit offsets of `.cbj`, read only when `.cbg` or `.cba` is over
    /// 4 GiB and the 32-bit ones of `.cbh` cannot reach every record.
    wide: Option<wide::Wide>,
    entities: Entities,
    records: u32,
    format_version: u8,
}

impl Database {
    /// Opens the database whose files share `path`'s stem. `path` may name the
    /// `.cbh` file or the bare stem. The record count is taken now.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let stem = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cbh")) {
            path.with_extension("")
        } else {
            path.to_owned()
        };
        let with = |ext: &str| {
            let mut s = stem.clone().into_os_string();
            s.push(ext);
            PathBuf::from(s)
        };
        let headers = DbFile::open(with(".cbh"))?;
        let len = headers.len()?;
        if len < RECORD_SIZE as u64 || !len.is_multiple_of(RECORD_SIZE as u64) {
            return Err(Error::Format(format!(".cbh size {len} is not a multiple of 46")));
        }
        let records = u32::try_from(len / RECORD_SIZE as u64 - 1)
            .map_err(|_| Error::Format(format!(".cbh size {len} holds more than 2^32 records")))?;
        let header = headers.read(0, RECORD_SIZE)?;
        let record_size = be_u16(&header, 0x03);
        if record_size as usize != RECORD_SIZE {
            return Err(Error::Format(format!(".cbh record size {record_size}, expected 46")));
        }
        let moves = DbFile::open(with(".cbg"))?;
        let annotations = match DbFile::open(with(".cba")) {
            Ok(f) => Some(f),
            Err(Error::Io(_, e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let large = |f: &DbFile| f.len().map(|n| n > u64::from(u32::MAX));
        let wide = if large(&moves)? || annotations.as_ref().map(large).transpose()?.unwrap_or(false) {
            Some(wide::Wide::open(with(".cbj"))?)
        } else {
            None
        };
        let entities = Entities::open(with)?;
        Ok(Database { stem, headers, moves, annotations, wide, entities, records, format_version: header[0x05] })
    }

    pub fn stem(&self) -> &Path {
        &self.stem
    }

    /// The paths of every file of the database and of the files beside it
    /// ([`BESIDE`]), whether or not each exists.
    pub fn file_paths(&self) -> Vec<PathBuf> {
        file::with_extensions(&self.stem, EXTENSIONS.iter().chain(&BESIDE))
    }

    /// Number of records, including deleted games and guiding texts.
    pub fn record_count(&self) -> u32 {
        self.records
    }

    /// The format version byte of the header file.
    pub fn format_version(&self) -> u8 {
        self.format_version
    }

    pub fn entities(&self) -> &Entities {
        &self.entities
    }

    /// The record for 1-based game id `id`.
    pub fn record(&self, id: u32) -> Result<Record> {
        if id == 0 || id > self.records {
            return Err(Error::NoSuchGame(id));
        }
        let mut b = [0; RECORD_SIZE];
        self.headers.read_into(u64::from(id) * RECORD_SIZE as u64, &mut b)?;
        Ok(Record { id, b })
    }

    /// Where a game's moves and annotations are: from `.cbh`, or from `.cbj`
    /// when the files are too large for its offsets.
    fn offsets(&self, record: &Record) -> Result<(u64, u64)> {
        let short = (record.moves_offset(), record.annotations_offset());
        match &self.wide {
            Some(w) => w.offsets(record.id(), short),
            None => Ok((u64::from(short.0), u64::from(short.1))),
        }
    }

    /// The move record a game header points at, or a guiding text's text
    /// record: its 4-byte head names its size.
    pub fn moves_of(&self, record: &Record) -> Result<MoveData<'static>> {
        self.moves_of_within(record, usize::MAX)
    }

    /// [`Database::moves_of`], refusing before it is read a record larger than
    /// `limit` bytes.
    pub fn moves_of_within(&self, record: &Record, limit: usize) -> Result<MoveData<'static>> {
        let (at, size) = self.move_extent(record)?;
        if size > limit {
            return Err(Error::Format(format!("move record at {at:#x}: {size} bytes, over the limit of {limit}")));
        }
        Ok(MoveData { bytes: Cow::Owned(self.moves.read(at, size)?) })
    }

    /// Where the `.cbg` record of `record` is and its size, from its head.
    fn move_extent(&self, record: &Record) -> Result<(u64, usize)> {
        let at = self.offsets(record)?.0;
        let bad = |what: &str| Error::Format(format!("move record at {at:#x}: {what}"));
        let file_len = self.moves.len()?;
        if at < MIN_FILE_HEADER || at + 4 > file_len {
            return Err(bad("offset out of range"));
        }
        let mut head = [0u8; 4];
        self.moves.read_into(at, &mut head)?;
        let size = be_u24(&head, 1) as u64;
        if size < 4 {
            return Err(bad(&format!("size {size} is smaller than the record's head")));
        }
        if at + size > file_len {
            return Err(bad("runs past end of file"));
        }
        Ok((at, size as usize))
    }

    /// Whether the database has a `.cba` file.
    pub fn has_annotations(&self) -> bool {
        self.annotations.is_some()
    }

    /// The annotations of a game, or `None` when the database has no `.cba`
    /// file. A game without annotations has an empty set. Positions count the
    /// moves in stored order ([`annotations`]). A record over
    /// [`MAX_ANNOTATION_RECORD`] is refused before it is read.
    pub fn annotations_of(&self, record: &Record) -> Result<Option<GameAnnotations>> {
        self.annotations_of_within(record, MAX_ANNOTATION_RECORD)
    }

    /// [`Database::annotations_of`], refusing before it is read an annotation
    /// record larger than `limit` bytes, and never reading one larger than
    /// [`MAX_ANNOTATION_RECORD`].
    pub fn annotations_of_within(&self, record: &Record, limit: usize) -> Result<Option<GameAnnotations>> {
        let limit = limit.min(MAX_ANNOTATION_RECORD);
        let Some(file) = &self.annotations else { return Ok(None) };
        let at = self.offsets(record)?.1;
        if at == 0 {
            return Ok(Some(GameAnnotations::default()));
        }
        let bad = |what: &str| Error::Format(format!("annotation record at {at:#x}: {what}"));
        let file_len = file.len()?;
        if at < MIN_FILE_HEADER || at + annotations::HEAD as u64 > file_len {
            return Err(bad("offset out of range"));
        }
        let size = annotations::record_size(&file.read(at, annotations::HEAD)?);
        if size < annotations::HEAD || at + size as u64 > file_len {
            return Err(bad("runs past end of file"));
        }
        if size > limit {
            return Err(bad(&format!("{size} bytes, over the limit of {limit}")));
        }
        annotations::parse(&file.read(at, size)?, record.id()).map(Some)
    }

    /// Reads the header records from `first` into `buf`, as many as it holds
    /// ([`RECORD_SIZE`] bytes each) up to the last record, in one read, and
    /// returns how many it read: none when `first` is 0 or past the end. It
    /// allocates nothing, as [`crate::v2::Database::read_records`];
    /// [`Record::from_bytes`] makes the records.
    pub fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        if first == 0 || first > self.records {
            return Ok(0);
        }
        let fits = u32::try_from(buf.len() / RECORD_SIZE).unwrap_or(u32::MAX);
        let count = fits.min(self.records - first + 1);
        let bytes = count as usize * RECORD_SIZE;
        self.headers.read_into(u64::from(first) * RECORD_SIZE as u64, &mut buf[..bytes])?;
        Ok(count)
    }

    /// Records `first..=last`, clamped to the database and to
    /// [`MAX_BATCH_RECORDS`] records, in one read.
    pub fn records(&self, first: u32, last: u32) -> Result<Vec<Record>> {
        let (first, last) = self.clamp(first, last);
        if first > last {
            return Ok(Vec::new());
        }
        let headers = self.read_headers(first, last)?;
        Ok(headers.as_chunks::<RECORD_SIZE>().0.iter().zip(first..=last).map(|(b, id)| Record { id, b: *b }).collect())
    }

    /// Records `first..=last`, clamped as [`Database::records`] clamps them,
    /// with their move records, read in two large reads. [`Batch::ids`] gives
    /// the ids read.
    pub fn batch(&self, first: u32, last: u32) -> Result<Batch<'_>> {
        let (first, last) = self.clamp(first, last);
        if first > last {
            return Ok(Batch { db: self, first, last, headers: Vec::new(), span_at: 0, span: Vec::new() });
        }
        if self.wide.is_some() {
            // The headers' 32-bit offsets cannot place a span; every move
            // record is read on its own through `.cbj`.
            let headers = self.read_headers(first, last)?;
            return Ok(Batch { db: self, first, last, headers, span_at: 0, span: Vec::new() });
        }
        // One record past the batch, when there is one: its move record starts
        // where the batch's last one ends, as move records are in id order.
        let upto = last.saturating_add(1).min(self.records);
        let headers = self.read_headers(first, upto)?;
        let offsets: Vec<u64> = headers
            .as_chunks::<RECORD_SIZE>()
            .0
            .iter()
            .map(|r| u64::from(Record { id: 0, b: *r }.moves_offset()))
            .filter(|&o| o >= 10)
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

    fn clamp(&self, first: u32, last: u32) -> (u32, u32) {
        let first = first.max(1);
        (first, last.min(self.records).min(first.saturating_add(MAX_BATCH_RECORDS - 1)))
    }

    /// The header records `first..=last`, at most [`MAX_BATCH_RECORDS`] + 1.
    fn read_headers(&self, first: u32, last: u32) -> Result<Vec<u8>> {
        let count = (last - first + 1) as usize;
        self.headers.read(u64::from(first) * RECORD_SIZE as u64, count * RECORD_SIZE)
    }
}

/// A run of records read together by [`Database::batch`].
pub struct Batch<'db> {
    db: &'db Database,
    first: u32,
    last: u32,
    headers: Vec<u8>,
    span_at: u64,
    span: Vec<u8>,
}

impl Batch<'_> {
    pub fn ids(&self) -> RangeInclusive<u32> {
        self.first..=self.last
    }

    pub fn record(&self, id: u32) -> Result<Record> {
        if !self.ids().contains(&id) {
            return self.db.record(id);
        }
        let o = (id - self.first) as usize * RECORD_SIZE;
        let mut b = [0; RECORD_SIZE];
        b.copy_from_slice(&self.headers[o..o + RECORD_SIZE]);
        Ok(Record { id, b })
    }

    /// The move record of `record`, from the batch's buffer when it lies
    /// inside it and read on its own otherwise.
    pub fn moves_of(&self, record: &Record) -> Result<MoveData<'_>> {
        let inside = u64::from(record.moves_offset())
            .checked_sub(self.span_at)
            .and_then(|rel| usize::try_from(rel).ok())
            .filter(|&rel| rel.saturating_add(4) <= self.span.len());
        if let Some(rel) = inside {
            let size = be_u24(&self.span, rel + 1) as usize;
            if size >= 4 && rel + size <= self.span.len() {
                return Ok(MoveData { bytes: Cow::Borrowed(&self.span[rel..rel + size]) });
            }
        }
        self.db.moves_of(record)
    }

    /// The annotations of `record`, as [`Database::annotations_of`].
    pub fn annotations_of(&self, record: &Record) -> Result<Option<GameAnnotations>> {
        self.db.annotations_of(record)
    }
}

/// A whole `.cbg` record, borrowed from a batch or owned.
pub struct MoveData<'a> {
    bytes: Cow<'a, [u8]>,
}

impl MoveData<'_> {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn moves(&self) -> Result<GameMoves<'_>> {
        GameMoves::parse(&self.bytes)
    }
}
