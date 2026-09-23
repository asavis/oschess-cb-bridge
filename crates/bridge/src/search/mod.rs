//! Search, sort and suggestions over a database's game headers
//! (`docs/search-grammar.md`). Everything built here belongs to one database
//! generation: a changed database is reopened with fresh [`Indexes`]. All of
//! it lives within the search memory budget of [`memory`].

mod compare;
mod fields;
pub mod memory;
mod names;
mod order;
pub mod query;
mod scan;
mod sort;
mod suggest;

pub use suggest::{SuggestField, Suggestion, suggest};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use cbformat::v2::Database;

use memory::{Allowance, Cancel, Evict, Held, Hold, Refused, Streams};
use names::{BitSet, Groups, Kind, NameTable, joint_ranks};
use query::{Field, Query, Sort, SortKey};
use scan::Control;

/// Searches whose results are kept for paging.
const KEPT_RESULTS: usize = 4;
/// Record numbers kept over all those results: 128 MiB.
const KEPT_NUMBERS: usize = 32 << 20;

type Slot<T> = Mutex<Option<Arc<T>>>;
type OrderSlot = Slot<Held<Vec<u32>>>;
/// Record numbers in the order a list shows them, with the memory they hold.
pub type Numbers = Arc<Held<Vec<u32>>>;

/// What has been built for one generation of one database.
#[derive(Default)]
pub struct Indexes {
    players: Slot<NameTable>,
    tournaments: Slot<NameTable>,
    titles: Slot<NameTable>,
    player_ranks: Slot<Held<Vec<Vec<u32>>>>,
    /// Tournaments and titles in one name order: `[tournaments, titles]`.
    event_ranks: Slot<Held<Vec<Vec<u32>>>>,
    player_groups: Slot<Held<Groups>>,
    tournament_groups: Slot<Held<Groups>>,
    orders: Mutex<HashMap<Sort, Arc<OrderSlot>>>,
    counts: Slot<Held<suggest::Counts>>,
    /// The latest searches, newest last: the query and sort, and the result.
    results: Mutex<VecDeque<(String, Numbers)>>,
    /// Searches started on this database, per client stream: the latest in a
    /// stream supersedes the others there.
    streams: Streams,
    /// Records read by all passes, for tests and diagnostics.
    scanned: AtomicU64,
}

/// The value in `slot`, built by `build` the first time. Concurrent callers
/// wait for the one build instead of repeating it; a failed build stores
/// nothing, and the next caller builds again.
pub(super) fn cached<T, E>(slot: &Slot<T>, build: impl FnOnce() -> Result<T, E>) -> Result<Arc<T>, E> {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = guard.as_ref() {
        return Ok(v.clone());
    }
    let v = Arc::new(build()?);
    *guard = Some(v.clone());
    Ok(v)
}

impl Indexes {
    /// Indexes whose retained structures are evicted when the budget runs short.
    pub fn shared() -> Arc<Indexes> {
        let indexes = Arc::new(Indexes::default());
        let weak: Weak<dyn Evict> = Arc::downgrade(&indexes) as Weak<dyn Evict>;
        memory::register(weak);
        indexes
    }

    /// Records read by all passes over this database so far.
    pub fn scanned(&self) -> u64 {
        self.scanned.load(Ordering::Relaxed)
    }

    pub(super) fn names(&self, db: &Database, kind: Kind, cancel: &Cancel) -> Result<Arc<NameTable>, SearchError> {
        let slot = match kind {
            Kind::Players => &self.players,
            Kind::Tournaments => &self.tournaments,
            Kind::Titles => &self.titles,
        };
        cached(slot, || NameTable::load(db, kind, cancel))
    }

    /// Every record number in `sort` order.
    fn order(&self, db: &Database, ctl: &Control<'_>, sort: Sort) -> Result<Numbers, SearchError> {
        let slot = self.orders.lock().unwrap_or_else(|e| e.into_inner()).entry(sort).or_default().clone();
        cached(&slot, || {
            // Refused before any name is read when the order itself cannot fit.
            if order::build_bytes(db.record_count()) > memory::budget() {
                return Err(SearchError::TooLarge);
            }
            let players = match sort.key {
                SortKey::White | SortKey::Black | SortKey::Annotator => {
                    Some(cached(&self.player_ranks, || joint_ranks(&[&*self.names(db, Kind::Players, ctl.cancel)?]))?)
                }
                _ => None,
            };
            let events = match sort.key {
                SortKey::Tournament => Some(cached(&self.event_ranks, || {
                    let tournaments = self.names(db, Kind::Tournaments, ctl.cancel)?;
                    joint_ranks(&[&*tournaments, &*self.names(db, Kind::Titles, ctl.cancel)?])
                })?),
                _ => None,
            };
            let ranks = order::Ranks {
                players: players.as_deref().map(|p| p[0].as_slice()),
                tournaments: events.as_deref().map(|e| e[0].as_slice()),
                titles: events.as_deref().map(|e| e[1].as_slice()),
            };
            order::build(db, ctl, sort, &ranks)
        })
    }
}

impl Evict for Indexes {
    /// Drops what is retained, skipping what a build holds right now. Memory
    /// still in use by a request is returned when that request ends.
    fn evict(&self) {
        fn clear<T>(slot: &Slot<T>) {
            if let Ok(mut s) = slot.try_lock() {
                s.take();
            }
        }
        // The slots stay: one being built keeps its place and is retained.
        if let Ok(orders) = self.orders.try_lock() {
            orders.values().for_each(|slot| clear(slot));
        }
        if let Ok(mut results) = self.results.try_lock() {
            results.clear();
        }
        clear(&self.counts);
        clear(&self.player_ranks);
        clear(&self.event_ranks);
        clear(&self.player_groups);
        clear(&self.tournament_groups);
        clear(&self.players);
        clear(&self.tournaments);
        clear(&self.titles);
    }
}

