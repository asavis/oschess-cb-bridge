//! Parallel passes over a database's header records, and the compiled search
//! predicate they evaluate.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cbformat::v2::{Database, Record, RecordKind};

use super::compare::{IntCmp, TextCmp, int_cmp, normalize_date, text_cmps};
use super::fields::{date_text, eco_text, round_text};

use super::SearchError;
use super::memory::{Allowance, Cancel, Refused};
use super::names::{BitSet, NameTable};
use super::query::{Cmp, Field, Query, Value};

/// Header records read at a time by one worker: 3 MiB.
const CHUNK: u32 = 16 << 10;

/// Workers for a pass: `OSCHESS_BRIDGE_THREADS` when set (1 to 64), else the
/// machine's cores, at most 16.
pub fn threads() -> usize {
    static THREADS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THREADS.get_or_init(|| {
        let set = std::env::var("OSCHESS_BRIDGE_THREADS").ok().and_then(|v| v.trim().parse::<usize>().ok());
        match set {
            Some(n) => n.clamp(1, 64),
            None => std::thread::available_parallelism().map_or(1, |n| n.get()).min(16),
        }
    })
}

/// What a pass answers to: the cancellation of its search, and a count of the
/// records it read.
pub struct Control<'a> {
    pub cancel: &'a Cancel,
    pub scanned: &'a AtomicU64,
}

