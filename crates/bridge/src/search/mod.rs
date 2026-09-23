//! Search, sort and suggestions over a database's game headers
//! (`docs/search-grammar.md`). Everything built here belongs to one database
//! generation: a changed database is reopened with fresh [`Indexes`].

mod fields;
mod names;
mod order;
pub mod query;
mod scan;
mod sort;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cbformat::v2::{Database, RecordKind};

use names::{BitSet, Kind, NameTable, joint_ranks};
use query::{Field, Query, Sort, SortKey};

/// Searches whose results are kept for paging.
const KEPT_RESULTS: usize = 4;
/// Record numbers kept over all those results: 128 MiB.
const KEPT_NUMBERS: usize = 32 << 20;

type Slot<T> = Mutex<Option<Arc<T>>>;

/// What has been built for one generation of one database.
#[derive(Default)]
pub struct Indexes {
    players: Slot<NameTable>,
    tournaments: Slot<NameTable>,
    titles: Slot<NameTable>,
    player_ranks: Slot<Vec<u32>>,
    /// Tournaments and titles in one name order: `[tournaments, titles]`.
    event_ranks: Slot<Vec<Vec<u32>>>,
    orders: Mutex<HashMap<Sort, Arc<Slot<Vec<u32>>>>>,
    counts: Slot<Counts>,
    /// The latest searches, newest last: the query and sort, and the result.
    results: Mutex<VecDeque<(String, Arc<Vec<u32>>)>>,
}

/// The value in `slot`, built by `build` the first time. Concurrent callers
/// wait for the one build instead of repeating it.
fn cached<T>(slot: &Slot<T>, build: impl FnOnce() -> cbformat::Result<T>) -> cbformat::Result<Arc<T>> {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = guard.as_ref() {
        return Ok(v.clone());
    }
    let v = Arc::new(build()?);
    *guard = Some(v.clone());
    Ok(v)
}

impl Indexes {
    fn names(&self, db: &Database, kind: Kind) -> cbformat::Result<Arc<NameTable>> {
        let slot = match kind {
            Kind::Players => &self.players,
            Kind::Tournaments => &self.tournaments,
            Kind::Titles => &self.titles,
        };
        cached(slot, || NameTable::load(db, kind))
    }

    fn player_ranks(&self, db: &Database) -> cbformat::Result<Arc<Vec<u32>>> {
        cached(&self.player_ranks, || Ok(self.names(db, Kind::Players)?.ranks()))
    }

    fn event_ranks(&self, db: &Database) -> cbformat::Result<Arc<Vec<Vec<u32>>>> {
        cached(&self.event_ranks, || {
            let (tournaments, titles) = (self.names(db, Kind::Tournaments)?, self.names(db, Kind::Titles)?);
            Ok(joint_ranks(&[&tournaments, &titles]))
        })
    }

    /// Every record number in `sort` order.
    fn order(&self, db: &Database, sort: Sort) -> cbformat::Result<Arc<Vec<u32>>> {
        let slot = self.orders.lock().unwrap_or_else(|e| e.into_inner()).entry(sort).or_default().clone();
        cached(&slot, || {
            let players = match sort.key {
                SortKey::White | SortKey::Black | SortKey::Annotator => Some(self.player_ranks(db)?),
                _ => None,
            };
            let events = if sort.key == SortKey::Tournament { Some(self.event_ranks(db)?) } else { None };
            let ranks = order::Ranks {
                players: players.as_deref().map(Vec::as_slice),
                tournaments: events.as_deref().map(|e| e[0].as_slice()),
                titles: events.as_deref().map(|e| e[1].as_slice()),
            };
            order::build(db, sort, &ranks)
        })
    }
}

/// Which records a list request shows.
pub enum Selection {
    /// Every record, in number order: no search and no other sort.
    All { descending: bool },
    /// These record numbers, in this order.
    Numbers(Arc<Vec<u32>>),
}

pub enum SearchError {
    /// A qualifier ChessBase databases do not have, as typed.
    Unsupported(String),
    Read(cbformat::Error),
}

impl From<cbformat::Error> for SearchError {
    fn from(e: cbformat::Error) -> Self {
        SearchError::Read(e)
    }
}

/// The records `q` selects, in the order of `sort_param`, else of the query's
/// `sort:` token, else by number; and that order.
pub fn select(
    db: &Database,
    idx: &Indexes,
    q: &str,
    sort_param: Option<Sort>,
) -> Result<(Selection, Sort), SearchError> {
    let query = query::parse(q).map_err(|u| SearchError::Unsupported(u.0))?;
    let sort = sort_param.or(query.sort).unwrap_or(Sort::DEFAULT);
    if query.terms.is_empty() {
        return Ok(match sort.key {
            SortKey::Number => (Selection::All { descending: sort.descending }, sort),
            _ => (Selection::Numbers(idx.order(db, sort)?), sort),
        });
    }
    let key = format!("{}|{}", sort.name(), q.trim());
    let kept =
        idx.results.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone());
    if let Some(numbers) = kept {
        return Ok((Selection::Numbers(numbers), sort));
    }
    let numbers = Arc::new(search(db, idx, &query, sort)?);
    let mut results = idx.results.lock().unwrap_or_else(|e| e.into_inner());
    results.push_back((key, numbers.clone()));
    while results.len() > KEPT_RESULTS || results.iter().map(|(_, v)| v.len()).sum::<usize>() > KEPT_NUMBERS {
        if results.pop_front().is_none() {
            break;
        }
    }
    Ok((Selection::Numbers(numbers), sort))
}

