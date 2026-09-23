//! Parallel passes over a database's header records, and the compiled search
//! predicate they evaluate.

use cbformat::v2::{Database, Record, RecordKind};

use super::fields::{date_text, eco_text, round_text};

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

/// Visits every record, in parallel over contiguous ranges of numbers. Each
/// worker folds its range, in number order, into its own accumulator; the
/// accumulators come back in range order.
pub fn scan<T: Send>(
    db: &Database,
    init: impl Fn() -> T + Sync,
    visit: impl Fn(&mut T, &Record) + Sync,
) -> cbformat::Result<Vec<T>> {
    let total = u64::from(db.record_count());
    let workers = (threads() as u64).min(total.div_ceil(u64::from(CHUNK))).max(1);
    let per = total.div_ceil(workers);
    let (init, visit) = (&init, &visit);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                let (first, last) = (w * per + 1, ((w + 1) * per).min(total));
                s.spawn(move || -> cbformat::Result<T> {
                    let mut acc = init();
                    let mut next = first;
                    while next <= last {
                        let upto = last.min(next + u64::from(CHUNK) - 1);
                        let records = db.records(next as u32, upto as u32)?;
                        let Some(end) = records.last().map(|r| u64::from(r.id())) else { break };
                        for r in &records {
                            visit(&mut acc, r);
                        }
                        next = end + 1;
                    }
                    Ok(acc)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))).collect()
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

/// A comparison of fixed-width text (ECO codes, dates) in byte order. A bound
/// followed by `~` sorts after every text that starts with it, which turns a
/// prefix into the upper end of a range.
enum TextCmp {
    Prefix(Vec<u8>),
    AtLeast(Vec<u8>),
    AtMost(Vec<u8>),
    Above(Vec<u8>),
    Below(Vec<u8>),
}

enum IntCmp {
    Range(i64, i64),
    Never,
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
    pub fn new(query: &Query, tables: &Tables<'_>) -> Matcher {
        let set = |t: Option<&NameTable>, needle: &str| t.map_or_else(|| BitSet::new(0), |t| t.containing(needle));
        let names = |needle: &str| set(tables.players, needle);
        let events = |needle: &str| set(tables.tournaments, needle);
        let titles = |needle: &str| set(tables.titles, needle);
        let terms = query
            .terms
            .iter()
            .map(|term| CompiledTerm {
                negated: term.negated,
                tests: term
                    .values
                    .iter()
                    .map(|v| {
                        let needle = v.text.to_lowercase();
                        match term.field {
                            Field::Text => Test::Text {
                                players: names(&needle),
                                tournaments: events(&needle),
                                titles: titles(&needle),
                            },
                            Field::White => Test::Players(names(&needle), Role::White),
                            Field::Black => Test::Players(names(&needle), Role::Black),
                            Field::Player => Test::Players(names(&needle), Role::Either),
                            Field::Annotator => Test::Players(names(&needle), Role::Annotator),
                            Field::Event => Test::Event { tournaments: events(&needle), titles: titles(&needle) },
                            Field::Result => Test::Result(v.text.clone()),
                            Field::Round => Test::Round(needle),
                            Field::Eco => Test::Eco(text_cmps(v.text.as_bytes(), v)),
                            Field::Date => date_test(v),
                            Field::Moves => Test::Moves(int_cmp(v)),
                            Field::Elo => Test::Elo(int_cmp(v)),
                        }
                    })
                    .collect(),
            })
            .collect();
        let games_only = query.terms.iter().any(|t| !shared(t.field));
        Matcher { terms, games_only }
    }

    pub fn matches(&self, r: &Record) -> bool {
        let other = other(r);
        if other.is_some() && self.games_only {
            return false;
        }
        self.terms.iter().all(|t| t.tests.iter().any(|test| test.holds(r, other.as_ref())) != t.negated)
    }
}

