//! The names of a database's players, tournaments, annotators and titles (of
//! guiding texts and analyses), read once per database generation in full:
//! substring matching for search, ranks for sorting, identities for
//! suggestions. Their memory is reserved in the search budget as it grows.

use std::sync::Mutex;

use super::SearchError;
use super::memory::{Allowance, Cancel, Held, Hold, Refused};
use super::workers::{self, threads};
pub use crate::store::Kind;
use crate::store::Store;

/// Ids of a type read in one worker's go.
const IDS_PER_WORKER_MIN: usize = 4096;
/// Ids read between two cancellation checks.
const IDS_PER_CHECK: usize = 1024;
/// Each name worker's workspace for one record, decoded and lowercased,
/// reserved in the budget before the worker starts.
const NAME_WORKSPACE: usize = 64 << 10;

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
        let name = |id: usize| -> cbformat::Result<Named> {
            let id = match &keys {
                Some(keys) => i64::from(keys[id]),
                None => id as i64,
            };
            Ok(match kind {
                Kind::Players => match db.player(id)? {
                    Some(p) => (p.pgn(), p.first.to_lowercase()),
                    None => (String::new(), String::new()),
                },
                Kind::Tournaments => (db.tournament(id)?.map(|t| t.title).unwrap_or_default(), String::new()),
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
        let (per, chunks, hold) = NameTable::read(count, &name, cancel)?;
        Ok(NameTable { len: count, per, chunks, keys, _hold: hold })
    }

    /// Reads the names of ids `0..count` on the workers.
    fn read(
        count: usize,
        name: &(dyn Fn(usize) -> cbformat::Result<Named> + Sync),
        cancel: &Cancel,
    ) -> Result<(usize, Vec<Chunk>, Hold), SearchError> {
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
            for id in first..end {
                if (id - first) % IDS_PER_CHECK == 0 && (w.stopped() || cancel.is_cancelled()) {
                    return Err(SearchError::Superseded);
                }
                let (name, first_name) = name(id)?;
                let lower = name.to_lowercase();
                // The first name ends the name ("Last, First"); a comma inside
                // the last name does not move it.
                let given = match lower.strip_suffix(first_name.as_str()) {
                    Some(before) if !first_name.is_empty() => before.len(),
                    _ => 0,
                };
                push_str(&mut c.names, &name, &mut allow)?;
                push_str(&mut c.lower, &lower, &mut allow)?;
                c.name_ends.push(u32::try_from(c.names.len()).map_err(|_| Refused::TooLarge)?);
                c.lower_ends.push(u32::try_from(c.lower.len()).map_err(|_| Refused::TooLarge)?);
                c.given.push(u32::try_from(given).map_err(|_| Refused::TooLarge)?);
            }
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

    /// The ids in order of their name as shown; ids with an empty name left out.
    fn by_exact_name(&self, allow: &mut Allowance<'_>) -> Result<Vec<u32>, Refused> {
        allow.take(self.len * 4)?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(self.len).map_err(|_| Refused::Busy)?;
        ids.extend((0..self.len as u32).filter(|&i| !self.name(i64::from(i)).is_empty()));
        ids.sort_unstable_by(|&a, &b| self.name(i64::from(a)).cmp(self.name(i64::from(b))));
        Ok(ids)
    }
}

/// Positions in one case-insensitive name order over several tables together,
/// per table and id: a tournament and a guiding text's title sort among each
/// other. Names equal but for case share a position, so that sorting falls back
/// to the record number, and every empty name has position 0, which is also the
/// key of a missing name.
pub fn joint_ranks(tables: &[&NameTable]) -> Result<Held<Vec<Vec<u32>>>, SearchError> {
    let total: usize = tables.iter().map(|t| t.len()).sum();
    // The ranks kept, and the sorted entries while they are built.
    let mut hold = Hold::reserve(total.checked_mul(12).ok_or(Refused::TooLarge)?)?;
    let mut all: Vec<u64> = Vec::new();
    all.try_reserve_exact(total).map_err(|_| Refused::Busy)?;
    for (t, table) in tables.iter().enumerate() {
        all.extend((0..table.len()).filter(|&id| !table.lower(id).is_empty()).map(|id| ((t as u64) << 48) | id as u64));
    }
    let name = |e: u64| tables[(e >> 48) as usize].lower((e & ((1 << 48) - 1)) as usize);
    all.sort_unstable_by(|&a, &b| name(a).cmp(name(b)));
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
        NameTable { len: names.len(), per: names.len().max(1), chunks: vec![c], keys: None, _hold: Hold::default() }
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
