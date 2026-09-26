//! The names of a database's players, tournaments, annotators and titles (of
//! guiding texts and analyses), read once per database generation in full:
//! substring matching for search, ranks for sorting, identities for
//! suggestions. Their memory is reserved in the search budget as it grows.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use cbformat::game::{Player, Tournament};

use super::SearchError;
use super::memory::{Allowance, Cancel, Held, Hold, Refused};
use super::workers::{self, threads};
pub use crate::store::Kind;
use crate::store::Store;

/// Ids of a type read in one worker's go.
const IDS_PER_WORKER_MIN: usize = 4096;
/// The buffer a name worker reads many entity records into at once: a
/// mebibyte, about 250 players of a Mega Database, and one read instead of
/// 250 (#83).
const NAME_READ: usize = 1 << 20;
/// Each name worker's workspace, reserved in the budget before the worker
/// starts: its read buffer, and one record decoded and lowercased.
const NAME_WORKSPACE: usize = NAME_READ + (64 << 10);

/// The names of consecutive ids, one after another in two strings: as shown
/// ("Last, First" for players) and in lower case.
struct Chunk {
    first: usize,
    names: String,
    name_ends: Vec<u32>,
    lower: String,
    lower_ends: Vec<u32>,
    /// Where a person's first name starts in the lower-case name, from the
    /// entity's own first-name field; 0 when there is none.
    given: Vec<u32>,
}

fn part<'a>(text: &'a str, ends: &[u32], i: usize) -> &'a str {
    let start = if i == 0 { 0 } else { ends[i - 1] as usize };
    &text[start..ends[i] as usize]
}

/// Appends `add` to `s`, reserving any capacity it grows by in the budget first.
fn push_str(s: &mut String, add: &str, allow: &mut Allowance<'_>) -> Result<(), Refused> {
    let needed = s.len() + add.len();
    if needed > s.capacity() {
        let capacity = needed.max(s.capacity() * 2).max(256);
        allow.take(capacity - s.capacity())?;
        s.try_reserve_exact(capacity - s.len()).map_err(|_| Refused::Busy)?;
    }
    s.push_str(add);
    Ok(())
}

pub struct NameTable {
    len: usize,
    /// Where the table is to be written once it is shared, and for which
    /// generation: set for a table read from the database while a heads file
    /// is set (#108).
    write_to: Mutex<Option<(PathBuf, Kind, u64)>>,
    /// Ids per chunk; the last chunk may hold fewer.
    per: usize,
    chunks: Vec<Chunk>,
    /// For titles found by record ([`Store::TITLES_BY_RECORD`]): the record
    /// numbers in order, each title's key; a title's id is its position.
    keys: Option<Held<Vec<u32>>>,
    _hold: Hold,
}

/// A name as shown, and a person's first name in lower case.
type Named = (String, String);

/// What takes each name as it is read.
type Each<'a> = dyn FnMut(Named) -> Result<(), SearchError> + 'a;

/// What reads the names from a range's start on into a buffer, handing each
/// over, and says how many it read.
type Names<'a> = dyn Fn(Range<usize>, &mut [u8], &mut Each<'_>) -> Result<usize, SearchError> + Sync + 'a;

