//! The conformance corpus of `docs/search-grammar.md`, run against the fixture
//! that document defines. Both are read from the document itself, so the two
//! cannot drift apart.

use std::collections::HashMap;

use bridge::search::{self, Indexes, SearchError, Selection, SuggestField};
use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;

const DOC: &str = include_str!("../../../docs/search-grammar.md");

/// The non-blank lines of the fenced block tagged `tag`.
fn block(tag: &str) -> Vec<&'static str> {
    let start = DOC.find(&format!("```{tag}\n")).unwrap_or_else(|| panic!("no {tag} block")) + tag.len() + 4;
    let end = start + DOC[start..].find("```").unwrap();
    DOC[start..end].lines().filter(|l| !l.trim().is_empty()).collect()
}

/// Entity ids by name, in order of first use; id 0 is the empty name.
#[derive(Default)]
struct Names(Vec<String>);

impl Names {
    fn id(&mut self, name: &str) -> i64 {
        if name == "-" {
            return 0;
        }
        if self.0.is_empty() {
            self.0.push(String::new());
        }
        let at = self.0.iter().position(|n| n == name).unwrap_or_else(|| {
            self.0.push(name.to_string());
            self.0.len() - 1
        });
        at as i64
    }
}

fn container(size: usize, record: &[u8]) -> Vec<u8> {
    if record.is_empty() {
        return vec![0; size];
    }
    let mut c = (record.len() as i32).to_le_bytes().to_vec();
    c.extend(record);
    assert!(c.len() <= size);
    c.resize(size, 0);
    c
}

fn string(s: &str) -> Vec<u8> {
    let mut v = (s.len() as i32).to_le_bytes().to_vec();
    v.extend(s.as_bytes());
    v
}

