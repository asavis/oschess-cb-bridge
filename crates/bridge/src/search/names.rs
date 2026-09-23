//! The names of a database's players, tournaments and titles (the game tags
//! of guiding texts and analyses), read once per database generation:
//! substring matching for search, ranks for sorting, prefixes for suggestions.

use cbformat::v2::{Database, GAME_TAG, PLAYER, TOURNAMENT};

use super::scan::threads;

/// Ids of a type read in one worker's go.
const IDS_PER_WORKER_MIN: usize = 4096;
/// Ids read per type: a Mega Database has under 500,000 players. Ids past this
/// have no name here, which bounds memory for a hostile `.2lid`.
pub const MAX_IDS: usize = 2_000_000;
/// Bytes of a name that are kept.
pub const MAX_NAME_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Players,
    Tournaments,
    /// Titles of guiding texts and analyses.
    Titles,
}

pub struct NameTable {
    /// The name shown for each id: "Last, First" for players, the title for
    /// tournaments; empty for an unused id.
    names: Vec<Box<str>>,
    lower: Vec<Box<str>>,
}

impl NameTable {
    pub fn load(db: &Database, kind: Kind) -> cbformat::Result<NameTable> {
        let e = db.entities();
        let typ = match kind {
            Kind::Players => PLAYER,
            Kind::Tournaments => TOURNAMENT,
            Kind::Titles => GAME_TAG,
        };
        let count = usize::try_from(e.stored_count(typ)).unwrap_or(0).min(MAX_IDS);
        let workers = threads().min(count.div_ceil(IDS_PER_WORKER_MIN)).max(1);
        let per = count.div_ceil(workers);
        let parts: Vec<cbformat::Result<Vec<Box<str>>>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|w| {
                    let ids = w * per..((w + 1) * per).min(count);
                    s.spawn(move || {
                        ids.map(|id| {
                            let id = id as i64;
                            let name = match kind {
                                Kind::Players => e.player(id)?.map(|p| p.pgn()),
                                Kind::Tournaments => e.tournament(id)?.map(|t| t.title),
                                Kind::Titles => e.title(id)?,
                            };
                            Ok(shorten(name.unwrap_or_default()).into_boxed_str())
                        })
                        .collect()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
        });
        let mut names = Vec::with_capacity(count);
        for part in parts {
            names.extend(part?);
        }
        let lower = names.iter().map(|n| n.to_lowercase().into_boxed_str()).collect();
        Ok(NameTable { names, lower })
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// The name of `id`, empty when there is none.
    pub fn name(&self, id: i64) -> &str {
        usize::try_from(id).ok().and_then(|i| self.names.get(i)).map_or("", |n| n)
    }

    pub fn lower(&self, id: usize) -> &str {
        &self.lower[id]
    }

    /// The ids whose name contains `needle`, which is in lower case.
    pub fn containing(&self, needle: &str) -> BitSet {
        let mut set = BitSet::new(self.len());
        for (id, name) in self.lower.iter().enumerate() {
            if name.contains(needle) {
                set.insert(id);
            }
        }
        set
    }

    /// Each id's position in name order, case-insensitive, then exact. Equal
    /// names share a position, so that sorting falls back to the record number.
    pub fn ranks(&self) -> Vec<u32> {
        joint_ranks(&[self]).pop().unwrap_or_default()
    }
}

/// Positions in one name order over several tables together, per table and id:
/// a tournament and a guiding text's title sort among each other.
pub fn joint_ranks(tables: &[&NameTable]) -> Vec<Vec<u32>> {
    let mut all: Vec<(usize, usize)> =
        tables.iter().enumerate().flat_map(|(t, table)| (0..table.len()).map(move |id| (t, id))).collect();
    let name = |&(t, id): &(usize, usize)| (&*tables[t].lower[id], &*tables[t].names[id]);
    all.sort_unstable_by(|a, b| name(a).cmp(&name(b)));
    let mut ranks: Vec<Vec<u32>> = tables.iter().map(|t| vec![0; t.len()]).collect();
    let mut rank = 0u32;
    for (i, entry) in all.iter().enumerate() {
        if i > 0 && name(&all[i - 1]) != name(entry) {
            rank += 1;
        }
        ranks[entry.0][entry.1] = rank;
    }
    ranks
}

/// `name` cut to at most [`MAX_NAME_BYTES`] bytes, at a character boundary.
fn shorten(mut name: String) -> String {
    if name.len() > MAX_NAME_BYTES {
        let end = (0..=MAX_NAME_BYTES).rev().find(|&i| name.is_char_boundary(i)).unwrap_or(0);
        name.truncate(end);
    }
    name
}

/// A set of small non-negative integers.
pub struct BitSet {
    bits: Vec<u64>,
}

impl BitSet {
    pub fn new(len: usize) -> BitSet {
        BitSet { bits: vec![0; len.div_ceil(64)] }
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

    #[test]
    fn bit_sets() {
        let mut s = BitSet::new(130);
        s.insert(0);
        s.insert(129);
        assert!(s.contains(0) && s.contains(129) && !s.contains(64));
        assert!(!s.contains(10_000) && !s.contains_id(-1));
    }

    #[test]
    fn long_names_are_cut_at_a_character_boundary() {
        assert_eq!(shorten("é".repeat(100)).len(), 128);
        assert_eq!(shorten(format!("a{}", "é".repeat(100))).len(), 127);
        assert_eq!(shorten("short".into()), "short");
    }

    #[test]
    fn ranks_and_matching() {
        let t = NameTable {
            names: ["", "b", "A", "a"].map(Box::from).to_vec(),
            lower: ["", "b", "a", "a"].map(Box::from).to_vec(),
        };
        assert_eq!(t.ranks(), [0, 3, 1, 2]);
        // Across tables, and equal names (the two "a") sharing a rank.
        let u =
            NameTable { names: ["", "B", "a"].map(Box::from).to_vec(), lower: ["", "b", "a"].map(Box::from).to_vec() };
        assert_eq!(joint_ranks(&[&t, &u]), [vec![0, 4, 1, 2], vec![0, 3, 2]]);
        let s = t.containing("a");
        assert!(!s.contains(0) && !s.contains(1) && s.contains(2) && s.contains(3));
        assert_eq!((t.name(3), t.name(-1), t.name(9)), ("a", "", ""));
    }
}