fn int_cmp(v: &Value) -> IntCmp {
    let parse = |s: &str| s.trim().parse::<i64>().ok();
    let Some(low) = parse(&v.text) else { return IntCmp::Never };
    match v.cmp {
        Cmp::Equal => IntCmp::Range(low, low),
        Cmp::Greater => IntCmp::Range(low.saturating_add(1), i64::MAX),
        Cmp::GreaterOrEqual => IntCmp::Range(low, i64::MAX),
        Cmp::Less => IntCmp::Range(i64::MIN, low.saturating_sub(1)),
        Cmp::LessOrEqual => IntCmp::Range(i64::MIN, low),
        Cmp::Range => match v.upper.as_deref().and_then(parse) {
            Some(high) => IntCmp::Range(low, high),
            None => IntCmp::Never,
        },
    }
}

fn with_tilde(bound: &[u8]) -> Vec<u8> {
    let mut v = bound.to_vec();
    v.push(b'~');
    v
}

/// The comparisons `v` asks for, on `low` and, for a range, `v.upper`.
fn text_cmps(low: &[u8], v: &Value) -> Vec<TextCmp> {
    let high = v.upper.as_deref().map_or(low, str::as_bytes);
    match v.cmp {
        Cmp::Equal => vec![TextCmp::Prefix(low.to_vec())],
        Cmp::GreaterOrEqual => vec![TextCmp::AtLeast(low.to_vec())],
        Cmp::Greater => vec![TextCmp::Above(with_tilde(low))],
        Cmp::Less => vec![TextCmp::Below(low.to_vec())],
        Cmp::LessOrEqual => vec![TextCmp::AtMost(with_tilde(low))],
        Cmp::Range => vec![TextCmp::AtLeast(low.to_vec()), TextCmp::AtMost(with_tilde(high))],
    }
}

/// A typed date in the stored `YYYY.MM.DD` prefix form: `2024`, `2024-3` →
/// `2024.03`, `2024/03/15` → `2024.03.15`; `None` when it is not a date.
pub fn normalize_date(text: &str) -> Option<String> {
    let mut parts = text.trim().split(['-', '.', '/']);
    let year = parts.next().filter(|y| y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()))?;
    let mut out = year.to_string();
    for part in parts.by_ref().take(2) {
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out.push('.');
        out.push_str(&format!("{part:0>2}"));
    }
    parts.next().is_none().then_some(out)
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

impl TextCmp {
    fn holds(&self, s: &[u8]) -> bool {
        match self {
            TextCmp::Prefix(p) => s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p),
            TextCmp::AtLeast(b) => s >= &b[..],
            TextCmp::AtMost(b) => s <= &b[..],
            TextCmp::Above(b) => s > &b[..],
            TextCmp::Below(b) => s < &b[..],
        }
    }
}

impl IntCmp {
    fn holds(&self, v: i64) -> bool {
        match *self {
            IntCmp::Range(low, high) => (low..=high).contains(&v),
            IntCmp::Never => false,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_normalise_as_typed() {
        assert_eq!(normalize_date("2024").as_deref(), Some("2024"));
        assert_eq!(normalize_date("2024-3").as_deref(), Some("2024.03"));
        assert_eq!(normalize_date("2024/03/5").as_deref(), Some("2024.03.05"));
        for bad in ["24", "2024-", "2024-123", "2024-01-02-03", "x", "2024-1a"] {
            assert_eq!(normalize_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn text_comparisons_with_the_tilde_bound() {
        let v = |cmp, upper: Option<&str>| Value { text: "B9".into(), cmp, upper: upper.map(Into::into) };
        let all = |cmps: Vec<TextCmp>, s: &str| cmps.iter().all(|c| c.holds(s.as_bytes()));
        assert!(all(text_cmps(b"B9", &v(Cmp::Equal, None)), "B90"));
        assert!(!all(text_cmps(b"B9", &v(Cmp::Equal, None)), "B80"));
        assert!(all(text_cmps(b"B9", &v(Cmp::LessOrEqual, None)), "B99"));
        assert!(!all(text_cmps(b"B9", &v(Cmp::Greater, None)), "B99"));
        assert!(all(text_cmps(b"B9", &v(Cmp::Greater, None)), "C00"));
        assert!(all(text_cmps(b"B9", &v(Cmp::Range, Some("C1"))), "C19"));
        assert!(!all(text_cmps(b"B9", &v(Cmp::Range, Some("C1"))), "C20"));
    }
}