impl NameTable {
    /// The names of `kind`, by entity id; for titles found by record, of the
    /// records `keys` numbers, by position.
    pub fn load<S: Store>(
        db: &S,
        kind: Kind,
        keys: Option<Held<Vec<u32>>>,
        cancel: &Cancel,
    ) -> Result<NameTable, SearchError> {
        let count = match &keys {
            Some(keys) => keys.len(),
            None => usize::try_from(db.name_count(kind)).unwrap_or(usize::MAX),
        };
        let player = |p: Option<Player>| match p {
            Some(p) => (p.pgn(), p.first.to_lowercase()),
            None => (String::new(), String::new()),
        };
        let tournament = |t: Option<Tournament>| (t.map(|t| t.title).unwrap_or_default(), String::new());
        let name = |id: usize| -> cbformat::Result<Named> {
            let id = match &keys {
                Some(keys) => i64::from(keys[id]),
                None => id as i64,
            };
            Ok(match kind {
                Kind::Players => player(db.player(id)?),
                Kind::Tournaments => tournament(db.tournament(id)?),
                // An annotator of its own is one text; written as "Last,
                // First", what follows the comma is its first name.
                Kind::Annotators => {
                    let name = db.annotator(id)?.unwrap_or_default();
                    let first = name.split_once(", ").map(|(_, first)| first.to_lowercase()).unwrap_or_default();
                    (name, first)
                }
                Kind::Titles => (db.title(id)?.unwrap_or_default(), String::new()),
            })
        };
        // Players and tournaments by id are read many at a time; the rest one
        // by one.
        let names = |ids: Range<usize>, buf: &mut [u8], each: &mut Each<'_>| -> Result<usize, SearchError> {
            let range = ids.start as i64..ids.end as i64;
            let read = match (&keys, kind) {
                (None, Kind::Players) => db.read_players(range, buf, &mut |p| each(player(p)))?,
                (None, Kind::Tournaments) => db.read_tournaments(range, buf, &mut |t| each(tournament(t)))?,
                _ => {
                    each(name(ids.start)?)?;
                    1
                }
            };
            Ok(read as usize)
        };
        let (per, chunks, hold) = NameTable::read(count, &names, cancel)?;
        Ok(NameTable { len: count, write_to: Mutex::default(), per, chunks, keys, _hold: hold })
    }

    /// Reads the names of ids `0..count` on the workers, each worker a range
    /// of them in order. `names` hands `each` the names from its range's start
    /// on, as many as it reads at once into the buffer, and says how many.
    fn read(count: usize, names: &Names<'_>, cancel: &Cancel) -> Result<(usize, Vec<Chunk>, Hold), SearchError> {
        if count > u32::MAX as usize {
            return Err(Refused::TooLarge.into());
        }
        // Two string ends and a first-name start per id, reserved before
        // anything is read.
        let shared = Mutex::new(Hold::reserve(count.checked_mul(12).ok_or(Refused::TooLarge)?)?);
        let want = threads().min(count.div_ceil(IDS_PER_WORKER_MIN)).max(1);
        let chunks = workers::run(want, NAME_WORKSPACE, cancel, |w| {
            let per = count.div_ceil(w.count).max(1);
            let (first, end) = ((w.index * per).min(count), ((w.index + 1) * per).min(count));
            let mut allow = Allowance::new(&shared);
            let n = end - first;
            let mut c = Chunk {
                first,
                names: String::new(),
                name_ends: Vec::new(),
                lower: String::new(),
                lower_ends: Vec::new(),
                given: Vec::new(),
            };
            c.name_ends.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
            c.lower_ends.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
            c.given.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
            let mut buf = Vec::new();
            buf.try_reserve_exact(NAME_READ).map_err(|_| Refused::Busy)?;
            buf.resize(NAME_READ, 0);
            let mut id = first;
            while id < end {
                if w.stopped() || cancel.is_cancelled() {
                    return Err(SearchError::Superseded);
                }
                let read = names(id..end, &mut buf, &mut |(name, first_name)| {
                    let lower = name.to_lowercase();
                    // The first name ends the name ("Last, First"); a comma
                    // inside the last name does not move it.
                    let given = match lower.strip_suffix(first_name.as_str()) {
                        Some(before) if !first_name.is_empty() => before.len(),
                        _ => 0,
                    };
                    push_str(&mut c.names, &name, &mut allow)?;
                    push_str(&mut c.lower, &lower, &mut allow)?;
                    c.name_ends.push(u32::try_from(c.names.len()).map_err(|_| Refused::TooLarge)?);
                    c.lower_ends.push(u32::try_from(c.lower.len()).map_err(|_| Refused::TooLarge)?);
                    c.given.push(u32::try_from(given).map_err(|_| Refused::TooLarge)?);
                    Ok(())
                })?;
                debug_assert!(read > 0, "a read of names that are left hands at least one over");
                id += read.max(1);
            }
            debug_assert_eq!(c.name_ends.len(), n);
            Ok(c)
        })?;
        let per = count.div_ceil(chunks.len()).max(1);
        let hold = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        Ok((per, chunks, hold))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The id of the name whose key is `key`: the key itself, or for titles
    /// found by record the record's position among the keys; -1 for none.
    pub fn slot(&self, key: i64) -> i64 {
        match &self.keys {
            None => key,
            Some(keys) => u32::try_from(key).ok().and_then(|k| keys.binary_search(&k).ok()).map_or(-1, |i| i as i64),
        }
    }

    fn chunk(&self, id: usize) -> (&Chunk, usize) {
        let c = &self.chunks[id / self.per];
        (c, id - c.first)
    }

    /// The name of `id`, empty when there is none.
    pub fn name(&self, id: i64) -> &str {
        match usize::try_from(id).ok().filter(|&i| i < self.len) {
            Some(i) => {
                let (c, i) = self.chunk(i);
                part(&c.names, &c.name_ends, i)
            }
            None => "",
        }
    }

    pub fn lower(&self, id: usize) -> &str {
        let (c, i) = self.chunk(id);
        part(&c.lower, &c.lower_ends, i)
    }

    /// A person's first name in lower case, from the entity's first-name
    /// field; `None` for a name without one.
    pub fn given_lower(&self, id: usize) -> Option<&str> {
        let (c, i) = self.chunk(id);
        let at = c.given[i] as usize;
        (at != 0).then(|| &part(&c.lower, &c.lower_ends, i)[at..])
    }

    /// The ids whose name contains `needle`, which is in lower case.
    pub fn containing(&self, needle: &str, allow: &mut Allowance<'_>) -> Result<BitSet, Refused> {
        let mut set = BitSet::new(self.len, allow)?;
        for c in &self.chunks {
            for i in 0..c.lower_ends.len() {
                if part(&c.lower, &c.lower_ends, i).contains(needle) {
                    set.insert(c.first + i);
                }
            }
        }
        Ok(set)
    }

    /// The ids in order of their name as shown, equal names in id order; ids
    /// with an empty name left out. Sorted on the workers, with a second list
    /// while they sort.
    fn by_exact_name(&self, allow: &mut Allowance<'_>) -> Result<Vec<u32>, SearchError> {
        allow.take(self.len * 8)?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(self.len).map_err(|_| Refused::Busy)?;
        ids.extend((0..self.len as u32).filter(|&i| !self.name(i64::from(i)).is_empty()));
        workers::sort_by(&mut ids, &|&a: &u32, &b: &u32| {
            self.name(i64::from(a)).cmp(self.name(i64::from(b))).then(a.cmp(&b))
        })?;
        Ok(ids)
    }
}

