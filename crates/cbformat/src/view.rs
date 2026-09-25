//! One view of a database of any format.
//!
//! [`Base`] opens a 2CBH (`.2cbh`) or a classic (`.cbh`) database and reads
//! both the same way: header fields as the [`v2`] types, the names of a
//! game's players, tournament and annotator, the move tree through the same
//! [`TreeVisitor`] walk, annotations, and PGN. [`v2`] and [`cbh`] each
//! provide what it reads. A PGN file, read through its index
//! ([`crate::pgnfile`]), gives the same headers, names and PGN; its moves are
//! only in its text.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::game::{Date, Eco, GameAnnotations, GameResult, Player, RecordKind, Start, Tournament};
use crate::pgn::{self, Options, Rendered};
use crate::replay::{self, TreeStats, TreeVisitor};
use crate::v2;
use crate::{Error, Result, cbh, pgnfile};

/// The format of a database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// ChessBase 17 and later: `.2cbh` and its companions.
    TwoCbh,
    /// The classic format: `.cbh` and its companions.
    Cbh,
    /// A PGN file, `.pgn`.
    Pgn,
}

/// How a format numbers the moves annotations are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionOrder {
    /// In PGN order: each alternative and everything after it right after the
    /// move it replaces. 2CBH numbers them so.
    Pgn,
    /// In stored order: depth first, the main line first at every position.
    /// The classic format numbers them so.
    Stored,
}

/// An open database of any format.
pub enum Base {
    TwoCbh(v2::Database),
    Cbh(cbh::Database),
    Pgn(pgnfile::Database),
}

