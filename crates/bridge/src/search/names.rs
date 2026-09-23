//! The names of a database's players, tournaments and titles (the game tags
//! of guiding texts and analyses), read once per database generation in full:
//! substring matching for search, ranks for sorting, identities for
//! suggestions. Their memory is reserved in the search budget as it grows.

use std::sync::Mutex;

use cbformat::v2::{Database, GAME_TAG, PLAYER, TOURNAMENT};

use super::SearchError;
use super::memory::{Allowance, Cancel, Held, Hold, Refused};
use super::scan::threads;

/// Ids of a type read in one worker's go.
const IDS_PER_WORKER_MIN: usize = 4096;
/// Ids read between two cancellation checks.
const IDS_PER_CHECK: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Players,
    Tournaments,
    /// Titles of guiding texts and analyses.
    Titles,
}

/// The names of consecutive ids, one after another in two strings: as shown
/// ("Last, First" for players) and in lower case.
struct Chunk {
    first: usize,
    names: String,
    name_ends: Vec<u32>,
    lower: String,
    lower_ends: Vec<u32>,
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
    _hold: Hold,
}

impl NameTable {
    pub fn load(db: &Database, kind: Kind, cancel: &Cancel) -> Result<NameTable, SearchError> {
        let e = db.entities();
        let typ = match kind {
            Kind::Players => PLAYER,
            Kind::Tournaments => TOURNAMENT,
            Kind::Titles => GAME_TAG,
        };
        let count = usize::try_from(e.stored_count(typ)).unwrap_or(usize::MAX);
        if count > u32::MAX as usize {
            return Err(Refused::TooLarge.into());
        }
        // Two string ends per id, reserved before anything is read.
        let shared = Mutex::new(Hold::reserve(count.checked_mul(8).ok_or(Refused::TooLarge)?)?);
        let workers = threads().min(count.div_ceil(IDS_PER_WORKER_MIN)).max(1);
        let per = count.div_ceil(workers).max(1);
        let chunks: Vec<Result<Chunk, SearchError>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|w| {
                    let (first, end) = ((w * per).min(count), ((w + 1) * per).min(count));
                    let shared = &shared;
                    s.spawn(move || -> Result<Chunk, SearchError> {
                        let mut allow = Allowance::new(shared);
                        let n = end - first;
                        let mut c = Chunk {
                            first,
                            names: String::new(),
                            name_ends: Vec::new(),
                            lower: String::new(),
                            lower_ends: Vec::new(),
                        };
                        c.name_ends.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
                        c.lower_ends.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
                        for id in first..end {
                            if (id - first) % IDS_PER_CHECK == 0 && cancel.is_cancelled() {
                                return Err(SearchError::Superseded);
                            }
                            let name = match kind {
                                Kind::Players => e.player(id as i64)?.map(|p| p.pgn()),
                                Kind::Tournaments => e.tournament(id as i64)?.map(|t| t.title),
                                Kind::Titles => e.title(id as i64)?,
                            }
                            .unwrap_or_default();
                            push_str(&mut c.names, &name, &mut allow)?;
                            push_str(&mut c.lower, &name.to_lowercase(), &mut allow)?;
                            c.name_ends.push(u32::try_from(c.names.len()).map_err(|_| Refused::TooLarge)?);
                            c.lower_ends.push(u32::try_from(c.lower.len()).map_err(|_| Refused::TooLarge)?);
                        }
                        Ok(c)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
        });
        let chunks = chunks.into_iter().collect::<Result<Vec<_>, _>>()?;
        let hold = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        Ok(NameTable { len: count, per, chunks, _hold: hold })
    }

    pub fn len(&self) -> usize {
        self.len
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

/// Positions in one name order — case-insensitive, then exact — over several
/// tables together, per table and id: a tournament and a guiding text's title
/// sort among each other. Equal names share a position, and every empty name
/// has position 0, which is also the key of a missing name.
pub fn joint_ranks(tables: &[&NameTable]) -> Result<Held<Vec<Vec<u32>>>, SearchError> {
    let total: usize = tables.iter().map(|t| t.len()).sum();
    // The ranks kept, and the sorted entries while they are built.
    let mut hold = Hold::reserve(total.checked_mul(12).ok_or(Refused::TooLarge)?)?;
    let mut all: Vec<u64> = Vec::new();
    all.try_reserve_exact(total).map_err(|_| Refused::Busy)?;
    for (t, table) in tables.iter().enumerate() {
        all.extend((0..table.len()).filter(|&id| !table.lower(id).is_empty()).map(|id| ((t as u64) << 48) | id as u64));
    }
    let name = |e: u64| {
        let (t, id) = ((e >> 48) as usize, (e & ((1 << 48) - 1)) as usize);
        (tables[t].lower(id), tables[t].name(id as i64))
    };
    all.sort_unstable_by(|&a, &b| name(a).cmp(&name(b)));
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
        let mut c =
            Chunk { first: 0, names: String::new(), name_ends: vec![], lower: String::new(), lower_ends: vec![] };
        for n in names {
            c.names.push_str(n);
            c.name_ends.push(c.names.len() as u32);
            c.lower.push_str(&n.to_lowercase());
            c.lower_ends.push(c.lower.len() as u32);
        }
        NameTable { len: names.len(), per: names.len().max(1), chunks: vec![c], _hold: Hold::default() }
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
        assert_eq!(*joint_ranks(&[&t]).ok().unwrap(), [vec![0, 3, 1, 2, 2]], "empty is 0, equal names share");
        let u = table(&["", "B", "a"]);
        assert_eq!(*joint_ranks(&[&t, &u]).ok().unwrap(), [vec![0, 4, 1, 2, 2], vec![0, 3, 2]]);
        let g = groups(&t).ok().unwrap();
        assert_eq!(g.of_id, [NO_GROUP, 2, 0, 1, 1]);
        assert_eq!(g.first_id, [2, 3, 1]);
        let shared = Mutex::new(Hold::default());
        let s = t.containing("a", &mut Allowance::new(&shared)).unwrap();
        assert!(!s.contains(0) && !s.contains(1) && s.contains(2) && s.contains(3));
        assert_eq!((t.name(3), t.name(-1), t.name(9)), ("a", "", ""));
    }
}
