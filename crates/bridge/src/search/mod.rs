//! Search, sort and suggestions over a database's game headers
//! (`docs/search-grammar.md`). Everything built here belongs to one database
//! generation: a changed database is reopened with fresh [`Indexes`]. All of
//! it lives within the search memory budget of [`memory`].

mod compare;
pub mod gate;
pub mod heads;
mod members;
pub mod memory;
mod names;
mod order;
pub mod query;
mod scan;
mod slim;
mod sort;
mod suggest;
pub mod workers;

pub use members::{Members, Position};
pub use names::FILES_READ as NAME_FILES_READ;
pub use scan::BATCH_BYTES;
pub use suggest::{SuggestField, Suggestion, suggest};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use cbformat::view::Base;

use memory::{Allowance, Cancel, Evict, Held, Hold, Refused, Streams};
use names::{BitSet, Groups, Kind, NameTable, joint_ranks};
use query::{Field, Query, Sort, SortKey};
use scan::Control;

use crate::store::{Head, Store, with_store};

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
    /// Where annotators are not players.
    annotators: Slot<NameTable>,
    titles: Slot<NameTable>,
    player_ranks: Slot<Held<Vec<Vec<u32>>>>,
    annotator_ranks: Slot<Held<Vec<Vec<u32>>>>,
    /// Tournaments and titles in one name order: `[tournaments, titles]`.
    event_ranks: Slot<Held<Vec<Vec<u32>>>>,
    player_groups: Slot<Held<Groups>>,
    annotator_groups: Slot<Held<Groups>>,
    tournament_groups: Slot<Held<Groups>>,
    orders: Mutex<HashMap<Sort, Arc<OrderSlot>>>,
    counts: Slot<Held<suggest::Counts>>,
    /// The latest searches, newest last.
    results: Mutex<VecDeque<Kept>>,
    /// Searches started on this database, per client stream: the latest in a
    /// stream supersedes the others there.
    streams: Streams,
    /// Records read by all passes, for tests and diagnostics.
    scanned: AtomicU64,
    /// Where tests hold searches on this database.
    gate: gate::Gate,
    /// The database's heads file at this generation, once it is ready (#106).
    heads: Mutex<Option<Arc<heads::Heads>>>,
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
    /// Passes read `heads` from now on, while it stays usable.
    pub fn set_heads(&self, heads: Arc<heads::Heads>) {
        *self.heads.lock().unwrap_or_else(|e| e.into_inner()) = Some(heads);
    }

    /// Whether a heads file is set that passes still read.
    pub fn has_usable_heads(&self) -> bool {
        self.heads().is_some()
    }

    /// The heads file passes read, when one is set and still usable.
    /// A file found broken is let go here, so that its handles close and
    /// Windows lets its replacement take its name.
    fn heads(&self) -> Option<Arc<heads::Heads>> {
        let mut slot = self.heads.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|h| !h.usable()) {
            *slot = None;
        }
        slot.clone()
    }

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

    /// Where tests hold searches on this database.
    pub fn gate(&self) -> &gate::Gate {
        &self.gate
    }

    /// The names of `kind`; where annotators are players, their table is the
    /// players' one.
    pub(super) fn names<S: Store>(&self, db: &S, kind: Kind, cancel: &Cancel) -> Result<Arc<NameTable>, SearchError> {
        let kind = if kind == Kind::Annotators && S::ANNOTATORS_ARE_PLAYERS { Kind::Players } else { kind };
        let slot = match kind {
            Kind::Players => &self.players,
            Kind::Tournaments => &self.tournaments,
            Kind::Annotators => &self.annotators,
            Kind::Titles => &self.titles,
        };
        let table = cached(slot, || {
            let keys = match kind == Kind::Titles && S::TITLES_BY_RECORD {
                true => Some(self.title_keys(db, cancel)?),
                false => None,
            };
            // Beside a heads file the table is kept in a file of its own
            // (#108): read from there, else read from the database and
            // written there.
            let file = self.heads().and_then(|h| Some((names::file_path(&h.path, kind)?, h.generation)));
            if let Some((path, generation)) = &file {
                let count = usize::try_from(db.name_count(kind)).unwrap_or(usize::MAX);
                if let Some(table) = NameTable::open_file(path, kind, *generation, count) {
                    return table;
                }
            }
            let table = NameTable::load(db, kind, keys, cancel)?;
            if let Some((path, generation)) = file {
                table.to_be_written(path, kind, generation);
            }
            Ok(table)
        })?;
        table.write_later();
        Ok(table)
    }

    /// The numbers of the records with a title of their own, in order, for a
    /// format that keeps titles with their records: one pass over the headers.
    fn title_keys<S: Store>(&self, db: &S, cancel: &Cancel) -> Result<Held<Vec<u32>>, SearchError> {
        let heads = self.heads();
        let ctl = Control { cancel, scanned: &self.scanned, heads: heads.as_deref() };
        let found = Mutex::new(Hold::default());
        let parts = scan::scan(db, &ctl, |_| Ok((Vec::new(), Allowance::new(&found))), &TitleKeys, |_| {})?;
        let total: usize = parts.iter().map(|p| p.0.len()).sum();
        let hold = Hold::reserve(total * 4)?;
        let mut keys: Vec<u32> = Vec::new();
        keys.try_reserve_exact(total).map_err(|_| Refused::Busy)?;
        parts.iter().for_each(|p| keys.extend_from_slice(&p.0));
        Ok(Held::new(keys, hold))
    }

    /// Every record number in `sort` order.
    fn order<S: Store>(&self, db: &S, ctl: &Control<'_>, sort: Sort) -> Result<Numbers, SearchError> {
        let slot = self.orders.lock().unwrap_or_else(|e| e.into_inner()).entry(sort).or_default().clone();
        cached(&slot, || {
            // Refused before any name is read when the order itself cannot fit.
            if order::build_bytes(db.record_count()) > memory::budget() {
                return Err(SearchError::TooLarge);
            }
            let player_ranks =
                || cached(&self.player_ranks, || joint_ranks(&[&*self.names(db, Kind::Players, ctl.cancel)?]));
            let players = match sort.key {
                SortKey::White | SortKey::Black => Some(player_ranks()?),
                SortKey::Annotator if S::ANNOTATORS_ARE_PLAYERS => Some(player_ranks()?),
                _ => None,
            };
            let annotators = match sort.key {
                SortKey::Annotator if S::ANNOTATORS_ARE_PLAYERS => players.clone(),
                SortKey::Annotator => Some(cached(&self.annotator_ranks, || {
                    joint_ranks(&[&*self.names(db, Kind::Annotators, ctl.cancel)?])
                })?),
                _ => None,
            };
            let events = match sort.key {
                SortKey::Tournament => Some(cached(&self.event_ranks, || {
                    let tournaments = self.names(db, Kind::Tournaments, ctl.cancel)?;
                    joint_ranks(&[&*tournaments, &*self.names(db, Kind::Titles, ctl.cancel)?])
                })?),
                _ => None,
            };
            // Where a title's key is not its id, the table that maps them.
            let title_table = match sort.key {
                SortKey::Tournament if S::TITLES_BY_RECORD => Some(self.names(db, Kind::Titles, ctl.cancel)?),
                _ => None,
            };
            let ranks = order::Ranks {
                players: players.as_deref().map(|p| p[0].as_slice()),
                annotators: annotators.as_deref().map(|a| a[0].as_slice()),
                tournaments: events.as_deref().map(|e| e[0].as_slice()),
                titles: events.as_deref().map(|e| e[1].as_slice()),
                title_table: title_table.as_deref(),
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
        clear(&self.annotator_ranks);
        clear(&self.event_ranks);
        clear(&self.player_groups);
        clear(&self.annotator_groups);
        clear(&self.tournament_groups);
        clear(&self.players);
        clear(&self.tournaments);
        clear(&self.annotators);
        clear(&self.titles);
    }
}

/// A search kept for paging: its sort and query, the position it is narrowed
/// to, its result, and the games of that position before the query.
struct Kept {
    query: String,
    position: Option<u64>,
    numbers: Numbers,
    games: u64,
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
    /// The position index that finds a position's games was found damaged
    /// (#148): it is dropped and built again.
    IndexDamaged,
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
    db: &Base,
    idx: &Indexes,
    q: Option<&str>,
    stream: Option<&str>,
    sort_param: Option<Sort>,
) -> Result<(Selection, Sort), SearchError> {
    with_store!(db, db => select_in(db, idx, q, stream, sort_param, None)).map(|(s, sort, _)| (s, sort))
}

/// [`select`] among the games of `position` (#148): those `q` selects, in
/// the same order; that order; and how many games the position has before
/// `q`. A request with a position and a `stream` supersedes the search still
/// running in that stream, as one with `q` does. The result is kept for the
/// next windows under its sort, position and query.
pub fn select_position(
    db: &Base,
    idx: &Indexes,
    q: Option<&str>,
    stream: Option<&str>,
    sort_param: Option<Sort>,
    position: &dyn Position,
) -> Result<(Selection, Sort, u64), SearchError> {
    with_store!(db, db => select_in(db, idx, q, stream, sort_param, Some(position)))
}

fn select_in<S: Store>(
    db: &S,
    idx: &Indexes,
    q: Option<&str>,
    stream: Option<&str>,
    sort_param: Option<Sort>,
    position: Option<&dyn Position>,
) -> Result<(Selection, Sort, u64), SearchError> {
    let cancel = match stream {
        Some(stream) if q.is_some() || position.is_some() => idx.streams.newest(stream),
        _ => Cancel::never(),
    };
    idx.gate.enter();
    let q = q.unwrap_or("");
    let query = query::parse(q).map_err(|u| SearchError::Unsupported(u.0))?;
    let sort = sort_param.or(query.sort).unwrap_or(Sort::DEFAULT);
    let heads = idx.heads();
    let ctl = Control { cancel: &cancel, scanned: &idx.scanned, heads: heads.as_deref() };
    if query.terms.is_empty() && position.is_none() {
        return Ok(match sort.key {
            SortKey::Number => (Selection::All { descending: sort.descending }, sort, 0),
            _ => (Selection::Numbers(idx.order(db, &ctl, sort)?), sort, 0),
        });
    }
    let key = format!("{}|{}", sort.name(), q.trim());
    let at = position.map(|p| p.key());
    let kept = idx
        .results
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|k| k.query == key && k.position == at)
        .map(|k| (k.numbers.clone(), k.games));
    if let Some((numbers, games)) = kept {
        // A kept result answers at once, but not a request a newer one in its
        // stream has superseded meanwhile: that one gets `409 superseded`
        // whether or not its result was kept (#148).
        if cancel.is_cancelled() {
            return Err(SearchError::Superseded);
        }
        return Ok((Selection::Numbers(numbers), sort, games));
    }
    let members = position.map(|p| p.games(&cancel)).transpose()?;
    let games = members.as_ref().map_or(0, Members::count);
    // A position no game reaches needs neither a sort order nor a pass.
    let numbers = Arc::new(match &members {
        Some(members) if query.terms.is_empty() || games == 0 => members_in(db, idx, &ctl, members, games, sort)?,
        members => search(db, idx, &ctl, &query, sort, members.as_ref())?,
    });
    drop(members);
    let mut results = idx.results.lock().unwrap_or_else(|e| e.into_inner());
    results.push_back(Kept { query: key, position: at, numbers: numbers.clone(), games });
    while results.len() > KEPT_RESULTS || results.iter().map(|k| k.numbers.len()).sum::<usize>() > KEPT_NUMBERS {
        if results.pop_front().is_none() {
            break;
        }
    }
    Ok((Selection::Numbers(numbers), sort, games))
}

/// The `count` records of `members` in `sort` order: in number order, the
/// set's own; in any other, the key's whole order passed through the set,
/// which costs the same for any set.
fn members_in<S: Store>(
    db: &S,
    idx: &Indexes,
    ctl: &Control<'_>,
    members: &Members,
    count: u64,
    sort: Sort,
) -> Result<Held<Vec<u32>>, SearchError> {
    let count = usize::try_from(count).map_err(|_| SearchError::TooLarge)?;
    let hold = Hold::reserve(count.checked_mul(4).ok_or(SearchError::TooLarge)?)?;
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve_exact(count).map_err(|_| Refused::Busy)?;
    match sort {
        _ if count == 0 => {}
        Sort { key: SortKey::Number, descending } => {
            out.extend(members.iter().take(count));
            if descending {
                out.reverse();
            }
        }
        _ => out.extend(idx.order(db, ctl, sort)?.iter().copied().filter(|&n| members.contains(n)).take(count)),
    }
    Ok(Held::new(out, hold))
}

/// Appends to a vector whose growth is reserved in the budget first.
/// The numbers of the records with a title of their own.
struct TitleKeys;

impl<'h> scan::Visit<(Vec<u32>, Allowance<'h>)> for TitleKeys {
    fn visit(&self, acc: &mut (Vec<u32>, Allowance<'h>), r: &impl Head) -> Result<(), SearchError> {
        if r.other().is_some_and(|(key, _)| key >= 0) {
            push_u32(&mut acc.0, r.id(), &mut acc.1)?;
        }
        Ok(())
    }
}

/// The numbers of the records a query matches, among the games of a
/// position when one is given.
struct Matching<'m, 'a>(&'m scan::Matcher<'a>, Option<&'m Members>);

impl<'h> scan::Visit<(Vec<u32>, Allowance<'h>)> for Matching<'_, '_> {
    fn visit(&self, acc: &mut (Vec<u32>, Allowance<'h>), r: &impl Head) -> Result<(), SearchError> {
        if self.1.is_none_or(|m| m.contains(r.id())) && self.0.matches(r) {
            push_u32(&mut acc.0, r.id(), &mut acc.1)?;
        }
        Ok(())
    }
}

fn push_u32(v: &mut Vec<u32>, x: u32, allow: &mut Allowance<'_>) -> Result<(), Refused> {
    if v.len() == v.capacity() {
        let add = v.capacity().max(1024);
        allow.take(add * 4)?;
        v.try_reserve_exact(add).map_err(|_| Refused::Busy)?;
    }
    v.push(x);
    Ok(())
}

/// The records `query` matches, among `members` when given, in `sort` order:
/// the pass that evaluates the query tests the membership first.
fn search<S: Store>(
    db: &S,
    idx: &Indexes,
    ctl: &Control<'_>,
    query: &Query,
    sort: Sort,
    members: Option<&Members>,
) -> Result<Held<Vec<u32>>, SearchError> {
    let uses = |fields: &[Field]| query.terms.iter().any(|t| fields.contains(&t.field));
    let load = |used: bool, kind| if used { idx.names(db, kind, ctl.cancel).map(Some) } else { Ok(None) };
    let people = uses(&[Field::Text, Field::White, Field::Black, Field::Player]);
    let annotated = uses(&[Field::Text, Field::Annotator]);
    let players = load(people || (annotated && S::ANNOTATORS_ARE_PLAYERS), Kind::Players)?;
    let annotators = match S::ANNOTATORS_ARE_PLAYERS {
        true => players.clone(),
        false => load(annotated, Kind::Annotators)?,
    };
    let events = uses(&[Field::Text, Field::Event]);
    let (tournaments, titles) = (load(events, Kind::Tournaments)?, load(events, Kind::Titles)?);
    let tables = scan::Tables {
        players: players.as_deref(),
        annotators: annotators.as_deref(),
        annotators_are_players: S::ANNOTATORS_ARE_PLAYERS,
        tournaments: tournaments.as_deref(),
        titles: titles.as_deref(),
    };
    let sets = Mutex::new(Hold::default());
    let matcher = scan::Matcher::new(query, &tables, &mut Allowance::new(&sets))?;
    let found = Mutex::new(Hold::default());
    let parts =
        scan::scan(db, ctl, |_| Ok((Vec::new(), Allowance::new(&found))), &Matching(&matcher, members), |_| {})?;
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
