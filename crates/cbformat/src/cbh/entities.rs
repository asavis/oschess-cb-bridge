//! `.cbp` `.cbt` `.cbc` `.cbs`: players, tournaments, annotators and sources.
//!
//! Each file is a small little-endian header and fixed-size records; an entity
//! id is a record's 0-based position. The records also form a sorted tree,
//! which a reader addressing entities by id does not need.

use std::ops::Range;
use std::path::PathBuf;

use super::text::text;
use crate::bytes::Fields;
use crate::file::DbFile;
use crate::game::{Date, Player, Tournament};
use crate::{Error, Result};

/// Bytes of an entity file's header, before 0 or 4 more.
const HEADER: usize = 28;
/// The fixed value at 0x08 of every entity file header.
const MAGIC: i32 = 1_234_567_890;
/// Largest record data accepted; the real ones are at most 1,608 bytes.
const MAX_DATA: i32 = 64 << 10;
/// The left child of a deleted record.
const DELETED: i32 = -999;
/// Bytes of a record before its data: the tree's links and balance.
const LINKS: usize = 9;

// Where the fields read lie in the data of each file's records. A file whose
// records hold less data than the fields read from them, `*_DATA`, is
// refused when opened, so that every field lies inside every record.
const PLAYER_LAST: Range<usize> = 0..30;
const PLAYER_FIRST: Range<usize> = 30..50;
const PLAYER_DATA: usize = end(&[PLAYER_LAST, PLAYER_FIRST]);
const TOURNAMENT_TITLE: Range<usize> = 0..40;
const TOURNAMENT_PLACE: Range<usize> = 40..70;
/// The start date, an `int`.
const TOURNAMENT_START: Range<usize> = 0x46..0x4a;
/// The type byte (#268): every `.cbt` of the 251 databases examined stores 90
/// bytes of data a record.
const TOURNAMENT_KIND: Range<usize> = 0x4a..0x4b;
const TOURNAMENT_DATA: usize = end(&[TOURNAMENT_TITLE, TOURNAMENT_PLACE, TOURNAMENT_START, TOURNAMENT_KIND]);
const ANNOTATOR_NAME: Range<usize> = 0..45;
const ANNOTATOR_DATA: usize = end(&[ANNOTATOR_NAME]);
/// The source's title.
const SOURCE_TITLE: Range<usize> = 0..25;
const SOURCE_DATA: usize = end(&[SOURCE_TITLE]);

/// Where the last of `fields` ends.
const fn end(fields: &[Range<usize>]) -> usize {
    let (mut i, mut end) = (0, 0);
    while i < fields.len() {
        if fields[i].end > end {
            end = fields[i].end;
        }
        i += 1;
    }
    end
}

/// The bytes `at` of a record's `data`, which [`EntityFile::open`] makes
/// long enough to hold them.
fn field(data: &[u8], at: Range<usize>) -> &[u8] {
    data.get(at).unwrap_or_default()
}

/// One entity file.
struct EntityFile {
    file: DbFile,
    header: u64,
    record: u64,
    count: u64,
}

impl EntityFile {
    /// Opens `path`, whose records must carry at least `min_data` bytes of data.
    fn open(path: PathBuf, min_data: usize) -> Result<Self> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bad = |what: String| Error::Format(format!("{name}: {what}"));
        let file = DbFile::open(path)?;
        let len = file.size()?;
        if len < HEADER as u64 {
            return Err(bad(format!("{len}-byte file is shorter than its header")));
        }
        let mut h = [0u8; HEADER];
        file.read_into(0, &mut h)?;
        if h.le_i32::<0x08>() != MAGIC {
            return Err(bad("bad magic".into()));
        }
        let data = h.le_i32::<0x0c>();
        if !(min_data as i32..=MAX_DATA).contains(&data) {
            return Err(bad(format!("record data size {data}")));
        }
        let extra = h.le_i32::<0x18>();
        if extra != 0 && extra != 4 {
            return Err(bad(format!("{extra} extra header bytes")));
        }
        let header = (HEADER + extra as usize) as u64;
        let record = (LINKS + data as usize) as u64;
        let count = len.saturating_sub(header) / record;
        Ok(EntityFile { file, header, record, count })
    }

    /// The data of entity `id`, or `None` for an id past the file or a
    /// deleted record.
    fn data(&self, id: u32) -> Result<Option<Vec<u8>>> {
        if u64::from(id) >= self.count {
            return Ok(None);
        }
        let r = self.file.read(self.header + u64::from(id) * self.record, self.record as usize)?;
        if r.first_chunk().map(|&left| i32::from_le_bytes(left)) == Some(DELETED) {
            return Ok(None);
        }
        Ok(Some(r.get(LINKS..).unwrap_or_default().to_vec()))
    }
}

/// One of the entity files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entity {
    Player,
    Tournament,
    Annotator,
    Source,
}

/// The entity files of a classic database.
pub struct Entities {
    players: EntityFile,
    tournaments: EntityFile,
    annotators: EntityFile,
    sources: EntityFile,
}

impl Entities {
    pub(super) fn open(with: impl Fn(&str) -> PathBuf) -> Result<Self> {
        Ok(Entities {
            players: EntityFile::open(with(".cbp"), PLAYER_DATA)?,
            tournaments: EntityFile::open(with(".cbt"), TOURNAMENT_DATA)?,
            annotators: EntityFile::open(with(".cbc"), ANNOTATOR_DATA)?,
            sources: EntityFile::open(with(".cbs"), SOURCE_DATA)?,
        })
    }

    /// Records in each file, deleted ones included: players, tournaments,
    /// annotators, sources.
    pub fn counts(&self) -> [u64; 4] {
        [self.players.count, self.tournaments.count, self.annotators.count, self.sources.count]
    }

    /// The stored data of entity `id`, which [`Self::player`] and the others
    /// decode, or `None` for an id past the file or a deleted record. A
    /// name's bytes up to its terminating zero are its length in the field,
    /// whatever its encoding.
    pub fn data(&self, entity: Entity, id: u32) -> Result<Option<Vec<u8>>> {
        match entity {
            Entity::Player => &self.players,
            Entity::Tournament => &self.tournaments,
            Entity::Annotator => &self.annotators,
            Entity::Source => &self.sources,
        }
        .data(id)
    }

    pub fn player(&self, id: u32) -> Result<Option<Player>> {
        Ok(self
            .players
            .data(id)?
            .map(|d| Player { last: text(field(&d, PLAYER_LAST)), first: text(field(&d, PLAYER_FIRST)) }))
    }

    pub fn tournament(&self, id: u32) -> Result<Option<Tournament>> {
        Ok(self.tournaments.data(id)?.map(|d| Tournament {
            title: text(field(&d, TOURNAMENT_TITLE)),
            place: text(field(&d, TOURNAMENT_PLACE)),
            start: Date(field(&d, TOURNAMENT_START).try_into().map_or(0, i32::from_le_bytes)),
            kind: field(&d, TOURNAMENT_KIND).first().copied().unwrap_or(0),
        }))
    }

    pub fn annotator(&self, id: u32) -> Result<Option<String>> {
        Ok(self.annotators.data(id)?.map(|d| text(field(&d, ANNOTATOR_NAME))))
    }

    /// The source's title.
    pub fn source(&self, id: u32) -> Result<Option<String>> {
        Ok(self.sources.data(id)?.map(|d| text(field(&d, SOURCE_TITLE))))
    }
}