const FILE_MAGIC: [u8; 8] = *b"OSCBNAM\0";
const FILE_VERSION: u32 = 1;
const FILE_HEADER: usize = 64;

/// Name tables read from their files, for tests.
pub static FILES_READ: AtomicU64 = AtomicU64::new(0);

/// The names file of `kind` beside the heads file `heads` (#108).
pub fn file_path(heads: &Path, kind: Kind) -> Option<PathBuf> {
    let ext = match kind {
        Kind::Players => "players",
        Kind::Tournaments => "tournaments",
        Kind::Annotators => "annotators",
        Kind::Titles => return None,
    };
    Some(heads.with_extension(ext))
}

fn kind_code(kind: Kind) -> u32 {
    match kind {
        Kind::Players => 1,
        Kind::Tournaments => 2,
        Kind::Annotators => 3,
        Kind::Titles => 4,
    }
}

impl NameTable {
    /// Marks the table to be written as the names file `path` of `kind` at
    /// `generation`, by [`NameTable::write_later`].
    pub(super) fn to_be_written(&self, path: PathBuf, kind: Kind, generation: u64) {
        *self.write_to.lock().unwrap_or_else(|e| e.into_inner()) = Some((path, kind, generation));
    }

    /// Writes the table on a thread of its own when it is marked to be, once.
    pub(super) fn write_later(self: &std::sync::Arc<Self>) {
        let Some((path, kind, generation)) = self.write_to.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let table = std::sync::Arc::clone(self);
        let _ =
            std::thread::Builder::new().name("bridge-names".into()).stack_size(crate::THREAD_STACK).spawn(move || {
                if let Err(e) = table.write_file(&path, kind, generation) {
                    eprintln!("oschess-bridge: writing a names file failed: {e}");
                }
            });
    }

