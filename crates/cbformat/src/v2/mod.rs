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
mod entities;
mod frame;
mod guide;
mod moves;
mod record;
mod window;

pub use annotations::ANNOTATION_TAG;
pub use entities::{Entities, GAME_TAG, PLAYER, SOURCE, TEAM, TOURNAMENT};
pub use frame::checksum;
use frame::{MAX_FRAME_PART, frame_at, read_frame_into};
pub use guide::TEXT_TAG;
pub use moves::{GameMoves, Token};
pub use record::Record;
pub use window::MoveWindow;

use crate::bytes::Fields;
use crate::file::{self, DbFile};
use crate::game::{GameAnnotations, Names};
use crate::recordfile::{RecordFile, Run, span};

pub const HEADER_RECORD_SIZE: usize = 192;

/// The extensions of the files that make up a database.
pub const EXTENSIONS: [&str; 6] = [".2cbh", ".2cbg", ".2cba", ".2lid", ".2lgd", ".2lcd"];
/// The files [`Database::open`] opens, `.2cba` when it is there: the others
/// of [`EXTENSIONS`] are part of the database but never read.
pub const READ: [&str; 4] = [".2cbh", ".2cbg", ".2cba", ".2lid"];
/// Files that sit beside a database under its name without being needed to
/// read it: its settings and the opening key files. An export must not
/// overwrite them either.
pub const BESIDE: [&str; 3] = [".ini", ".cko", ".cpo"];
/// Largest span of `.2cbg` read for one batch; a batch whose moves lie wider
/// apart reads each move record on its own.
const MAX_BATCH_SPAN: u64 = 256 << 20;
/// Bytes of the header of `.2cbg` and `.2cba`, before the first record: the
/// file's length, the header's length and the version.
const FILE_HEADER: u64 = 12;

/// An open 2CBH database: game headers, moves, annotations and entities.
pub struct Database {
    stem: PathBuf,
    headers: RecordFile<HEADER_RECORD_SIZE>,
    moves: DbFile,
    /// `None` when the database has no `.2cba` file.
    annotations: Option<DbFile>,
    entities: Entities,
    format_version: u8,
}

impl Database {
    /// Opens the database whose files share `path`'s stem. `path` may name the
    /// `.2cbh` file or the bare stem. The record count is taken now; games
    /// added later are seen by opening the database again.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let stem = file::stem(path.as_ref(), "2cbh");
        let with = |ext: &str| file::with_extension(&stem, ext);
        let headers = RecordFile::open(with(".2cbh"), ".2cbh")?;
        let mut header = [0u8; HEADER_RECORD_SIZE];
        headers.file().read_into(0, &mut header)?;
        let record_size = header.le_i16::<0x0a>();
        if record_size as usize != HEADER_RECORD_SIZE {
            return Err(Error::Format(format!(".2cbh record size {record_size}, expected 192")));
        }
        let moves = DbFile::open(with(".2cbg"))?;
        let annotations = DbFile::open_optional(with(".2cba"))?;
        let entities = Entities::new(DbFile::open(with(".2lid"))?)?;
        Ok(Database { stem, headers, moves, annotations, entities, format_version: header[0x0d] })
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
        self.headers.count()
    }

    /// The format version byte of the header file.
    pub fn format_version(&self) -> u8 {
        self.format_version
    }

    /// The record for 1-based game id `id`.
    pub fn record(&self, id: u32) -> Result<Record> {
        Ok(Record { id, b: self.headers.record(id)? })
    }

    pub fn entities(&self) -> &Entities {
        &self.entities
    }

    /// The players and the tournament game `record` names, which its PGN tags
    /// hold: the one lookup of them that the PGN writer and
    /// [`crate::view::Base::names`] share. The annotator, which the tags do
    /// not hold, is left `None`.
    pub(crate) fn tag_names(&self, record: &Record) -> Result<Names> {
        let e = &self.entities;
        let tournament = e.tournament(record.tournament())?;
        Ok(Names { white: e.player(record.white())?, black: e.player(record.black())?, tournament, annotator: None })
    }

    /// The move record a game or analysis header points at.
    pub fn moves_of(&self, record: &Record) -> Result<MoveData<'static>> {
        self.moves_of_within(record, MAX_FRAME_PART)
    }

    /// [`Database::moves_of`], refusing before it is read a move record whose
    /// content or spare area is larger than `limit` bytes: a caller that must
    /// bound its work per game (a server) chooses the limit.
    pub fn moves_of_within(&self, record: &Record, limit: usize) -> Result<MoveData<'static>> {
        let mut frame = Vec::new();
        let (tag, content) =
            read_frame_into(&self.moves, record.moves_offset(), limit, usize::MAX, "move record", &mut frame)?;
        Ok(MoveData { tag, content: Cow::Owned(content.to_vec()) })
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
        let mut frame = Vec::new();
        let (tag, content) = read_frame_into(file, offset, limit, usize::MAX, "annotation record", &mut frame)?;
        annotation_content(tag, content, offset).map(Some)
    }

    /// Reads the header records from `first` into `buf`, as many as it holds
    /// (192 bytes each) up to the last record, in one read, and returns how
    /// many it read: none when `first` is 0 or past the end. It allocates
    /// nothing, so a caller that scans many records reuses one buffer whose
    /// memory it has accounted for; [`Record::from_bytes`] makes the records.
    pub fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        self.headers.read_records(first, buf)
    }

    /// Records `first..=last`, clamped to the database and to
    /// [`crate::game::MAX_BATCH_RECORDS`] records, in one read. Each carries
    /// its id; fewer records than asked may come back.
    pub fn records(&self, first: u32, last: u32) -> Result<Vec<Record>> {
        self.headers.records(first, last, Record::from_bytes)
    }

    /// Records `first..=last`, clamped to the database and to
    /// [`crate::game::MAX_BATCH_RECORDS`] records, with their move records,
    /// read in two large reads, for scanning many games. [`Batch::ids`] gives
    /// the ids read.
    pub fn batch(&self, first: u32, last: u32) -> Result<Batch<'_>> {
        // One record past the batch, when there is one: its move record starts
        // where the batch's last one ends, since move records are stored back
        // to back in id order.
        let run = self.headers.run(first, last, true)?;
        let moves = Span::read::<0x08>(&self.moves, &run)?;
        let annotations = match &self.annotations {
            Some(file) => Span::read::<0x10>(file, &run)?,
            None => Span::EMPTY,
        };
        Ok(Batch { db: self, run, moves, annotations })
    }
}