fn search(db: &Database, idx: &Indexes, query: &Query, sort: Sort) -> cbformat::Result<Vec<u32>> {
    let uses = |fields: &[Field]| query.terms.iter().any(|t| fields.contains(&t.field));
    let load = |used: bool, kind| if used { idx.names(db, kind).map(Some) } else { Ok(None) };
    let players =
        load(uses(&[Field::Text, Field::White, Field::Black, Field::Player, Field::Annotator]), Kind::Players)?;
    let events = uses(&[Field::Text, Field::Event]);
    let (tournaments, titles) = (load(events, Kind::Tournaments)?, load(events, Kind::Titles)?);
    let tables =
        scan::Tables { players: players.as_deref(), tournaments: tournaments.as_deref(), titles: titles.as_deref() };
    let matcher = scan::Matcher::new(query, &tables);
    let parts = scan::scan(db, Vec::new, |found: &mut Vec<u32>, r| {
        if matcher.matches(r) {
            found.push(r.id());
        }
    })?;
    let found: Vec<u32> = parts.into_iter().flatten().collect();
    Ok(match sort {
        Sort { key: SortKey::Number, descending: false } => found,
        Sort { key: SortKey::Number, descending: true } => found.into_iter().rev().collect(),
        _ => {
            let mut set = BitSet::new(db.record_count() as usize + 1);
            for &n in &found {
                set.insert(n as usize);
            }
            idx.order(db, sort)?.iter().copied().filter(|&n| set.contains(n as usize)).collect()
        }
    })
}

/// How many games each player, annotator and tournament appears in.
struct Counts {
    players: Vec<u32>,
    annotators: Vec<u32>,
    tournaments: Vec<u32>,
}

fn counts(db: &Database, players: usize, tournaments: usize) -> cbformat::Result<Counts> {
    let zeros = |n: usize| (0..n).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
    let (p, a, t) = (zeros(players), zeros(players), zeros(tournaments));
    let bump = |v: &[AtomicU32], id: i64| {
        if let Some(c) = usize::try_from(id).ok().and_then(|i| v.get(i)) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    };
    scan::scan(
        db,
        || (),
        |_, r| {
            if matches!(r.kind(), RecordKind::Game) {
                bump(&p, r.white());
                // A game counts once for a player who is recorded with both colours.
                if r.black() != r.white() {
                    bump(&p, r.black());
                }
                bump(&a, r.annotator());
                bump(&t, r.tournament());
            }
        },
    )?;
    let plain = |v: Vec<AtomicU32>| v.into_iter().map(AtomicU32::into_inner).collect();
    Ok(Counts { players: plain(p), annotators: plain(a), tournaments: plain(t) })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestField {
    Player,
    Event,
    Annotator,
}

/// Up to `limit` names of `field` starting with `prefix` (or whose first name
/// does, for people), with their game counts: most games first, then by name.
pub fn suggest(
    db: &Database,
    idx: &Indexes,
    field: SuggestField,
    prefix: &str,
    limit: usize,
) -> cbformat::Result<Vec<(String, u32)>> {
    let players = idx.names(db, Kind::Players)?;
    let tournaments = idx.names(db, Kind::Tournaments)?;
    let counts = cached(&idx.counts, || counts(db, players.len(), tournaments.len()))?;
    let (table, games) = match field {
        SuggestField::Player => (&players, &counts.players),
        SuggestField::Annotator => (&players, &counts.annotators),
        SuggestField::Event => (&tournaments, &counts.tournaments),
    };
    let prefix = prefix.trim().to_lowercase();
    let mut found: HashMap<&str, u32> = HashMap::new();
    for (id, &n) in games.iter().enumerate() {
        let lower = table.lower(id);
        let first_name = lower.split_once(", ").map(|(_, f)| f);
        if n > 0 && (lower.starts_with(&prefix) || first_name.is_some_and(|f| f.starts_with(&prefix))) {
            *found.entry(table.name(id as i64)).or_default() += n;
        }
    }
    let mut list: Vec<(String, u32)> = found.into_iter().map(|(name, n)| (name.to_string(), n)).collect();
    list.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())).then_with(|| a.0.cmp(&b.0))
    });
    list.truncate(limit);
    Ok(list)
}
