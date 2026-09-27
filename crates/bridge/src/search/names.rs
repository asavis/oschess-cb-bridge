//! The names of a database's players, tournaments, annotators and titles (of
//! guiding texts and analyses), read once per database generation in full:
//! substring matching for search, ranks for sorting, identities for
//! suggestions. Their memory is reserved in the search budget as it grows.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use cbformat::game::{Player, Tournament};

use super::SearchError;
use super::memory::{Allowance, Cancel, Held, Hold, Refused};
use super::workers::{self, threads};
use crate::indexdir::{crc32, crc32_update, u32_at, u64_at};
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

/// The buffer a names file is written through.
const FILE_WRITE_BUFFER: usize = 1 << 16;

/// A names file being written: its buffer, and the CRC and length of the
/// body so far. Text longer than the buffer goes to the file as it is.
struct Out {
    file: File,
    buf: Vec<u8>,
    crc: u32,
    body_len: u64,
}

impl Out {
    fn put(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.crc = crc32_update(self.crc, bytes);
        self.body_len += bytes.len() as u64;
        if self.buf.len() + bytes.len() > self.buf.capacity() {
            self.flush()?;
        }
        if bytes.len() > self.buf.capacity() {
            return self.file.write_all(bytes);
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

/// A names file's body as it is read: its CRC so far, and how much of its
/// declared length is used. Every allocation here is fallible and no larger
/// than what it decodes, so a file that the budget holds never asks for
/// more than the budget reserved.
struct Body {
    /// Read without a buffer of its own: the text in one read each, the
    /// offsets 4 KiB at a time, so reading takes nothing the budget did not
    /// reserve.
    r: File,
    crc: u32,
    consumed: u64,
    len: u64,
}

impl Body {
    /// `n` more bytes, when the body has them.
    fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        self.consumed = self.consumed.checked_add(n as u64).filter(|&c| c <= self.len)?;
        let mut v = Vec::new();
        v.try_reserve_exact(n).ok()?;
        v.resize(n, 0);
        self.r.read_exact(&mut v).ok()?;
        self.crc = crc32_update(self.crc, &v);
        Some(v)
    }

    /// `n` more little-endian `u32`s, decoded straight into their list.
    fn u32s(&mut self, n: usize) -> Option<Vec<u32>> {
        let bytes = n.checked_mul(4)?;
        self.consumed = self.consumed.checked_add(bytes as u64).filter(|&c| c <= self.len)?;
        let mut out = Vec::new();
        out.try_reserve_exact(n).ok()?;
        let mut piece = [0u8; 4096];
        let mut left = bytes;
        while left > 0 {
            let k = left.min(piece.len());
            self.r.read_exact(&mut piece[..k]).ok()?;
            self.crc = crc32_update(self.crc, &piece[..k]);
            out.extend(piece[..k].as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)));
            left -= k;
        }
        Some(out)
    }
}

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
                    // The file is named after the database's id.
                    let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    crate::log!("writing a names file of database {id} failed: {e}");
                }
            });
    }

    /// Writes the table as the names file `path` of `kind` at `generation`:
    /// as `<path>.partial`, then renamed.
    pub fn write_file(&self, path: &Path, kind: Kind, generation: u64) -> std::io::Result<()> {
        let mut partial = path.as_os_str().to_owned();
        partial.push(".partial");
        let partial = PathBuf::from(partial);
        let _writing = super::heads::Writing::new(partial.clone());
        // The one buffer the write takes, reserved in the budget and
        // allocated fallibly: an optional background write that does not fit
        // is skipped, never the process.
        let refused = || std::io::Error::other("the search budget holds no names writer now");
        let _hold = Hold::reserve(FILE_WRITE_BUFFER).map_err(|_| refused())?;
        let mut out = Out { file: File::create(&partial)?, buf: Vec::new(), crc: !0, body_len: 0 };
        out.buf.try_reserve_exact(FILE_WRITE_BUFFER).map_err(|_| refused())?;
        out.file.write_all(&[0u8; FILE_HEADER])?;
        for c in &self.chunks {
            for v in [c.first as u64, c.name_ends.len() as u64, c.names.len() as u64, c.lower.len() as u64] {
                out.put(&v.to_le_bytes())?;
            }
            out.put(c.names.as_bytes())?;
            out.put(c.lower.as_bytes())?;
            for list in [&c.name_ends, &c.lower_ends, &c.given] {
                // Encoded a piece at a time, on the stack.
                for part in list.chunks(1024) {
                    let mut piece = [0u8; 4096];
                    for (i, v) in part.iter().enumerate() {
                        piece[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
                    }
                    out.put(&piece[..part.len() * 4])?;
                }
            }
        }
        out.flush()?;
        let (mut file, crc, body_len) = (out.file, out.crc, out.body_len);
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
        let header_crc = crc32(&h[..60]);
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
        let mut r = file;
        let mut h = [0u8; FILE_HEADER];
        r.read_exact(&mut h).ok()?;
        if h[0..8] != FILE_MAGIC
            || u32_at(&h, 8) != FILE_VERSION
            || crc32(&h[..60]) != u32_at(&h, 60)
            || u32_at(&h, 12) != kind_code(kind)
            || u64_at(&h, 16) != generation
            || u64_at(&h, 24) != count as u64
            || (FILE_HEADER as u64).checked_add(u64_at(&h, 48)) != Some(size)
        {
            return None;
        }
        let (per, chunks, body_crc, body_len) =
            (usize::try_from(u64_at(&h, 32)).ok()?, u32_at(&h, 40) as usize, u32_at(&h, 44), u64_at(&h, 48));
        // The layout `chunk` looks names up in: chunk `i` holds the ids from
        // `i * per`, `per` of them but for the last, and every chunk takes at
        // least its 32-byte head of the body.
        let needed = if count == 0 { 1 } else { count.div_ceil(per.max(1)) };
        if per == 0 && count > 0 || chunks < needed || (chunks as u64).checked_mul(32).is_none_or(|b| b > body_len) {
            return None;
        }
        // An optional file that cannot be held is a miss: the database's own
        // read decides whether the table fits.
        let chunk_bytes = chunks.checked_mul(std::mem::size_of::<Chunk>())?;
        // The table holds as many bytes as the file, decoded in place; the
        // chunks come on top.
        let hold = Hold::reserve(usize::try_from(size).ok()?.checked_add(chunk_bytes)?).ok()?;
        let mut body = Body { r, crc: !0, consumed: 0, len: body_len };
        let mut out: Vec<Chunk> = Vec::new();
        out.try_reserve_exact(chunks).ok()?;
        let mut seen = 0usize;
        for i in 0..chunks {
            let head = body.bytes(32)?;
            let field = |at: usize| usize::try_from(u64_at(&head, at)).ok();
            let (first, n, names_len, lower_len) = (field(0)?, field(8)?, field(16)?, field(24)?);
            if Some(first) != i.checked_mul(per.max(1)).map(|f| f.min(count))
                || first != seen
                || n != per.max(1).min(count - seen)
                || names_len as u64 > body_len
                || lower_len as u64 > body_len
            {
                return None;
            }
            let names = String::from_utf8(body.bytes(names_len)?).ok()?;
            let lower = String::from_utf8(body.bytes(lower_len)?).ok()?;
            let (name_ends, lower_ends, given) = (body.u32s(n)?, body.u32s(n)?, body.u32s(n)?);
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
        if seen != count || body.consumed != body_len || !body.crc != body_crc {
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

    /// A chunk as a names file holds it: first, count, names, lower, ends,
    /// lower ends, given.
    type RawChunk<'a> = (u64, u64, &'a str, &'a str, &'a [u32], &'a [u32], &'a [u32]);

    /// A names file of players at generation 7 as given, with correct CRCs.
    fn raw_file(
        name: &str,
        count: u64,
        per: u64,
        chunk_count: u32,
        chunks: &[RawChunk<'_>],
        body_len: Option<u64>,
    ) -> PathBuf {
        let mut body = Vec::new();
        for (first, n, names, lower, ends, lower_ends, given) in chunks {
            for v in [*first, *n, names.len() as u64, lower.len() as u64] {
                body.extend_from_slice(&v.to_le_bytes());
            }
            body.extend_from_slice(names.as_bytes());
            body.extend_from_slice(lower.as_bytes());
            for list in [ends, lower_ends, given] {
                body.extend(list.iter().flat_map(|v| v.to_le_bytes()));
            }
        }
        let mut h = [0u8; FILE_HEADER];
        h[0..8].copy_from_slice(&FILE_MAGIC);
        h[8..12].copy_from_slice(&FILE_VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&kind_code(Kind::Players).to_le_bytes());
        h[16..24].copy_from_slice(&7u64.to_le_bytes());
        h[24..32].copy_from_slice(&count.to_le_bytes());
        h[32..40].copy_from_slice(&per.to_le_bytes());
        h[40..44].copy_from_slice(&chunk_count.to_le_bytes());
        h[44..48].copy_from_slice(&crc32(&body).to_le_bytes());
        h[48..56].copy_from_slice(&body_len.unwrap_or(body.len() as u64).to_le_bytes());
        let crc = crc32(&h[..60]);
        h[60..64].copy_from_slice(&crc.to_le_bytes());
        let dir = std::env::temp_dir().join(format!("bridge-names-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("0123456789abcdef.players");
        std::fs::write(&path, [&h[..], &body].concat()).unwrap();
        path
    }

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static LIVE: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    fn grew(n: usize) {
        let _ = LIVE.try_with(|live| {
            live.set(live.get() + n);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
        });
    }

    fn shrank(n: usize) {
        let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(n)));
    }

    /// The system allocator, counting each thread's live bytes and their
    /// peak: what reading a names file takes on the thread that reads it.
    struct Counting;

    // SAFETY: every call goes to the system allocator as it came; the counts
    // beside it touch only the calling thread's cells.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(layout) };
            if !p.is_null() {
                grew(layout.size());
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
            unsafe { System.dealloc(p, layout) };
            shrank(layout.size());
        }
        unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let q = unsafe { System.realloc(p, layout, size) };
            if !q.is_null() {
                grew(size);
                shrank(layout.size());
            }
            q
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    #[test]
    fn reading_a_names_file_takes_no_more_memory_than_it_reserves() {
        // The review's file: one chunk of 1.3 million empty names, three
        // arrays of offsets, and a body CRC that fails at the very end.
        let n = 1_300_000usize;
        let zeros = vec![0u32; n];
        let path = raw_file("peak", n as u64, n as u64, 1, &[(0, n as u64, "", "", &zeros, &zeros, &zeros)], None);
        drop(zeros);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[44] ^= 1;
        let crc = crc32(&bytes[..60]);
        bytes[60..64].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let size = bytes.len();
        drop(bytes);
        let start = LIVE.with(Cell::get);
        PEAK.with(|peak| peak.set(start));
        assert!(NameTable::open_file(&path, Kind::Players, 7, n).is_none());
        let peak = PEAK.with(Cell::get) - start;
        // What `open_file` reserves, and a page for the file API's own
        // bookkeeping.
        let reserved = size + std::mem::size_of::<Chunk>();
        assert!(peak <= reserved + 4096, "{peak} bytes at the peak against {reserved} reserved");
    }

    #[test]
    fn writing_a_names_file_takes_only_its_own_buffer() {
        let n = 1_300_000;
        let names = vec![""; n];
        let big = table(&names);
        drop(names);
        let dir = std::env::temp_dir().join(format!("bridge-names-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("0123456789abcdef.players");
        let start = LIVE.with(Cell::get);
        PEAK.with(|peak| peak.set(start));
        big.write_file(&path, Kind::Players, 7).unwrap();
        let peak = PEAK.with(Cell::get) - start;
        assert!(peak <= FILE_WRITE_BUFFER + 4096, "{peak} bytes at the peak against a {FILE_WRITE_BUFFER}-byte buffer");
        let back = NameTable::open_file(&path, Kind::Players, 7, n).unwrap().unwrap();
        assert_eq!(back.len(), n);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_names_file_whose_chunks_do_not_cover_their_ids_is_never_used() {
        // Two names in one chunk, where one per chunk is declared: a lookup of
        // id 1 would find no chunk.
        let path = raw_file("layout", 2, 1, 1, &[(0, 2, "ab", "ab", &[1, 2], &[1, 2], &[0, 0])], None);
        assert!(NameTable::open_file(&path, Kind::Players, 7, 2).is_none());
        // The same names laid out as declared are read.
        let path = raw_file(
            "layout-ok",
            2,
            1,
            2,
            &[(0, 1, "a", "a", &[1], &[1], &[0]), (1, 1, "b", "b", &[1], &[1], &[0])],
            None,
        );
        let table = NameTable::open_file(&path, Kind::Players, 7, 2).unwrap().unwrap();
        assert_eq!((table.name(0), table.name(1)), ("a", "b"));
    }

    #[test]
    fn a_names_file_claiming_more_chunks_than_its_body_holds_allocates_nothing() {
        let path = raw_file("chunks", 4_000_000, 1, 4_000_000, &[], None);
        assert!(NameTable::open_file(&path, Kind::Players, 7, 4_000_000).is_none());
    }

    #[test]
    fn a_names_file_body_length_past_any_size_is_a_miss() {
        let path = raw_file("overflow", 1, 1, 1, &[], Some(u64::MAX));
        assert!(NameTable::open_file(&path, Kind::Players, 7, 1).is_none());
    }

    #[test]
    fn a_names_file_the_budget_cannot_hold_is_a_miss_not_an_error() {
        let path = raw_file("budget", 1, 1, 1, &[(0, 1, "a", "a", &[1], &[1], &[0])], None);
        // Sparse: past the budget, with a header that still names its length.
        let size = super::super::memory::budget() as u64 + 1;
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[48..56].copy_from_slice(&(size - FILE_HEADER as u64).to_le_bytes());
        let crc = crc32(&bytes[..60]);
        bytes[60..64].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(size).unwrap();
        assert!(NameTable::open_file(&path, Kind::Players, 7, 1).is_none());
    }

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
