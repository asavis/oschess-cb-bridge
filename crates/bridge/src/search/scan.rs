//! Parallel passes over a database's header records, and the compiled search
//! predicate they evaluate.

use std::sync::atomic::{AtomicU64, Ordering};

use cbformat::v2::{Database, HEADER_RECORD_SIZE, Record, RecordKind};

use super::compare::{IntCmp, TextCmp, int_cmp, normalize_date, text_cmps};
use super::fields::{date_text, eco_text, round_text};

use super::SearchError;
use super::memory::{Allowance, Cancel, Refused};
use super::names::{BitSet, NameTable};
use super::query::{Cmp, Field, Query, Value};
use super::workers::{self, threads};

/// Header records read at a time by one worker.
const CHUNK: u32 = 16 << 10;
/// The batch buffer of one worker: 3 MiB, reserved in the budget and reused.
pub const BATCH_BYTES: usize = CHUNK as usize * HEADER_RECORD_SIZE;

/// What a pass answers to: the cancellation of its search, and a count of the
/// records it read.
pub struct Control<'a> {
    pub cancel: &'a Cancel,
    pub scanned: &'a AtomicU64,
}

/// Visits every record, in parallel over contiguous ranges of numbers, on the
/// workers [`workers::run`] grants. Each worker folds its range, in number
/// order, into its own accumulator, made by `init` from the number of records
/// it will visit and finished by `finish`; the accumulators come back in range
/// order. Every worker reads its batches into one reserved buffer, and stops at
/// its next batch once one has failed or the search is superseded.
pub fn scan<T: Send>(
    db: &Database,
    ctl: &Control<'_>,
    init: impl Fn(usize) -> Result<T, SearchError> + Sync,
    visit: impl Fn(&mut T, &Record) -> Result<(), SearchError> + Sync,
    finish: impl Fn(&mut T) + Sync,
) -> Result<Vec<T>, SearchError> {
    let total = u64::from(db.record_count());
    let want = (threads() as u64).min(total.div_ceil(u64::from(CHUNK))).max(1) as usize;
    workers::run(want, BATCH_BYTES, ctl.cancel, |w| {
        let per = total.div_ceil(w.count as u64);
        let (first, last) = (w.index as u64 * per + 1, ((w.index as u64 + 1) * per).min(total));
        let mut acc = init(last.saturating_sub(first - 1) as usize)?;
        let mut buf = w.buffer()?;
        let mut next = first;
        while next <= last {
            if w.stopped() || ctl.cancel.is_cancelled() {
                return Err(SearchError::Superseded);
            }
            let batch = (last - next + 1).min(u64::from(CHUNK)) as usize;
            let read = db.read_records(next as u32, &mut buf[..batch * HEADER_RECORD_SIZE])?;
            if read == 0 {
                break;
            }
            ctl.scanned.fetch_add(u64::from(read), Ordering::Relaxed);
            let bytes = read as usize * HEADER_RECORD_SIZE;
            for (i, b) in buf[..bytes].as_chunks::<HEADER_RECORD_SIZE>().0.iter().enumerate() {
                visit(&mut acc, &Record::from_bytes(next as u32 + i as u32, b))?;
            }
            next += u64::from(read);
        }
        finish(&mut acc);
        Ok(acc)
    })
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
