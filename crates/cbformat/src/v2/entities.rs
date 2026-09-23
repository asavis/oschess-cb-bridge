//! `.2lid`: players, tournaments, sources, teams and game tags.

use super::bytes::{be_i32, be_i64, le_i32};
use super::file::DbFile;
use super::record::Date;
use crate::{Error, Result};

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
    pub(super) fn new(file: DbFile) -> Result<Self> {
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

impl Entities {
    /// The first non-empty title of game tag `id`: the title of the guiding
    /// text or analysis that refers to it. A tag holds one title per language;
    /// `None` when all are empty or the id is unused. Errors as for
    /// [`Entities::raw`].
    pub fn title(&self, id: i64) -> Result<Option<String>> {
        Ok(self.raw(GAME_TAG, id)?.and_then(|r| {
            let mut c = Cursor(&r, 0);
            let count = c.i32()?;
            for _ in 0..count.max(0) {
                let _language = c.i32()?;
                let title = c.string()?;
                if !title.is_empty() {
                    return Some(title);
                }
            }
            None
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