/// Which records a list request shows.
pub enum Selection {
    /// Every record, in number order: no search and no other sort.
    All { descending: bool },
    /// These record numbers, in this order.
    Numbers(Numbers),
}

#[derive(Debug)]
pub enum SearchError {
    /// A qualifier ChessBase databases do not have, as typed.
    Unsupported(String),
    Read(cbformat::Error),
    /// The search structures could never fit in the memory budget.
    TooLarge,
    /// The budget is taken by other searches now.
    Busy,
    /// A newer search on the same database replaced this one.
    Superseded,
}

impl From<cbformat::Error> for SearchError {
    fn from(e: cbformat::Error) -> Self {
        SearchError::Read(e)
    }
}

impl From<Refused> for SearchError {
    fn from(r: Refused) -> Self {
        match r {
            Refused::TooLarge => SearchError::TooLarge,
            Refused::Busy => SearchError::Busy,
        }
    }
}

/// The records `q` selects, in the order of `sort_param`, else of the query's
/// `sort:` token, else by number; and that order. A request that carries a `q`,
/// even an empty one, and names a `stream` supersedes the search still running
/// in that stream on the same database; without a stream nothing is superseded.
pub fn select(
    db: &Database,
    idx: &Indexes,
    q: Option<&str>,
    stream: Option<&str>,
    sort_param: Option<Sort>,
) -> Result<(Selection, Sort), SearchError> {
    let cancel = match (q, stream) {
        (Some(_), Some(stream)) => idx.streams.newest(stream),
        _ => Cancel::never(),
    };
    let q = q.unwrap_or("");
    let query = query::parse(q).map_err(|u| SearchError::Unsupported(u.0))?;
    let sort = sort_param.or(query.sort).unwrap_or(Sort::DEFAULT);
    let ctl = Control { cancel: &cancel, scanned: &idx.scanned };
    if query.terms.is_empty() {
        return Ok(match sort.key {
            SortKey::Number => (Selection::All { descending: sort.descending }, sort),
            _ => (Selection::Numbers(idx.order(db, &ctl, sort)?), sort),
        });
    }
    let key = format!("{}|{}", sort.name(), q.trim());
    let kept =
        idx.results.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone());
    if let Some(numbers) = kept {
        return Ok((Selection::Numbers(numbers), sort));
    }
    let numbers = Arc::new(search(db, idx, &ctl, &query, sort)?);
    let mut results = idx.results.lock().unwrap_or_else(|e| e.into_inner());
    results.push_back((key, numbers.clone()));
    while results.len() > KEPT_RESULTS || results.iter().map(|(_, v)| v.len()).sum::<usize>() > KEPT_NUMBERS {
        if results.pop_front().is_none() {
            break;
        }
    }
    Ok((Selection::Numbers(numbers), sort))
}

/// Appends to a vector whose growth is reserved in the budget first.
fn push_u32(v: &mut Vec<u32>, x: u32, allow: &mut Allowance<'_>) -> Result<(), Refused> {
    if v.len() == v.capacity() {
        let add = v.capacity().max(1024);
        allow.take(add * 4)?;
        v.try_reserve_exact(add).map_err(|_| Refused::Busy)?;
    }
    v.push(x);
    Ok(())
}

fn search(
    db: &Database,
    idx: &Indexes,
    ctl: &Control<'_>,
    query: &Query,
    sort: Sort,
) -> Result<Held<Vec<u32>>, SearchError> {
    let uses = |fields: &[Field]| query.terms.iter().any(|t| fields.contains(&t.field));
    let load = |used: bool, kind| if used { idx.names(db, kind, ctl.cancel).map(Some) } else { Ok(None) };
    let players =
        load(uses(&[Field::Text, Field::White, Field::Black, Field::Player, Field::Annotator]), Kind::Players)?;
    let events = uses(&[Field::Text, Field::Event]);
    let (tournaments, titles) = (load(events, Kind::Tournaments)?, load(events, Kind::Titles)?);
    let tables =
        scan::Tables { players: players.as_deref(), tournaments: tournaments.as_deref(), titles: titles.as_deref() };
    let sets = Mutex::new(Hold::default());
    let matcher = scan::Matcher::new(query, &tables, &mut Allowance::new(&sets))?;
    let found = Mutex::new(Hold::default());
    let parts = scan::scan(
        db,
        ctl,
        |_| Ok((Vec::new(), Allowance::new(&found))),
        |(numbers, allow), r| {
            if matcher.matches(r) {
                push_u32(numbers, r.id(), allow)?;
            }
            Ok(())
        },
    )?;
    let parts: Vec<Vec<u32>> = parts.into_iter().map(|(numbers, _)| numbers).collect();
    let matches: usize = parts.iter().map(Vec::len).sum();
    let mut hold = Hold::reserve(matches * 4)?;
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve_exact(matches).map_err(|_| Refused::Busy)?;
    match sort {
        Sort { key: SortKey::Number, descending } => {
            parts.iter().for_each(|p| out.extend_from_slice(p));
            if descending {
                out.reverse();
            }
        }
        _ => {
            let set_hold = Mutex::new(Hold::default());
            let mut set = BitSet::new(db.record_count() as usize + 1, &mut Allowance::new(&set_hold))?;
            parts.iter().flatten().for_each(|&n| set.insert(n as usize));
            drop(parts);
            drop(found);
            out.extend(idx.order(db, ctl, sort)?.iter().copied().filter(|&n| set.contains(n as usize)));
        }
    }
    hold.shrink(out.capacity() * 4);
    Ok(Held::new(out, hold))
}