/// Where a record of `.2cbg` or `.2cba` starts, as [`span`] takes it: a
/// negative offset, which names no record, as 0, inside the file header.
fn position(offset: i64) -> u64 {
    u64::try_from(offset).unwrap_or(0)
}

/// A run of records read together by [`Database::batch`].
pub struct Batch<'db> {
    db: &'db Database,
    /// The records of the batch, and possibly the one after it.
    run: Run<HEADER_RECORD_SIZE>,
    moves: Span,
    annotations: Span,
}

impl<'db> Batch<'db> {
    /// The ids the batch covers.
    pub fn ids(&self) -> RangeInclusive<u32> {
        self.run.ids()
    }

    pub fn record(&self, id: u32) -> Result<Record> {
        match self.run.get(id) {
            Some(b) => Ok(Record { id, b }),
            None => self.db.record(id),
        }
    }

    /// The move record of `record`, from the batch's buffer when it lies
    /// inside it and read on its own otherwise.
    pub fn moves_of(&self, record: &Record) -> Result<MoveData<'_>> {
        self.moves_of_within(record, MAX_FRAME_PART)
    }

    /// [`Batch::moves_of`], refusing a move record whose content or spare
    /// area is larger than `limit` bytes, as [`Database::moves_of_within`]
    /// refuses it, however the batch's buffer holds it.
    pub fn moves_of_within(&self, record: &Record, limit: usize) -> Result<MoveData<'_>> {
        match self.moves.frame(record.moves_offset(), limit, "move record") {
            Some(found) => {
                let (tag, content) = found?;
                Ok(MoveData { tag, content: Cow::Borrowed(content) })
            }
            None => self.db.moves_of_within(record, limit),
        }
    }

    /// The annotations of `record`, as [`Database::annotations_of`], from the
    /// batch's buffer when the record lies inside it.
    pub fn annotations_of(&self, record: &Record) -> Result<Option<GameAnnotations>> {
        self.annotations_of_within(record, MAX_FRAME_PART)
    }

    /// [`Batch::annotations_of`], refusing an annotation record whose content
    /// or spare area is larger than `limit` bytes, as
    /// [`Database::annotations_of_within`] refuses it.
    pub fn annotations_of_within(&self, record: &Record, limit: usize) -> Result<Option<GameAnnotations>> {
        if !self.db.has_annotations() {
            return Ok(None);
        }
        let offset = record.annotations_offset();
        match self.annotations.frame(offset, limit, "annotation record") {
            Some(found) => {
                let (tag, content) = found?;
                annotation_content(tag, content, offset).map(Some)
            }
            None => self.db.annotations_of_within(record, limit),
        }
    }
}

/// A stretch of `.2cbg` or `.2cba` read for a batch: the [`span`] of the
/// batch's records.
struct Span {
    at: u64,
    bytes: Vec<u8>,
}

impl Span {
    const EMPTY: Span = Span { at: 0, bytes: Vec::new() };

    /// The span of `file` for the offsets at `FIELD` of the records of `run`.
    fn read<const FIELD: usize>(file: &DbFile, run: &Run<HEADER_RECORD_SIZE>) -> Result<Span> {
        let offset = |r: &[u8; HEADER_RECORD_SIZE]| position(r.le_i64::<FIELD>());
        let offsets = run.records().iter().map(offset);
        match span(offsets, run.next().map(offset), file.size()?, FILE_HEADER) {
            Some(s) if s.end - s.start <= MAX_BATCH_SPAN => {
                Ok(Span { at: s.start, bytes: file.read(s.start, (s.end - s.start) as usize)? })
            }
            _ => Ok(Span::EMPTY),
        }
    }

    /// The tag and content of the frame at `offset`, or `None` when the frame
    /// does not lie wholly inside the span; one whose content or spare area
    /// is over `limit` bytes is refused, and `kind` names the record.
    fn frame(&self, offset: i64, limit: usize, kind: &str) -> Option<Result<(u16, &[u8])>> {
        frame_at(&self.bytes, self.at, offset, limit, kind)
    }
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

    /// The record's content, after its framing.
    pub fn content(&self) -> &[u8] {
        &self.content
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