impl Base {
    /// Opens the database `path` names: a `.2cbh` or `.cbh` file, or a stem
    /// shared by the files, which is read as 2CBH when a `.2cbh` file has it.
    /// A PGN file is opened with its index, [`pgnfile::Database::open`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Ok(match format_of(path) {
            Format::TwoCbh => Base::TwoCbh(v2::Database::open(path)?),
            Format::Cbh => Base::Cbh(cbh::Database::open(path)?),
            Format::Pgn => return Err(Error::Format("a PGN file is read through its index".into())),
        })
    }

    pub fn format(&self) -> Format {
        match self {
            Base::TwoCbh(_) => Format::TwoCbh,
            Base::Cbh(_) => Format::Cbh,
            Base::Pgn(_) => Format::Pgn,
        }
    }

    /// The path of the database without its extension; a PGN file's own path.
    pub fn stem(&self) -> &Path {
        match self {
            Base::TwoCbh(db) => db.stem(),
            Base::Cbh(db) => db.stem(),
            Base::Pgn(db) => db.path(),
        }
    }

    /// The paths of every file of the database and of those beside it, in
    /// both formats, whether or not each exists: a stem may hold a 2CBH and a
    /// classic copy of one database, and an export must overwrite neither.
    /// A PGN file is one file.
    pub fn file_paths(&self) -> Vec<PathBuf> {
        if let Base::Pgn(db) = self {
            return vec![db.path().to_path_buf()];
        }
        let extensions = v2::EXTENSIONS.iter().chain(&v2::BESIDE).chain(&cbh::EXTENSIONS).chain(&cbh::BESIDE);
        let mut paths = crate::file::with_extensions(self.stem(), extensions);
        paths.sort();
        paths.dedup();
        paths
    }

    /// Number of records, including deleted games and guiding texts.
    pub fn record_count(&self) -> u32 {
        match self {
            Base::TwoCbh(db) => db.record_count(),
            Base::Cbh(db) => db.record_count(),
            Base::Pgn(db) => db.record_count(),
        }
    }

    /// Whether the database has an annotation file. A PGN file has none: its
    /// comments are in its text.
    pub fn has_annotations(&self) -> bool {
        match self {
            Base::TwoCbh(db) => db.has_annotations(),
            Base::Cbh(db) => db.has_annotations(),
            Base::Pgn(_) => false,
        }
    }

    /// How [`Base::annotations_of`] numbers the moves.
    pub fn position_order(&self) -> PositionOrder {
        match self {
            Base::TwoCbh(_) | Base::Pgn(_) => PositionOrder::Pgn,
            Base::Cbh(_) => PositionOrder::Stored,
        }
    }

    /// The header of 1-based record `id`.
    pub fn header(&self, id: u32) -> Result<Header> {
        Ok(match self {
            Base::TwoCbh(db) => Header::TwoCbh(db.record(id)?),
            Base::Cbh(db) => Header::Cbh(db.record(id)?),
            Base::Pgn(db) => Header::Pgn(db.record(id)?),
        })
    }

    /// Headers `first..=last`, clamped to the database and to
    /// [`crate::game::MAX_BATCH_RECORDS`] records, in one read.
    pub fn headers(&self, first: u32, last: u32) -> Result<Vec<Header>> {
        Ok(match self {
            Base::TwoCbh(db) => db.records(first, last)?.into_iter().map(Header::TwoCbh).collect(),
            Base::Cbh(db) => db.records(first, last)?.into_iter().map(Header::Cbh).collect(),
            Base::Pgn(db) => db.records(first, last)?.into_iter().map(Header::Pgn).collect(),
        })
    }

    /// The players, tournament and annotator a game names.
    pub fn names(&self, header: &Header) -> Result<Names> {
        match (self, header) {
            (Base::TwoCbh(db), Header::TwoCbh(r)) => {
                let e = db.entities();
                Ok(Names {
                    white: e.player(r.white())?,
                    black: e.player(r.black())?,
                    tournament: e.tournament(r.tournament())?,
                    annotator: e.player(r.annotator())?.map(|p| p.pgn()),
                })
            }
            (Base::Cbh(db), Header::Cbh(r)) => {
                let e = db.entities();
                Ok(Names {
                    white: e.player(r.white())?,
                    black: e.player(r.black())?,
                    tournament: e.tournament(r.tournament())?,
                    annotator: e.annotator(r.annotator())?,
                })
            }
            (Base::Pgn(db), Header::Pgn(r)) => Ok(Names {
                white: db.player(r.white())?,
                black: db.player(r.black())?,
                tournament: db.tournament(r.tournament())?,
                annotator: db.annotator(r.annotator())?,
            }),
            _ => Err(other_format()),
        }
    }

    /// The move record of a game, read whole. A PGN game has none: its moves
    /// are in its text ([`Base::pgn`]).
    pub fn moves_of(&self, header: &Header) -> Result<Moves> {
        match (self, header) {
            (Base::TwoCbh(db), Header::TwoCbh(r)) => Ok(Moves::TwoCbh(db.moves_of(r)?)),
            (Base::Cbh(db), Header::Cbh(r)) => Ok(Moves::Cbh(db.moves_of(r)?)),
            (Base::Pgn(_), Header::Pgn(_)) => Err(Error::Format("a PGN game's moves are in its text".into())),
            _ => Err(other_format()),
        }
    }

    /// The annotations of a game, numbered as [`Base::position_order`] says,
    /// or `None` when the database has no annotation file.
    pub fn annotations_of(&self, header: &Header) -> Result<Option<GameAnnotations>> {
        match (self, header) {
            (Base::TwoCbh(db), Header::TwoCbh(r)) => db.annotations_of(r),
            (Base::Cbh(db), Header::Cbh(r)) => db.annotations_of(r),
            (Base::Pgn(_), Header::Pgn(_)) => Ok(None),
            _ => Err(other_format()),
        }
    }

    /// Game `id` as PGN with its annotations; a PGN game as its file has it.
    pub fn pgn(&self, id: u32, options: &Options) -> Result<Rendered> {
        match self {
            Base::TwoCbh(db) => pgn::game_with(db, id, options),
            Base::Cbh(db) => pgn::classic_game_with(db, id, options),
            Base::Pgn(db) => pgn_text(db, &db.record(id)?),
        }
    }

    /// Records `first..=last` with their moves, read together for export,
    /// clamped as [`Base::headers`] clamps them.
    pub fn batch(&self, first: u32, last: u32) -> Result<Batch<'_>> {
        Ok(match self {
            Base::TwoCbh(db) => Batch::TwoCbh(db, db.batch(first, last)?),
            Base::Cbh(db) => Batch::Cbh(db, db.batch(first, last)?),
            Base::Pgn(db) => {
                let first = first.max(1);
                let last = last.min(db.record_count()).min(first.saturating_add(crate::game::MAX_BATCH_RECORDS - 1));
                Batch::Pgn(db, first..=last)
            }
        })
    }
}