    /// Writes the table as the names file `path` of `kind` at `generation`:
    /// as `<path>.partial`, then renamed.
    pub fn write_file(&self, path: &Path, kind: Kind, generation: u64) -> std::io::Result<()> {
        let mut partial = path.as_os_str().to_owned();
        partial.push(".partial");
        let partial = PathBuf::from(partial);
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&partial)?);
        out.write_all(&[0u8; FILE_HEADER])?;
        let (mut crc, mut body_len) = (!0u32, 0u64);
        let mut put = |out: &mut BufWriter<File>, bytes: &[u8]| -> std::io::Result<()> {
            crc = super::heads::crc32_update(crc, bytes);
            body_len += bytes.len() as u64;
            out.write_all(bytes)
        };
        for c in &self.chunks {
            for v in [c.first as u64, c.name_ends.len() as u64, c.names.len() as u64, c.lower.len() as u64] {
                put(&mut out, &v.to_le_bytes())?;
            }
            put(&mut out, c.names.as_bytes())?;
            put(&mut out, c.lower.as_bytes())?;
            for list in [&c.name_ends, &c.lower_ends, &c.given] {
                let bytes: Vec<u8> = list.iter().flat_map(|v| v.to_le_bytes()).collect();
                put(&mut out, &bytes)?;
            }
        }
        let mut file = out.into_inner().map_err(|e| e.into_error())?;
        let mut h = [0u8; FILE_HEADER];
        h[0..8].copy_from_slice(&FILE_MAGIC);
        h[8..12].copy_from_slice(&FILE_VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&kind_code(kind).to_le_bytes());
        h[16..24].copy_from_slice(&generation.to_le_bytes());
        h[24..32].copy_from_slice(&(self.len as u64).to_le_bytes());
        h[32..40].copy_from_slice(&(self.per as u64).to_le_bytes());
        h[40..44].copy_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        h[44..48].copy_from_slice(&(!crc).to_le_bytes());
        h[48..56].copy_from_slice(&body_len.to_le_bytes());
        let header_crc = super::heads::crc32(&h[..60]);
        h[60..64].copy_from_slice(&header_crc.to_le_bytes());
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&h)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&partial, path)
    }

    /// The table of `kind` from the names file `path`, when it was written
    /// at `generation` for `count` names and reads back whole; `None`
    /// otherwise, and the table is read from the database instead. Its
    /// memory is reserved in the search budget first.
    pub fn open_file(path: &Path, kind: Kind, generation: u64, count: usize) -> Option<Result<NameTable, SearchError>> {
        let file = File::open(path).ok()?;
        let size = file.metadata().ok()?.len();
        let mut r = BufReader::with_capacity(1 << 20, file);
        let mut h = [0u8; FILE_HEADER];
        r.read_exact(&mut h).ok()?;
        let (u32_at, u64_at) = (
            |at: usize| u32::from_le_bytes(h[at..at + 4].try_into().unwrap()),
            |at: usize| u64::from_le_bytes(h[at..at + 8].try_into().unwrap()),
        );
        if h[0..8] != FILE_MAGIC
            || u32_at(8) != FILE_VERSION
            || super::heads::crc32(&h[..60]) != u32_at(60)
            || u32_at(12) != kind_code(kind)
            || u64_at(16) != generation
            || u64_at(24) != count as u64
            || size != FILE_HEADER as u64 + u64_at(48)
        {
            return None;
        }
        let (per, chunks, body_crc) = (usize::try_from(u64_at(32)).ok()?, u32_at(40) as usize, u32_at(44));
        if per == 0 && count > 0 || chunks > count.max(1) {
            return None;
        }
        let hold = match Hold::reserve(size as usize) {
            Ok(h) => h,
            Err(e) => return Some(Err(e.into())),
        };
        let (mut crc, mut consumed) = (!0u32, 0u64);
        let body_len = u64_at(48);
        let mut take = |r: &mut BufReader<File>, n: usize| -> Option<Vec<u8>> {
            consumed = consumed.checked_add(n as u64).filter(|&c| c <= body_len)?;
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            v.resize(n, 0);
            r.read_exact(&mut v).ok()?;
            crc = super::heads::crc32_update(crc, &v);
            Some(v)
        };
        let mut out = Vec::with_capacity(chunks);
        let mut seen = 0usize;
        for i in 0..chunks {
            let head = take(&mut r, 32)?;
            let field = |at: usize| usize::try_from(u64::from_le_bytes(head[at..at + 8].try_into().unwrap())).ok();
            let (first, n, names_len, lower_len) = (field(0)?, field(8)?, field(16)?, field(24)?);
            if first != i * per
                || first != seen
                || n > count - seen
                || names_len > size as usize
                || lower_len > size as usize
            {
                return None;
            }
            let names = String::from_utf8(take(&mut r, names_len)?).ok()?;
            let lower = String::from_utf8(take(&mut r, lower_len)?).ok()?;
            let mut list = || -> Option<Vec<u32>> {
                let bytes = take(&mut r, n.checked_mul(4)?)?;
                Some(bytes.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect())
            };
            let (name_ends, lower_ends, given) = (list()?, list()?, list()?);
            // Every part a lookup slices must lie on the text's own bounds.
            let bounded = |text: &str, ends: &[u32]| {
                ends.windows(2).all(|w| w[0] <= w[1])
                    && ends.last().is_none_or(|&e| e as usize == text.len())
                    && ends.iter().all(|&e| text.is_char_boundary(e as usize))
            };
            if !bounded(&names, &name_ends) || !bounded(&lower, &lower_ends) {
                return None;
            }
            let starts = std::iter::once(0).chain(lower_ends.iter().copied());
            if !starts
                .zip(&lower_ends)
                .zip(&given)
                .all(|((start, &end), &g)| g == 0 || (g <= end - start && lower.is_char_boundary((start + g) as usize)))
            {
                return None;
            }
            seen += n;
            out.push(Chunk { first, names, name_ends, lower, lower_ends, given });
        }
        if seen != count || consumed != body_len || !crc != body_crc {
            return None;
        }
        FILES_READ.fetch_add(1, Ordering::Relaxed);
        Some(Ok(NameTable {
            len: count,
            write_to: Mutex::default(),
            per: per.max(1),
            chunks: out,
            keys: None,
            _hold: hold,
        }))
    }
}