/// A `.2lid` with the six entity types, holding players (type 0), tournaments
/// (type 1) and the game tags that carry titles (type 5).
fn lid(players: &[String], tournaments: &[String], titles: &[String]) -> Vec<u8> {
    let tables: [Vec<Vec<u8>>; 6] = [
        players
            .iter()
            .map(|name| {
                let (last, first) = name.split_once(", ").unwrap_or((name, ""));
                [string(last), string(first)].concat()
            })
            .collect(),
        tournaments.iter().map(|t| [string(""), string(t), vec![0; 4]].concat()).collect(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        // One title, in language 0.
        titles.iter().map(|t| [1i32.to_le_bytes().to_vec(), 0i32.to_le_bytes().to_vec(), string(t)].concat()).collect(),
    ];
    let count = tables.iter().map(Vec::len).max().unwrap_or(0).max(1);
    // Containers big enough for the longest record.
    let size = tables.iter().flatten().map(|r| r.len() + 4).max().unwrap_or(0).max(64).next_multiple_of(8);
    let mut d = Vec::new();
    d.extend(184i32.to_be_bytes());
    d.extend(6i32.to_be_bytes());
    for _ in &tables {
        d.extend((size as i32).to_be_bytes());
        d.extend((count as i64).to_be_bytes());
        d.extend((-1i64).to_be_bytes());
    }
    d.resize(184, 0);
    for id in 0..count {
        for table in &tables {
            d.extend(container(size, table.get(id).map_or(&[][..], Vec::as_slice)));
        }
    }
    d
}

fn put(rec: &mut [u8; 192], at: usize, bytes: &[u8]) {
    rec[at..at + bytes.len()].copy_from_slice(bytes);
}

/// The fixture of the document, written to a temporary directory, plus `extra`
/// rows in the same form. A guiding text or an analysis takes its title from
/// the event column and its author from the annotator column, and stores them
/// in its own header layout.
fn fixture(name: &str, extra: &[&str]) -> TempDb {
    let (mut players, mut tournaments, mut titles) = (Names::default(), Names::default(), Names::default());
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    let rows = block("fixture").into_iter().filter(|l| !l.starts_with('#')).chain(extra.iter().copied());
    for (i, line) in rows.enumerate() {
        let f: Vec<&str> = line.split('|').map(str::trim).collect();
        assert_eq!(f[0].parse::<usize>().unwrap(), i + 1, "fixture rows are numbered in order");
        let rec = b.game(e4);
        let (white, black, annotator) = (players.id(f[2]), players.id(f[3]), players.id(f[12]));
        let ids: &[(usize, i64)] = match f[1] {
            "text" => {
                rec[0] |= 2;
                &[(0x20, annotator), (0x28, titles.id(f[4]))]
            }
            "analysis" => {
                rec[2] = 2;
                &[(0x18, titles.id(f[4])), (0x28, annotator)]
            }
            kind => {
                if kind == "deleted" {
                    rec[0] |= 0x80;
                }
                &[(0x18, white), (0x20, black), (0x28, tournaments.id(f[4])), (0x30, annotator)]
            }
        };
        for &(at, id) in ids {
            put(rec, at, &id.to_le_bytes());
        }
        if f[1] == "text" || f[1] == "analysis" {
            continue;
        }
        let date: Vec<i32> = f[5].split('.').map(|p| p.parse().unwrap_or(0)).collect();
        put(rec, 0xbc, &((date[0] << 9) | (date[1] << 5) | date[2]).to_le_bytes());
        let (round, sub): (i16, i16) = match f[6] {
            "-" => (0, 0),
            r => match r.split_once('(') {
                Some((r, s)) => (r.parse().unwrap(), s.trim_end_matches(')').parse().unwrap()),
                None => (r.parse().unwrap(), 0),
            },
        };
        put(rec, 0x5a, &round.to_le_bytes());
        put(rec, 0x5c, &sub.to_le_bytes());
        rec[0x58] = match f[7] {
            "0-1" => 0,
            "1/2-1/2" => 1,
            "1-0" => 2,
            _ => 3,
        };
        let eco = match f[8].as_bytes() {
            [l, d1, d2] => (u16::from(l - b'A') * 100 + u16::from(d1 - b'0') * 10 + u16::from(d2 - b'0') + 1) * 128,
            _ => 0,
        };
        put(rec, 0x80, &eco.to_le_bytes());
        put(rec, 0x8a, &f[9].parse::<i16>().unwrap().to_le_bytes());
        put(rec, 0x60, &f[10].parse::<i16>().unwrap().to_le_bytes());
        put(rec, 0x70, &f[11].parse::<i16>().unwrap().to_le_bytes());
    }
    b.lid(lid(&players.0, &tournaments.0, &titles.0));
    b.write(name)
}

fn numbers(db: &Database, idx: &Indexes, q: &str) -> Result<Vec<u32>, String> {
    match search::select(db, idx, q, None) {
        Ok((Selection::All { descending }, _)) => {
            let all = 1..=db.record_count();
            Ok(if descending { all.rev().collect() } else { all.collect() })
        }
        Ok((Selection::Numbers(v), _)) => Ok(v.to_vec()),
        Err(SearchError::Unsupported(q)) => Err(q),
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn the_conformance_corpus_holds() {
    let f = fixture("search-corpus", &[]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let lines = block("corpus");
    assert!(lines.len() > 50, "the corpus was read");
    let mut failures = Vec::new();
    for line in lines {
        let (q, want) = line.rsplit_once("=>").unwrap();
        let (q, want) = (q.trim(), want.trim());
        let got = match numbers(&db, &idx, q) {
            Ok(v) if v.is_empty() => "none".to_string(),
            Ok(v) => v.iter().map(u32::to_string).collect::<Vec<_>>().join(" "),
            Err(qualifier) => format!("unsupported {qualifier}"),
        };
        if got != want {
            failures.push(format!("{q:?}: want {want}, got {got}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn results_are_cached_and_the_url_sort_wins() {
    let f = fixture("search-cache", &[]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let first = search::select(&db, &idx, "player:morphy sort:white", None).ok().unwrap();
    let again = search::select(&db, &idx, "player:morphy sort:white", None).ok().unwrap();
    let (Selection::Numbers(a), Selection::Numbers(b)) = (first.0, again.0) else { panic!("numbers expected") };
    assert!(std::sync::Arc::ptr_eq(&a, &b), "the second request reuses the first result");
    // The URL's sort wins over the query's token.
    let by_param = search::select(&db, &idx, "player:morphy sort:white", search::query::Sort::parse("number-desc"));
    let Ok((Selection::Numbers(v), sort)) = by_param else { panic!("numbers expected") };
    assert_eq!((v.as_slice(), sort.name().as_str()), (&[9, 3, 2, 1][..], "number-desc"));
}

#[test]
fn suggestions_by_prefix_and_count() {
    let f = fixture("search-suggest", &[]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let s = |field, prefix: &str| -> HashMap<String, u32> {
        search::suggest(&db, &idx, field, prefix, 20).unwrap().into_iter().collect()
    };
    let ordered =
        |field, prefix: &str| -> Vec<(String, u32)> { search::suggest(&db, &idx, field, prefix, 20).unwrap() };
    assert_eq!(ordered(SuggestField::Player, "m"), [("Morphy, Paul".to_string(), 4), ("Tal, Mikhail".to_string(), 2)]);
    assert_eq!(ordered(SuggestField::Player, "LA"), [("Lasker, Emanuel".to_string(), 4)]);
    assert_eq!(s(SuggestField::Player, "mik").get("Tal, Mikhail"), Some(&2));
    // Nimzowitsch annotated two games but played none.
    assert!(!s(SuggestField::Player, "n").contains_key("Nimzowitsch, Aron"));
    assert_eq!(ordered(SuggestField::Annotator, "n"), [("Nimzowitsch, Aron".to_string(), 2)]);
    assert_eq!(ordered(SuggestField::Annotator, "t"), [("Tal, Mikhail".to_string(), 1)]);
    // The guiding text in London is not counted.
    assert_eq!(ordered(SuggestField::Event, "l"), [("London".to_string(), 1)]);
    assert_eq!(ordered(SuggestField::Event, "st"), [("St Petersburg".to_string(), 3)]);
    let all = ordered(SuggestField::Player, "a");
    assert_eq!(all, [("Anderssen, Adolf".to_string(), 3)]);
    assert_eq!(search::suggest(&db, &idx, SuggestField::Event, "", 1).unwrap().len(), 1, "limit applies");
}

/// Guiding texts and analyses have header layouts of their own: they are found
/// and sorted by their title and author, and any field only games have leaves
/// them out.
#[test]
fn texts_and_analyses_by_their_own_layout() {
    let extra = [
        "11 | analysis | - | - | Morphy's openings | ????.??.?? | - | * | - | 0 | 0 | 0 | Tal, Mikhail",
        "12 | text     | - | - | Aaa survey        | ????.??.?? | - | * | - | 0 | 0 | 0 | Steinitz, Wilhelm",
    ];
    let f = fixture("search-other", &extra);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let q = |text: &str| numbers(&db, &idx, text).unwrap();
    assert_eq!(q("openings"), [11], "a word finds the title");
    assert_eq!(q("morphy"), [1, 2, 3, 9, 11], "and a title that names a player");
    assert_eq!(q("event:survey"), [12]);
    assert_eq!(q("annotator:tal"), [10, 11], "the author is the annotator");
    assert_eq!(q("steinitz"), [4, 9, 12], "a word finds the author");
    assert_eq!(q("player:tal"), [7, 10], "players are game fields");
    assert_eq!(q("-result:1-0 steinitz"), [4], "a game-only term leaves them out, negated too");
    assert_eq!(q("sort:tournament")[..3], [12, 8, 9], "titles sort among tournaments");
    assert_eq!(q("sort:annotator")[7..], [3, 5, 12, 10, 11], "authors sort as annotators");
    assert_eq!(q("sort:white")[..3], [8, 11, 12], "no other key: first ascending, by number");
    // Neither is counted for suggestions.
    let annotators = search::suggest(&db, &idx, SuggestField::Annotator, "t", 20).unwrap();
    assert_eq!(annotators, [("Tal, Mikhail".to_string(), 1)]);
    assert!(search::suggest(&db, &idx, SuggestField::Event, "aaa", 20).unwrap().is_empty());
}

type Edit<'a> = &'a dyn Fn(&mut [u8; 192]);

/// A database of games sharing one move record, each header set by its edit,
/// with `players` as player entities 0, 1, … and no tournaments.
fn raw_db(name: &str, players: &[&str], edits: &[Edit<'_>]) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for edit in edits {
        edit(b.game(e4));
    }
    let players: Vec<String> = players.iter().map(|p| p.to_string()).collect();
    b.lid(lid(&players, &[], &[]));
    b.write(name)
}

fn set_i64(rec: &mut [u8; 192], at: usize, v: i64) {
    put(rec, at, &v.to_le_bytes());
}

fn sorted(db: &Database, idx: &Indexes, sort: &str) -> Vec<u32> {
    numbers(db, idx, &format!("sort:{sort}")).unwrap()
}

/// Names are matched and told apart in full, however long: two names that
/// share 140 bytes and differ at the end are both found and both suggested.
#[test]
fn long_names_are_matched_and_suggested_in_full() {
    let (a, b) = (format!("{}SuffixA", "é".repeat(70)), format!("{}SuffixB", "é".repeat(70)));
    let f = raw_db(
        "search-long-names",
        &[&a, &b],
        &[&|r| (set_i64(r, 0x18, 0), set_i64(r, 0x20, 0)).1, &|r| (set_i64(r, 0x18, 1), set_i64(r, 0x20, 1)).1],
    );
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    assert_eq!(numbers(&db, &idx, "player:suffixa").unwrap(), [1]);
    assert_eq!(numbers(&db, &idx, "player:SuffixB").unwrap(), [2]);
    assert_eq!(search::suggest(&db, &idx, SuggestField::Player, "éé", 20).unwrap(), [(a, 1), (b, 1)]);
}

/// ECO codes sort as shown: ChessBase's hidden sub-code does not order two
/// games with the same code, which stay in number order both ways.
#[test]
fn equal_eco_codes_keep_number_order() {
    let eco = |v: u16| move |r: &mut [u8; 192]| put(r, 0x80, &v.to_le_bytes());
    let (b52_2, b52_1, a00) = (eco(153 * 128 + 2), eco(153 * 128 + 1), eco(128));
    let f = raw_db("search-eco-sub", &["x"], &[&b52_2, &b52_1, &a00]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    assert_eq!(numbers(&db, &idx, "eco:B52").unwrap(), [1, 2]);
    assert_eq!(sorted(&db, &idx, "eco"), [3, 1, 2]);
    assert_eq!(sorted(&db, &idx, "eco-desc"), [1, 2, 3]);
}

/// An empty name, a missing entity and a record without the field all sort as
/// the same empty key, in number order.
#[test]
fn empty_and_missing_names_share_a_rank() {
    let empty = |r: &mut [u8; 192]| set_i64(r, 0x18, 0);
    let missing = |r: &mut [u8; 192]| set_i64(r, 0x18, -1);
    let named = |r: &mut [u8; 192]| set_i64(r, 0x18, 1);
    let text = |r: &mut [u8; 192]| r[0] |= 2;
    let f = raw_db("search-empty-names", &["", "Zed"], &[&empty, &missing, &named, &text]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    assert_eq!(sorted(&db, &idx, "white"), [1, 2, 4, 3]);
    assert_eq!(sorted(&db, &idx, "white-desc"), [3, 1, 2, 4]);
}

/// Two entities with one name are one suggestion, and a game that has that
/// name on both sides counts once.
#[test]
fn a_game_counts_once_per_name() {
    let both = |r: &mut [u8; 192]| (set_i64(r, 0x18, 0), set_i64(r, 0x20, 1)).1;
    let f = raw_db("search-same-name", &["Same, Person", "Same, Person"], &[&both]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    assert_eq!(numbers(&db, &idx, "player:same").unwrap(), [1]);
    assert_eq!(search::suggest(&db, &idx, SuggestField::Player, "same", 20).unwrap(), [("Same, Person".into(), 1)]);
}

/// A database of `records` headers of which only the first is written: the
/// rest of the header file is a hole, read as zeros, which needs a file system
/// with sparse files.
#[cfg(unix)]
fn sparse(name: &str, records: u64) -> TempDb {
    let f = raw_db(name, &["x"], &[&|_| {}]);
    let file = std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2cbh")).unwrap();
    file.set_len((records + 1) * 192).unwrap();
    f
}

/// A sort order that could never fit in the memory budget is refused before
/// anything is allocated: 200 million records need 2.4 GB to sort.
#[cfg(unix)]
#[test]
fn a_sort_that_cannot_fit_is_refused_up_front() {
    let f = sparse("search-too-large", 200_000_000);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let started = std::time::Instant::now();
    assert!(matches!(search::select(&db, &idx, "", search::query::Sort::parse("date")), Err(SearchError::TooLarge)));
    assert_eq!(idx.scanned(), 0, "not a record was read");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// A newer search on the same database stops the one still scanning: the
/// first answers `Superseded` before the second is done, having read only
/// a part of the database.
#[cfg(unix)]
#[test]
fn a_newer_search_stops_the_older_one() {
    const RECORDS: u64 = 8_000_000;
    let f = sparse("search-superseded", RECORDS);
    let db = std::sync::Arc::new(Database::open(f.dir().join("db.2cbh")).unwrap());
    let idx = std::sync::Arc::new(Indexes::default());
    let (db1, idx1) = (db.clone(), idx.clone());
    let first = std::thread::spawn(move || {
        let r = search::select(&db1, &idx1, "needle", None);
        (r.map(|_| ()), std::time::Instant::now())
    });
    while idx.scanned() == 0 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let second = search::select(&db, &idx, "other", None);
    let second_done = std::time::Instant::now();
    let (first, first_done) = first.join().unwrap();
    assert!(matches!(first, Err(SearchError::Superseded)), "{first:?}");
    assert!(matches!(second, Ok((Selection::Numbers(ref v), _)) if v.is_empty()));
    assert!(first_done <= second_done, "the first search stopped before the second finished");
    let first_read = idx.scanned() - RECORDS;
    assert!(first_read < RECORDS / 2, "the first search read {first_read} of {RECORDS} records");
}
