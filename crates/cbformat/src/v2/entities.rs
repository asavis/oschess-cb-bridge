//! `.2lid`: players, tournaments, sources, teams and game tags.

use std::ops::Range;

use super::bytes::{be_i32, be_i64, le_i32};
use crate::file::DbFile;
use crate::game::{Date, Player, Tournament};
use crate::{Error, Result};

/// Largest container size accepted; the real ones are at most 1,120 bytes.
const MAX_CONTAINER: i32 = 1 << 20;
/// Largest entity-file header accepted; the real ones are at most 236 bytes.
const MAX_ENTITY_HEADER: usize = 64 << 10;
/// Largest block of one id's containers read whole to read many ids at once;
/// the real ones are about 4 KiB. The ids of a larger block, which only a
/// damaged or hostile file has, are read one container at a time.
const MAX_RUN_BLOCK: usize = 64 << 10;

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

    /// The ids of type `typ` that can hold an entity: the header's count,
    /// bounded by the containers that start inside the file as it was when
    /// opened. Ids from 0 up to this are worth reading; the rest are missing.
    pub fn stored_count(&self, typ: usize) -> i64 {
        let Some(&(_, count, _)) = self.types.get(typ) else { return 0 };
        if self.block_size == 0 {
            return 0;
        }
        let start = (self.header_size + self.container_offset[typ]) as u64;
        let fit = self.len.saturating_sub(start).div_ceil(self.block_size as u64);
        count.min(i64::try_from(fit).unwrap_or(i64::MAX))
    }

    /// The record bytes after the length field, or `None` for an unused id or
    /// one past the end of the file as it was when opened. One positional read
    /// of the container; a container inside that length that cannot be read
    /// now, because the file was truncated or failed, is an error.
    pub fn raw(&self, typ: usize, id: i64) -> Result<Option<Vec<u8>>> {
        self.raw_within(typ, id, usize::MAX)
    }

    /// [`Entities::raw`], reading at most `limit` record bytes: a record longer
    /// than that is `None`, as an unreadable one is, and is never read whole.
    pub fn raw_within(&self, typ: usize, id: i64, limit: usize) -> Result<Option<Vec<u8>>> {
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
        let want = (size as u64).min(self.len.saturating_sub(o)).min(limit.saturating_add(4) as u64) as usize;
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

    /// The records of type `typ` from `ids.start` on, as
    /// [`Entities::raw_within`] returns them one by one, handed to `each` in
    /// id order: as many as `buf` holds the containers of, read at once, and
    /// never past `ids.end`. Ids of the type all lie in one block each, so
    /// reading many consecutive ones is one positional read instead of one per
    /// id. How many ids it handed over: none for an empty range, and at least
    /// one otherwise, read alone when `buf` is smaller than a container or
    /// the block is larger than [`MAX_RUN_BLOCK`].
    /// Errors as for [`Entities::raw`], and those of `each`, which stop it.
    pub fn read_within<E: From<Error>>(
        &self,
        typ: usize,
        ids: Range<i64>,
        buf: &mut [u8],
        limit: usize,
        each: &mut impl FnMut(Option<&[u8]>) -> std::result::Result<(), E>,
    ) -> std::result::Result<u64, E> {
        let first = ids.start;
        let Ok(wanted) = u64::try_from(ids.end.saturating_sub(first)) else { return Ok(0) };
        if wanted == 0 {
            return Ok(0);
        }
        let (size, count) = self.types.get(typ).map_or((0, 0), |t| (t.0, t.1));
        let start = u64::try_from(first)
            .ok()
            .filter(|_| first < count)
            .and_then(|id| id.checked_mul(self.block_size as u64))
            .and_then(|o| o.checked_add((self.header_size + self.container_offset[typ]) as u64))
            .filter(|&o| o < self.len);
        let fit = match buf.len().checked_sub(size) {
            Some(rest) if size > 0 && self.block_size <= MAX_RUN_BLOCK => 1 + rest / self.block_size,
            _ => 0,
        };
        let Some(start) = start.filter(|_| fit > 0) else {
            let raw = self.raw_within(typ, first, limit)?;
            each(raw.as_deref())?;
            return Ok(1);
        };
        // Up to the header's count, and within the file as it was when opened.
        let stored = u64::try_from(count - first).unwrap_or(u64::MAX);
        let n = (fit as u64).min(wanted).min(stored);
        let end = (start + (n - 1) * self.block_size as u64 + size as u64).min(self.len);
        let span = &mut buf[..(end - start) as usize];
        self.file.read_into(start, span)?;
        for i in 0..n as usize {
            let at = i * self.block_size;
            let want = size.min(span.len().saturating_sub(at)).min(limit.saturating_add(4));
            let record = (want >= 4)
                .then(|| usize::try_from(le_i32(span, at)).ok().filter(|&len| len != 0 && len <= want - 4))
                .flatten()
                .map(|len| &span[at + 4..at + 4 + len]);
            each(record)?;
        }
        Ok(n)
    }

    /// The player `id`, or `None` for an unused or unreadable entry; errors
    /// as for [`Entities::raw`].
    pub fn player(&self, id: i64) -> Result<Option<Player>> {
        self.player_within(id, usize::MAX)
    }

    /// [`Entities::player`] from a record of at most `limit` bytes.
    pub fn player_within(&self, id: i64, limit: usize) -> Result<Option<Player>> {
        Ok(self.raw_within(PLAYER, id, limit)?.and_then(|r| player_of(&r)))
    }

    /// [`Entities::player_within`] for the players [`Entities::read_within`]
    /// reads at once.
    pub fn read_players_within<E: From<Error>>(
        &self,
        ids: Range<i64>,
        buf: &mut [u8],
        limit: usize,
        each: &mut impl FnMut(Option<Player>) -> std::result::Result<(), E>,
    ) -> std::result::Result<u64, E> {
        self.read_within(PLAYER, ids, buf, limit, &mut |r| each(r.and_then(player_of)))
    }

    /// The tournament `id`, or `None` for an unused or unreadable entry;
    /// errors as for [`Entities::raw`].
    pub fn tournament(&self, id: i64) -> Result<Option<Tournament>> {
        self.tournament_within(id, usize::MAX)
    }

    /// [`Entities::tournament`] from a record of at most `limit` bytes.
    pub fn tournament_within(&self, id: i64, limit: usize) -> Result<Option<Tournament>> {
        Ok(self.raw_within(TOURNAMENT, id, limit)?.and_then(|r| tournament_of(&r)))
    }

    /// [`Entities::tournament_within`] for the tournaments
    /// [`Entities::read_within`] reads at once.
    pub fn read_tournaments_within<E: From<Error>>(
        &self,
        ids: Range<i64>,
        buf: &mut [u8],
        limit: usize,
        each: &mut impl FnMut(Option<Tournament>) -> std::result::Result<(), E>,
    ) -> std::result::Result<u64, E> {
        self.read_within(TOURNAMENT, ids, buf, limit, &mut |r| each(r.and_then(tournament_of)))
    }
}

fn player_of(r: &[u8]) -> Option<Player> {
    let mut c = Cursor(r, 0);
    Some(Player { last: c.string()?, first: c.string()? })
}

fn tournament_of(r: &[u8]) -> Option<Tournament> {
    let mut c = Cursor(r, 0);
    let place = c.string()?;
    let title = c.string()?;
    let start = Date(c.i32()?);
    Some(Tournament { title, place, start })
}

impl Entities {
    /// The first non-empty title of game tag `id`: the title of the guiding
    /// text or analysis that refers to it. A tag holds one title per language;
    /// `None` when all are empty or the id is unused. Errors as for
    /// [`Entities::raw`].
    pub fn title(&self, id: i64) -> Result<Option<String>> {
        self.title_within(id, usize::MAX)
    }

    /// [`Entities::title`] from a record of at most `limit` bytes.
    pub fn title_within(&self, id: i64, limit: usize) -> Result<Option<String>> {
        Ok(self.raw_within(GAME_TAG, id, limit)?.and_then(|r| {
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
