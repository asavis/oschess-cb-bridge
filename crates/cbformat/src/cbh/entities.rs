//! `.cbp` `.cbt` `.cbc` `.cbs`: players, tournaments, annotators and sources.
//!
//! Each file is a small little-endian header and fixed-size records; an entity
//! id is a record's 0-based position. The records also form a sorted tree,
//! which a reader addressing entities by id does not need.

use std::path::PathBuf;

use super::bytes::{le_i32, text};
use crate::file::DbFile;
use crate::game::{Date, Player, Tournament};
use crate::{Error, Result};

/// The fixed value at 0x08 of every entity file header.
const MAGIC: i32 = 1_234_567_890;
/// Largest record data accepted; the real ones are at most 1,608 bytes.
const MAX_DATA: i32 = 64 << 10;
/// The left child of a deleted record.
const DELETED: i32 = -999;

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
        let len = file.len()?;
        if len < 28 {
            return Err(bad(format!("{len}-byte file is shorter than its header")));
        }
        let h = file.read(0, 28)?;
        if le_i32(&h, 0x08) != MAGIC {
            return Err(bad("bad magic".into()));
        }
        let data = le_i32(&h, 0x0c);
        if !(min_data as i32..=MAX_DATA).contains(&data) {
            return Err(bad(format!("record data size {data}")));
        }
        let extra = le_i32(&h, 0x18);
        if extra != 0 && extra != 4 {
            return Err(bad(format!("{extra} extra header bytes")));
        }
        let header = 28 + extra as u64;
        let record = 9 + data as u64;
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
        if le_i32(&r, 0) == DELETED {
            return Ok(None);
        }
        Ok(Some(r[9..].to_vec()))
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
            players: EntityFile::open(with(".cbp"), 50)?,
            tournaments: EntityFile::open(with(".cbt"), 0x4a)?,
            annotators: EntityFile::open(with(".cbc"), 45)?,
            sources: EntityFile::open(with(".cbs"), 25)?,
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
        Ok(self.players.data(id)?.map(|d| Player { last: text(&d[..30]), first: text(&d[30..50]) }))
    }

    pub fn tournament(&self, id: u32) -> Result<Option<Tournament>> {
        Ok(self.tournaments.data(id)?.map(|d| Tournament {
            title: text(&d[..40]),
            place: text(&d[40..70]),
            start: Date(le_i32(&d, 0x46)),
        }))
    }

    pub fn annotator(&self, id: u32) -> Result<Option<String>> {
        Ok(self.annotators.data(id)?.map(|d| text(&d[..45])))
    }

    /// The source's title.
    pub fn source(&self, id: u32) -> Result<Option<String>> {
        Ok(self.sources.data(id)?.map(|d| text(&d[..25])))
    }
}