/// A PGN game as its file has it: the comments in its text are all it has.
fn pgn_text(db: &pgnfile::Database, r: &pgnfile::Record) -> Result<Rendered> {
    Ok(Rendered { pgn: db.text(r, pgnfile::MAX_TEXT)?, annotations: pgn::AnnotationStatus::Complete })
}

/// The format of the database `path` names; see [`Base::open`].
pub fn format_of(path: &Path) -> Format {
    let has = |ext: &str| path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext));
    if has("2cbh") {
        return Format::TwoCbh;
    }
    if has("cbh") {
        return Format::Cbh;
    }
    if has("pgn") {
        return Format::Pgn;
    }
    let with = |ext: &str| {
        let mut s = path.as_os_str().to_owned();
        s.push(ext);
        PathBuf::from(s)
    };
    if !with(".2cbh").exists() && with(".cbh").exists() { Format::Cbh } else { Format::TwoCbh }
}

fn other_format() -> Error {
    Error::Format("a header of one format given to a database of the other".into())
}

/// A header record of any format. Its fields are read as the 2CBH ones.
#[derive(Clone, Copy)]
pub enum Header {
    TwoCbh(v2::Record),
    Cbh(cbh::Record),
    Pgn(pgnfile::Record),
}

impl Header {
    pub fn id(&self) -> u32 {
        match self {
            Header::TwoCbh(r) => r.id(),
            Header::Cbh(r) => r.id(),
            Header::Pgn(r) => r.id(),
        }
    }
    /// A PGN file holds games only.
    pub fn kind(&self) -> RecordKind {
        match self {
            Header::TwoCbh(r) => r.kind(),
            Header::Cbh(r) => r.kind(),
            Header::Pgn(_) => RecordKind::Game,
        }
    }
    pub fn is_deleted(&self) -> bool {
        match self {
            Header::TwoCbh(r) => r.is_deleted(),
            Header::Cbh(r) => r.is_deleted(),
            Header::Pgn(_) => false,
        }
    }
    pub fn result(&self) -> GameResult {
        match self {
            Header::TwoCbh(r) => r.result(),
            Header::Cbh(r) => r.result(),
            Header::Pgn(r) => r.result(),
        }
    }
    pub fn eco(&self) -> Eco {
        match self {
            Header::TwoCbh(r) => r.eco(),
            Header::Cbh(r) => r.eco(),
            Header::Pgn(r) => r.eco(),
        }
    }
    pub fn played_date(&self) -> Date {
        match self {
            Header::TwoCbh(r) => r.played_date(),
            Header::Cbh(r) => r.played_date(),
            Header::Pgn(r) => r.played_date(),
        }
    }
    /// Round and sub-round; 0 when unknown.
    pub fn round(&self) -> (i32, i32) {
        match self {
            Header::TwoCbh(r) => (i32::from(r.round()), i32::from(r.subround())),
            Header::Cbh(r) => (i32::from(r.round()), i32::from(r.subround())),
            Header::Pgn(r) => (i32::from(r.round().0), i32::from(r.round().1)),
        }
    }
    /// White's and black's ratings; 0 when unknown.
    pub fn elo(&self) -> (i32, i32) {
        match self {
            Header::TwoCbh(r) => (i32::from(r.white_elo()), i32::from(r.black_elo())),
            Header::Cbh(r) => (i32::from(r.white_elo()), i32::from(r.black_elo())),
            Header::Pgn(r) => (i32::from(r.elo().0), i32::from(r.elo().1)),
        }
    }
    /// Moves in the main line, as the header stores them (the classic format
    /// caps them at 255; a PGN file's are counted as written).
    pub fn move_count(&self) -> i32 {
        match self {
            Header::TwoCbh(r) => i32::from(r.move_count()),
            Header::Cbh(r) => i32::from(r.move_count()),
            Header::Pgn(r) => i32::from(r.move_count()),
        }
    }
}