/// Visits every record, in parallel over contiguous ranges of numbers. Each
/// worker folds its range, in number order, into its own accumulator, made by
/// `init` from the number of records it will visit; the accumulators come back
/// in range order. Every worker stops at its next batch once one has failed or
/// the search is superseded.
pub fn scan<T: Send>(
    db: &Database,
    ctl: &Control<'_>,
    init: impl Fn(usize) -> Result<T, SearchError> + Sync,
    visit: impl Fn(&mut T, &Record) -> Result<(), SearchError> + Sync,
) -> Result<Vec<T>, SearchError> {
    let total = u64::from(db.record_count());
    let workers = (threads() as u64).min(total.div_ceil(u64::from(CHUNK))).max(1);
    let per = total.div_ceil(workers);
    let (init, visit) = (&init, &visit);
    let failed = AtomicBool::new(false);
    let failed = &failed;
    let parts: Vec<Result<T, SearchError>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                let (first, last) = (w * per + 1, ((w + 1) * per).min(total));
                s.spawn(move || -> Result<T, SearchError> {
                    let run = || -> Result<T, SearchError> {
                        let mut acc = init(last.saturating_sub(first - 1) as usize)?;
                        let mut next = first;
                        while next <= last {
                            if failed.load(Ordering::Relaxed) {
                                return Err(SearchError::Superseded);
                            }
                            if ctl.cancel.is_cancelled() {
                                return Err(SearchError::Superseded);
                            }
                            let upto = last.min(next + u64::from(CHUNK) - 1);
                            let records = db.records(next as u32, upto as u32)?;
                            let Some(end) = records.last().map(|r| u64::from(r.id())) else { break };
                            ctl.scanned.fetch_add(records.len() as u64, Ordering::Relaxed);
                            for r in &records {
                                visit(&mut acc, r)?;
                            }
                            next = end + 1;
                        }
                        Ok(acc)
                    };
                    let result = run();
                    if result.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    result
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
    });
    // The first real failure explains the others, which only stopped for it.
    let mut out = Vec::with_capacity(parts.len());
    let mut stopped = None;
    for part in parts {
        match part {
            Ok(acc) => out.push(acc),
            Err(SearchError::Superseded) => stopped = Some(SearchError::Superseded),
            Err(e) => return Err(e),
        }
    }
    match stopped {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// Which of a game's player ids a name test looks at.
#[derive(Clone, Copy)]
enum Role {
    White,
    Black,
    Either,
    Annotator,
}

enum Test {
    Players(BitSet, Role),
    /// A game's tournament, or the title of a guiding text or analysis.
    Event {
        tournaments: BitSet,
        titles: BitSet,
    },
    /// A bare word: a game's players, annotator and tournament, or a guiding
    /// text's or analysis's title and author.
    Text {
        players: BitSet,
        tournaments: BitSet,
        titles: BitSet,
    },
    Result(String),
    Eco(Vec<TextCmp>),
    /// All the comparisons, on a date whose first `known` characters have no `?`.
    Date {
        cmps: Vec<TextCmp>,
        known: usize,
    },
    DateContains(String),
    Round(String),
    Moves(IntCmp),
    Elo(IntCmp),
    Never,
}

struct CompiledTerm {
    tests: Vec<Test>,
    negated: bool,
}

pub struct Matcher {
    terms: Vec<CompiledTerm>,
    /// A term on a field only games have keeps guiding texts and analyses out,
    /// as the Library keeps folders out of such searches.
    games_only: bool,
}

/// The name tables a search needs; `None` where no term uses them.
pub struct Tables<'a> {
    pub players: Option<&'a NameTable>,
    pub tournaments: Option<&'a NameTable>,
    pub titles: Option<&'a NameTable>,
}

/// The fields of a record that is not a game: guiding texts and analyses have
/// header layouts of their own, sharing only their first eight bytes with games.
struct Other {
    title: i64,
    author: i64,
}

fn other(r: &Record) -> Option<Other> {
    match r.kind() {
        RecordKind::Game => None,
        RecordKind::Text => Some(Other { title: r.text_title(), author: r.text_author() }),
        RecordKind::Analysis => Some(Other { title: r.analysis_title(), author: r.analysis_author() }),
        RecordKind::Unknown(_) => Some(Other { title: -1, author: -1 }),
    }
}

/// Fields a guiding text or an analysis has too: its title as the event and
/// its author as the annotator, as its list row shows them.
pub fn shared(field: Field) -> bool {
    matches!(field, Field::Text | Field::Event | Field::Annotator)
}

impl Matcher {
    /// The query compiled against `tables`; the id sets its name terms need
    /// are taken from `allow`.
    pub fn new(query: &Query, tables: &Tables<'_>, allow: &mut Allowance<'_>) -> Result<Matcher, Refused> {
        let mut set = |t: Option<&NameTable>, needle: &str| match t {
            Some(t) => t.containing(needle, allow),
            None => BitSet::new(0, allow),
        };
        let mut terms = Vec::with_capacity(query.terms.len());
        for term in &query.terms {
            let mut tests = Vec::with_capacity(term.values.len());
            for v in &term.values {
                let needle = v.text.to_lowercase();
                tests.push(match term.field {
                    Field::Text => Test::Text {
                        players: set(tables.players, &needle)?,
                        tournaments: set(tables.tournaments, &needle)?,
                        titles: set(tables.titles, &needle)?,
                    },
                    Field::White => Test::Players(set(tables.players, &needle)?, Role::White),
                    Field::Black => Test::Players(set(tables.players, &needle)?, Role::Black),
                    Field::Player => Test::Players(set(tables.players, &needle)?, Role::Either),
                    Field::Annotator => Test::Players(set(tables.players, &needle)?, Role::Annotator),
                    Field::Event => Test::Event {
                        tournaments: set(tables.tournaments, &needle)?,
                        titles: set(tables.titles, &needle)?,
                    },
                    Field::Result => Test::Result(v.text.clone()),
                    Field::Round => Test::Round(needle),
                    Field::Eco => Test::Eco(text_cmps(v.text.as_bytes(), v)),
                    Field::Date => date_test(v),
                    Field::Moves => Test::Moves(int_cmp(v)),
                    Field::Elo => Test::Elo(int_cmp(v)),
                });
            }
            terms.push(CompiledTerm { negated: term.negated, tests });
        }
        let games_only = query.terms.iter().any(|t| !shared(t.field));
        Ok(Matcher { terms, games_only })
    }

    pub fn matches(&self, r: &Record) -> bool {
        let other = other(r);
        if other.is_some() && self.games_only {
            return false;
        }
        self.terms.iter().all(|t| t.tests.iter().any(|test| test.holds(r, other.as_ref())) != t.negated)
    }
}

fn date_test(v: &Value) -> Test {
    let low = normalize_date(&v.text);
    if v.cmp == Cmp::Equal {
        return match low {
            Some(prefix) => Test::Date { cmps: vec![TextCmp::Prefix(prefix.into_bytes())], known: 0 },
            None => Test::DateContains(v.text.to_lowercase()),
        };
    }
    let high = if v.cmp == Cmp::Range { v.upper.as_deref().and_then(normalize_date) } else { low.clone() };
    let (Some(low), Some(high)) = (low, high) else { return Test::Never };
    let known = if v.cmp == Cmp::Range { low.len().max(high.len()) } else { low.len() };
    let bounds = Value { text: low.clone(), cmp: v.cmp.clone(), upper: Some(high) };
    Test::Date { cmps: text_cmps(low.as_bytes(), &bounds), known }
}

impl Test {
    fn holds(&self, r: &Record, other: Option<&Other>) -> bool {
        if let Some(o) = other {
            return match self {
                Test::Text { players, titles, .. } => titles.contains_id(o.title) || players.contains_id(o.author),
                Test::Event { titles, .. } => titles.contains_id(o.title),
                Test::Players(set, Role::Annotator) => set.contains_id(o.author),
                _ => false,
            };
        }
        match self {
            Test::Text { players, tournaments, .. } => {
                tournaments.contains_id(r.tournament())
                    || players.contains_id(r.white())
                    || players.contains_id(r.black())
                    || players.contains_id(r.annotator())
            }
            Test::Players(set, role) => match role {
                Role::White => set.contains_id(r.white()),
                Role::Black => set.contains_id(r.black()),
                Role::Either => set.contains_id(r.white()) || set.contains_id(r.black()),
                Role::Annotator => set.contains_id(r.annotator()),
            },
            Test::Event { tournaments, .. } => tournaments.contains_id(r.tournament()),
            Test::Result(result) => r.result().pgn() == result,
            Test::Eco(cmps) => eco_text(r).is_some_and(|e| cmps.iter().all(|c| c.holds(&e))),
            Test::Date { cmps, known } => {
                let d = date_text(r);
                !d[..(*known).min(10)].contains(&b'?') && cmps.iter().all(|c| c.holds(&d))
            }
            Test::DateContains(needle) => std::str::from_utf8(&date_text(r)).is_ok_and(|d| d.contains(needle.as_str())),
            Test::Round(needle) => round_text(r, &mut [0; 16]).contains(needle.as_str()),
            Test::Moves(cmp) => cmp.holds(i64::from(r.move_count())),
            Test::Elo(cmp) => [r.white_elo(), r.black_elo()].into_iter().any(|e| e > 0 && cmp.holds(i64::from(e))),
            Test::Never => false,
        }
    }
}