/// Positions in one case-insensitive name order over several tables together,
/// per table and id: a tournament and a guiding text's title sort among each
/// other. Names equal but for case share a position, so that sorting falls back
/// to the record number, and every empty name has position 0, which is also the
/// key of a missing name.
pub fn joint_ranks(tables: &[&NameTable]) -> Result<Held<Vec<Vec<u32>>>, SearchError> {
    let total: usize = tables.iter().map(|t| t.len()).sum();
    // The entries twice while the workers sort them, then the entries and the
    // ranks kept.
    let mut hold = Hold::reserve(total.checked_mul(16).ok_or(Refused::TooLarge)?)?;
    let mut all: Vec<u64> = Vec::new();
    all.try_reserve_exact(total).map_err(|_| Refused::Busy)?;
    for (t, table) in tables.iter().enumerate() {
        all.extend((0..table.len()).filter(|&id| !table.lower(id).is_empty()).map(|id| ((t as u64) << 48) | id as u64));
    }
    let name = |e: u64| tables[(e >> 48) as usize].lower((e & ((1 << 48) - 1)) as usize);
    workers::sort_by(&mut all, &|&a: &u64, &b: &u64| name(a).cmp(name(b)).then(a.cmp(&b)))?;
    let mut ranks: Vec<Vec<u32>> = Vec::new();
    for t in tables {
        let mut r = Vec::new();
        r.try_reserve_exact(t.len()).map_err(|_| Refused::Busy)?;
        r.resize(t.len(), 0);
        ranks.push(r);
    }
    let mut rank = 0u32;
    for (i, &e) in all.iter().enumerate() {
        if i == 0 || name(all[i - 1]) != name(e) {
            rank += 1;
        }
        ranks[(e >> 48) as usize][(e & ((1 << 48) - 1)) as usize] = rank;
    }
    drop(all);
    hold.shrink(total * 4);
    Ok(Held::new(ranks, hold))
}

/// Names as identities: each id's group, ids of one exact name sharing it, and
/// one id per group. Ids with an empty name have the group [`NO_GROUP`].
pub struct Groups {
    pub of_id: Vec<u32>,
    pub first_id: Vec<u32>,
}

pub const NO_GROUP: u32 = u32::MAX;