/// The entities a game names; `None` for an unused or unreadable entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    pub white: Option<Player>,
    pub black: Option<Player>,
    pub tournament: Option<Tournament>,
    pub annotator: Option<String>,
}

/// A game's move record, in either format.
pub enum Moves {
    TwoCbh(v2::MoveData<'static>),
    Cbh(cbh::MoveData<'static>),
}

impl Moves {
    /// Walks every line of the tree, checking each move, and reports it to
    /// `visitor` in stored order, the same for both formats.
    pub fn walk(&self, visitor: &mut impl TreeVisitor) -> Result<TreeStats> {
        match self {
            Moves::TwoCbh(m) => replay::walk(&m.moves()?, visitor),
            Moves::Cbh(m) => cbh::walk(&m.moves()?, visitor),
        }
    }

    /// Where the game starts, as its moves play it.
    pub fn start(&self) -> Result<Start> {
        match self {
            Moves::TwoCbh(m) => m.moves()?.start(),
            Moves::Cbh(m) => cbh::start_as_played(&m.moves()?),
        }
    }

    pub fn is_chess960(&self) -> Result<bool> {
        Ok(match self {
            Moves::TwoCbh(m) => m.moves()?.is_chess960(),
            Moves::Cbh(m) => m.moves()?.is_chess960(),
        })
    }
}

/// Records read together by [`Base::batch`]. A PGN file's are read one at a
/// time.
pub enum Batch<'db> {
    TwoCbh(&'db v2::Database, v2::Batch<'db>),
    Cbh(&'db cbh::Database, cbh::Batch<'db>),
    Pgn(&'db pgnfile::Database, RangeInclusive<u32>),
}

impl Batch<'_> {
    pub fn ids(&self) -> RangeInclusive<u32> {
        match self {
            Batch::TwoCbh(_, b) => b.ids(),
            Batch::Cbh(_, b) => b.ids(),
            Batch::Pgn(_, ids) => ids.clone(),
        }
    }

    pub fn header(&self, id: u32) -> Result<Header> {
        Ok(match self {
            Batch::TwoCbh(_, b) => Header::TwoCbh(b.record(id)?),
            Batch::Cbh(_, b) => Header::Cbh(b.record(id)?),
            Batch::Pgn(db, ids) if ids.contains(&id) => Header::Pgn(db.record(id)?),
            Batch::Pgn(..) => return Err(Error::NoSuchGame(id)),
        })
    }

    /// Game `id` as PGN, from the batch's buffers when it lies inside them.
    pub fn pgn(&self, id: u32, options: &Options) -> Result<Rendered> {
        match self {
            Batch::TwoCbh(db, b) => {
                let r = b.record(id)?;
                if r.kind() != RecordKind::Game {
                    return pgn::game_with(db, id, options);
                }
                let data = b.moves_of(&r)?;
                let annotations = b.annotations_of(&r)?;
                pgn::game_from(db, &r, &data.moves()?, annotations.as_ref(), options)
            }
            Batch::Cbh(db, b) => {
                let r = b.record(id)?;
                if r.kind() != RecordKind::Game {
                    return pgn::classic_game_with(db, id, options);
                }
                let data = b.moves_of(&r)?;
                let annotations = b.annotations_of(&r)?;
                pgn::classic_game_from(db, &r, &data.moves()?, annotations.as_ref(), options)
            }
            Batch::Pgn(db, ids) if ids.contains(&id) => pgn_text(db, &db.record(id)?),
            Batch::Pgn(..) => Err(Error::NoSuchGame(id)),
        }
    }
}
