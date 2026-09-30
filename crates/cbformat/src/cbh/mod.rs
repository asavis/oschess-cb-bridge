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

use crate::file::{self, DbFile};
use crate::game::{GameAnnotations, Names, Source};
use crate::recordfile::{RecordFile, Run, over_limit, span};
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
pub use decode::{MAX_VARIATION_DEPTH, start_as_played, walk, walk_from};
pub use entities::{Entities, Entity};
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
/// The files the reader opens, the header file first: headers, moves and
/// texts, annotations, the four entity files, and the 64-bit offsets ChessBase
/// adds for files over 4 GiB (#66). [`Database::open`] opens no other, and in
/// a debug build it stops at a file not listed here, so that a reader change
/// updates this list. The bridge follows these files to see a database change.
pub const READ: [&str; 8] = [".cbh", ".cbg", ".cba", ".cbp", ".cbt", ".cbc", ".cbs", ".cbj"];
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

/// Whether a classic database needs its `.cbj` to be read: when its `.cbg`,
/// `moves_len` bytes long, or its `.cba`, `annotations_len` bytes when it has
/// one, is longer than the 32-bit offsets of `.cbh` reach. [`Database::open`]
/// opens `.cbj` exactly then, and [`crate::view::Format::files`] calls it
/// required exactly then.
pub fn needs_wide(moves_len: u64, annotations_len: Option<u64>) -> bool {
    let large = |len: u64| len > u64::from(u32::MAX);
    large(moves_len) || annotations_len.is_some_and(large)
}

/// An open classic database: game headers, moves and entities.
pub struct Database {
    stem: PathBuf,
    headers: RecordFile<RECORD_SIZE>,
    moves: DbFile,
    /// `None` when the database has no `.cba` file.
    annotations: Option<DbFile>,
    /// The 64-bit offsets of `.cbj`, read only when `.cbg` or `.cba` is over
    /// 4 GiB and the 32-bit ones of `.cbh` cannot reach every record.
    wide: Option<wide::Wide>,
    entities: Entities,
    format_version: u8,
}

impl Database {
    /// Opens the database whose files share `path`'s stem. `path` may name the
    /// `.cbh` file or the bare stem. The record count is taken now.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let stem = file::stem(path.as_ref(), "cbh");
        let with = |ext: &str| {
            debug_assert!(READ.contains(&ext), "the classic reader opens {ext}, which cbh::READ does not list");
            file::with_extension(&stem, ext)
        };
        let headers = RecordFile::open(with(".cbh"), ".cbh")?;
        let header = headers.file().read(0, RECORD_SIZE)?;
        let record_size = be_u16(&header, 0x03);
        if record_size as usize != RECORD_SIZE {
            return Err(Error::Format(format!(".cbh record size {record_size}, expected 46")));
        }
        let moves = DbFile::open(with(".cbg"))?;
        let annotations = DbFile::open_optional(with(".cba"))?;
        let annotations_len = annotations.as_ref().map(DbFile::len).transpose()?;
        let wide = if needs_wide(moves.len()?, annotations_len) { Some(wide::Wide::open(with(".cbj"))?) } else { None };
        let entities = Entities::open(with)?;
        Ok(Database { stem, headers, moves, annotations, wide, entities, format_version: header[0x05] })
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
        self.headers.count()
    }

    /// The format version byte of the header file.
    pub fn format_version(&self) -> u8 {
        self.format_version
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

    /// The record for 1-based game id `id`.
    pub fn record(&self, id: u32) -> Result<Record> {
        Ok(Record { id, b: self.headers.record(id)? })
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
            return Err(Error::Format(format!("move record at {at:#x}: {}", over_limit(size, limit))));
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
            return Ok(Some(GameAnnotations { source: Source::Classic, ..GameAnnotations::default() }));
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
            return Err(bad(&over_limit(size, limit)));
        }
        annotations::parse(&file.read(at, size)?, record.id()).map(Some)
    }

    /// Reads the header records from `first` into `buf`, as many as it holds
    /// ([`RECORD_SIZE`] bytes each) up to the last record, in one read, and
    /// returns how many it read: none when `first` is 0 or past the end. It
    /// allocates nothing, as [`crate::v2::Database::read_records`];
    /// [`Record::from_bytes`] makes the records.
    pub fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        self.headers.read_records(first, buf)
    }

    /// Records `first..=last`, clamped to the database and to
    /// [`crate::game::MAX_BATCH_RECORDS`] records, in one read.
    pub fn records(&self, first: u32, last: u32) -> Result<Vec<Record>> {
        self.headers.records(first, last, Record::from_bytes)
    }

    /// Records `first..=last`, clamped as [`Database::records`] clamps them,
    /// with their move records, read in two large reads. [`Batch::ids`] gives
    /// the ids read.
    pub fn batch(&self, first: u32, last: u32) -> Result<Batch<'_>> {
        // One record past the batch, when there is one: its move record starts
        // where the batch's last one ends, as move records are in id order.
        // With `.cbj`, the headers' 32-bit offsets cannot place a span, and
        // every move record is read on its own through it.
        let run = self.headers.run(first, last, self.wide.is_none())?;
        let offset = |r: &[u8; RECORD_SIZE]| u64::from(Record::from_bytes(0, r).moves_offset());
        let placed = match self.wide {
            Some(_) => None,
            None => span(run.records().iter().map(offset), run.next().map(offset), self.moves.len()?, MIN_FILE_HEADER),
        };
        let (span_at, span) = match placed {
            Some(s) if s.end - s.start <= MAX_BATCH_SPAN => {
                (s.start, self.moves.read(s.start, (s.end - s.start) as usize)?)
            }
            _ => (0, Vec::new()),
        };
        Ok(Batch { db: self, run, span_at, span })
    }
}

/// A run of records read together by [`Database::batch`].
pub struct Batch<'db> {
    db: &'db Database,
    /// The records of the batch, and possibly the one after it.
    run: Run<RECORD_SIZE>,
    span_at: u64,
    span: Vec<u8>,
}

impl Batch<'_> {
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
        self.moves_of_within(record, usize::MAX)
    }

    /// [`Batch::moves_of`], refusing a record larger than `limit` bytes, as
    /// [`Database::moves_of_within`] refuses it, however the batch's buffer
    /// holds it.
    pub fn moves_of_within(&self, record: &Record, limit: usize) -> Result<MoveData<'_>> {
        let inside = u64::from(record.moves_offset())
            .checked_sub(self.span_at)
            .and_then(|rel| usize::try_from(rel).ok())
            .filter(|&rel| rel.saturating_add(4) <= self.span.len());
        if let Some(rel) = inside {
            let size = be_u24(&self.span, rel + 1) as usize;
            if (4..=limit).contains(&size) && rel + size <= self.span.len() {
                return Ok(MoveData { bytes: Cow::Borrowed(&self.span[rel..rel + size]) });
            }
        }
        // Read on its own, which also refuses a record over `limit`.
        self.db.moves_of_within(record, limit)
    }

    /// The annotations of `record`, as [`Database::annotations_of`].
    pub fn annotations_of(&self, record: &Record) -> Result<Option<GameAnnotations>> {
        self.db.annotations_of(record)
    }

    /// The annotations of `record`, as [`Database::annotations_of_within`].
    pub fn annotations_of_within(&self, record: &Record, limit: usize) -> Result<Option<GameAnnotations>> {
        self.db.annotations_of_within(record, limit)
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