pub fn groups(table: &NameTable) -> Result<Held<Groups>, SearchError> {
    let shared = Mutex::new(Hold::reserve(table.len().checked_mul(8).ok_or(Refused::TooLarge)?)?);
    let (of_id, first_id) = {
        let mut allow = Allowance::new(&shared);
        let ids = table.by_exact_name(&mut allow)?;
        let mut of_id = Vec::new();
        of_id.try_reserve_exact(table.len()).map_err(|_| Refused::Busy)?;
        of_id.resize(table.len(), NO_GROUP);
        let mut first_id = Vec::new();
        first_id.try_reserve_exact(ids.len()).map_err(|_| Refused::Busy)?;
        for (i, &id) in ids.iter().enumerate() {
            if i == 0 || table.name(i64::from(ids[i - 1])) != table.name(i64::from(id)) {
                first_id.push(id);
            }
            of_id[id as usize] = (first_id.len() - 1) as u32;
        }
        (of_id, first_id)
    };
    let mut hold = shared.into_inner().unwrap_or_else(|e| e.into_inner());
    hold.shrink((of_id.capacity() + first_id.capacity()) * 4);
    Ok(Held::new(Groups { of_id, first_id }, hold))
}

/// A set of small non-negative integers, its memory taken from an allowance.
pub struct BitSet {
    bits: Vec<u64>,
}

impl BitSet {
    pub fn new(len: usize, allow: &mut Allowance<'_>) -> Result<BitSet, Refused> {
        let words = len.div_ceil(64);
        allow.take(words * 8)?;
        let mut bits = Vec::new();
        bits.try_reserve_exact(words).map_err(|_| Refused::Busy)?;
        bits.resize(words, 0);
        Ok(BitSet { bits })
    }

    pub fn insert(&mut self, i: usize) {
        self.bits[i / 64] |= 1 << (i % 64);
    }

    pub fn contains(&self, i: usize) -> bool {
        self.bits.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    /// Whether the entity id `id` is in the set; negative ids never are.
    pub fn contains_id(&self, id: i64) -> bool {
        usize::try_from(id).is_ok_and(|i| self.contains(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(names: &[&str]) -> NameTable {
        let mut c = Chunk {
            first: 0,
            names: String::new(),
            name_ends: vec![],
            lower: String::new(),
            lower_ends: vec![],
            given: vec![],
        };
        for n in names {
            c.names.push_str(n);
            c.name_ends.push(c.names.len() as u32);
            c.lower.push_str(&n.to_lowercase());
            c.lower_ends.push(c.lower.len() as u32);
            c.given.push(0);
        }
        NameTable {
            len: names.len(),
            write_to: Mutex::default(),
            per: names.len().max(1),
            chunks: vec![c],
            keys: None,
            _hold: Hold::default(),
        }
    }

    #[test]
    fn bit_sets() {
        let shared = Mutex::new(Hold::default());
        let mut allow = Allowance::new(&shared);
        let mut s = BitSet::new(130, &mut allow).unwrap();
        s.insert(0);
        s.insert(129);
        assert!(s.contains(0) && s.contains(129) && !s.contains(64));
        assert!(!s.contains(10_000) && !s.contains_id(-1));
    }

    #[test]
    fn ranks_groups_and_matching() {
        let t = table(&["", "b", "A", "a", "a"]);
        assert_eq!(*joint_ranks(&[&t]).ok().unwrap(), [vec![0, 2, 1, 1, 1]], "empty is 0, case is ignored");
        let u = table(&["", "B", "a"]);
        assert_eq!(*joint_ranks(&[&t, &u]).ok().unwrap(), [vec![0, 2, 1, 1, 1], vec![0, 2, 1]]);
        let g = groups(&t).ok().unwrap();
        assert_eq!(g.of_id, [NO_GROUP, 2, 0, 1, 1]);
        assert_eq!(g.first_id, [2, 3, 1]);
        let shared = Mutex::new(Hold::default());
        let s = t.containing("a", &mut Allowance::new(&shared)).unwrap();
        assert!(!s.contains(0) && !s.contains(1) && s.contains(2) && s.contains(3));
        assert_eq!((t.name(3), t.name(-1), t.name(9)), ("a", "", ""));
    }
}
