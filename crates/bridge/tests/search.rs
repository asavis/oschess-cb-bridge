//! The conformance corpus of `docs/search-grammar.md`, run against the fixture
//! that document defines. Both are read from the document itself, so the two
//! cannot drift apart.

use std::collections::HashMap;

use bridge::search::{self, Indexes, SearchError, Selection, SuggestField};
use bridge::store::Any;
use cbformat::cbh;
use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;

mod common;
use common::{DOC, block, block_in, classic_fixture, fixture, lid, put};

#[test]
fn blocks_read_the_same_with_crlf_line_ends() {
    let crlf = DOC.replace('\n', "\r\n");
    for tag in ["fixture", "corpus"] {
        assert_eq!(block_in(&crlf, tag), block(tag), "{tag}");
    }
}

fn numbers<'a>(db: impl Into<Any<'a>>, idx: &Indexes, q: &str) -> Result<Vec<u32>, String> {
    let db = db.into();
    match search::select(db, idx, Some(q), None, None) {
        Ok((Selection::All { descending }, _)) => {
            let all = 1..=db.record_count();
            Ok(if descending { all.rev().collect() } else { all.collect() })
        }
        Ok((Selection::Numbers(v), _)) => Ok(v.to_vec()),
        Err(SearchError::Unsupported(q)) => Err(q),
        Err(e) => panic!("{e:?}"),
    }
}

/// The lines of the corpus whose result on `db` is not the one written.
fn corpus_failures<'a>(db: impl Into<Any<'a>>, idx: &Indexes) -> Vec<String> {
    let db = db.into();
    let lines = block("corpus");
    assert!(lines.len() > 50, "the corpus was read");
    let mut failures = Vec::new();
    for line in lines {
        let (q, want) = line.rsplit_once("=>").unwrap();
        let (q, want) = (q.trim(), want.trim());
        let got = match numbers(db, idx, q) {
            Ok(v) if v.is_empty() => "none".to_string(),
            Ok(v) => v.iter().map(u32::to_string).collect::<Vec<_>>().join(" "),
            Err(qualifier) => format!("unsupported {qualifier}"),
        };
        if got != want {
            failures.push(format!("{q:?}: want {want}, got {got}"));
        }
    }
    failures
}

#[test]
fn the_conformance_corpus_holds() {
    let f = fixture("search-corpus", &[]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let failures = corpus_failures(&db, &Indexes::default());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The same records written in the classic format give the same results:
/// its guiding text keeps its title in its text record, and its annotators
/// are a table of their own.
#[test]
fn the_conformance_corpus_holds_on_a_classic_copy() {
    let f = classic_fixture("search-corpus-classic", &[]);
    let db = cbh::Database::open(f.dir().join("db.cbh")).unwrap();
    let failures = corpus_failures(&db, &Indexes::default());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn results_are_cached_and_the_url_sort_wins() {
    let f = fixture("search-cache", &[]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let first = search::select(&db, &idx, Some("player:morphy sort:white"), None, None).ok().unwrap();
    let again = search::select(&db, &idx, Some("player:morphy sort:white"), None, None).ok().unwrap();
    let (Selection::Numbers(a), Selection::Numbers(b)) = (first.0, again.0) else { panic!("numbers expected") };
    assert!(std::sync::Arc::ptr_eq(&a, &b), "the second request reuses the first result");
    // The URL's sort wins over the query's token.
    let by_param =
        search::select(&db, &idx, Some("player:morphy sort:white"), None, search::query::Sort::parse("number-desc"));
    let Ok((Selection::Numbers(v), sort)) = by_param else { panic!("numbers expected") };
    assert_eq!((v.as_slice(), sort.name().as_str()), (&[9, 3, 2, 1][..], "number-desc"));
}

#[test]
fn suggestions_by_prefix_and_count() {
    let f = fixture("search-suggest", &[]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let s = |field, prefix: &str| -> HashMap<String, u32> {
        suggested(&db, &idx, field, prefix, 20).unwrap().into_iter().collect()
    };
    let ordered = |field, prefix: &str| -> Vec<(String, u32)> { suggested(&db, &idx, field, prefix, 20).unwrap() };
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
    assert_eq!(suggested(&db, &idx, SuggestField::Event, "", 1).unwrap().len(), 1, "limit applies");
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
    let annotators = suggested(&db, &idx, SuggestField::Annotator, "t", 20).unwrap();
    assert_eq!(annotators, [("Tal, Mikhail".to_string(), 1)]);
    assert!(suggested(&db, &idx, SuggestField::Event, "aaa", 20).unwrap().is_empty());
}

/// Suggestions as (name, games) pairs.
fn suggested<'a>(
    db: impl Into<Any<'a>>,
    idx: &Indexes,
    field: SuggestField,
    prefix: &str,
    limit: usize,
) -> Result<Vec<(String, u32)>, SearchError> {
    search::suggest(db, idx, field, prefix, limit).map(|list| list.iter().map(|s| (s.name.clone(), s.games)).collect())
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
    assert_eq!(suggested(&db, &idx, SuggestField::Player, "éé", 20).unwrap(), [(a, 1), (b, 1)]);
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
    assert_eq!(suggested(&db, &idx, SuggestField::Player, "same", 20).unwrap(), [("Same, Person".into(), 1)]);
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
    assert!(matches!(
        search::select(&db, &idx, None, None, search::query::Sort::parse("date")),
        Err(SearchError::TooLarge)
    ));
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
        let r = search::select(&db1, &idx1, Some("needle"), Some("tab"), None);
        (r.map(|_| ()), std::time::Instant::now())
    });
    while idx.scanned() == 0 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let second = search::select(&db, &idx, Some("other"), Some("tab"), None);
    let second_done = std::time::Instant::now();
    let (first, first_done) = first.join().unwrap();
    assert!(matches!(first, Err(SearchError::Superseded)), "{first:?}");
    assert!(matches!(second, Ok((Selection::Numbers(ref v), _)) if v.is_empty()));
    assert!(first_done <= second_done, "the first search stopped before the second finished");
    let first_read = idx.scanned() - RECORDS;
    assert!(first_read < RECORDS / 2, "the first search read {first_read} of {RECORDS} records");
}

/// Names sort ignoring case only: `alpha` and `ALPHA` are one key, so their
/// games stay in number order in both directions.
#[test]
fn names_sort_ignoring_case() {
    let white = |id: i64| move |r: &mut [u8; 192]| (set_i64(r, 0x18, id), set_i64(r, 0x20, -1)).1;
    let (alpha, upper, zulu) = (white(0), white(1), white(2));
    let f = raw_db("search-case", &["alpha", "ALPHA", "zulu"], &[&zulu, &alpha, &upper]);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    assert_eq!(sorted(&db, &idx, "white"), [2, 3, 1]);
    assert_eq!(sorted(&db, &idx, "white-desc"), [1, 2, 3]);
    // Suggestions still tell the two spellings apart.
    let s = suggested(&db, &idx, SuggestField::Player, "alp", 20).unwrap();
    assert_eq!(s, [("ALPHA".into(), 1), ("alpha".into(), 1)]);
}

/// Of many matching names only the best `limit` come back, in order: most
/// games first, then by name.
#[test]
fn suggestions_keep_the_best_of_many() {
    const NAMES: usize = 20_000;
    let names: Vec<String> = (0..NAMES).map(|i| format!("a{i:06}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    // One game each, a second game for every 1,000th name.
    let player = |i: usize| move |r: &mut [u8; 192]| (set_i64(r, 0x18, i as i64), set_i64(r, 0x20, i as i64)).1;
    let edits: Vec<_> = (0..NAMES).chain((999..NAMES).step_by(1000)).map(player).collect();
    let edits: Vec<Edit<'_>> = edits.iter().map(|e| e as Edit<'_>).collect();
    let f = raw_db("search-top", &refs, &edits);
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::default();
    let s = suggested(&db, &idx, SuggestField::Player, "a", 20).unwrap();
    let want: Vec<(String, u32)> = (999..NAMES)
        .step_by(1000)
        .map(|i| (format!("a{i:06}"), 2))
        .chain((0..).filter(|i| i % 1000 != 999).map(|i| (format!("a{i:06}"), 1)))
        .take(20)
        .collect();
    assert_eq!(s, want);
    assert_eq!(suggested(&db, &idx, SuggestField::Player, "a01", 3).unwrap().len(), 3);
}

/// A search started without a stream is never superseded; one in a stream
/// is, by any later request with `q` in the same stream, an empty one too.
#[cfg(unix)]
#[test]
fn only_the_same_stream_supersedes() {
    let f = sparse("search-streams", 100_000);
    let db = std::sync::Arc::new(Database::open(f.dir().join("db.2cbh")).unwrap());
    let idx = std::sync::Arc::new(Indexes::default());
    let held = idx.gate().hold(2);
    let run = |q: &'static str, stream: Option<&'static str>| {
        let (db1, idx1) = (db.clone(), idx.clone());
        std::thread::spawn(move || search::select(&db1, &idx1, Some(q), stream, None).map(|_| ()))
    };
    let named = run("needle", Some("tab"));
    assert!(held.arrived(1, std::time::Duration::from_secs(30)));
    let unnamed = run("pin", None);
    assert!(held.arrived(2, std::time::Duration::from_secs(30)));
    assert!(search::select(&db, &idx, Some(""), Some("tab"), None).is_ok(), "an empty q");
    drop(held);
    assert!(matches!(named.join().unwrap(), Err(SearchError::Superseded)));
    assert!(unnamed.join().unwrap().is_ok());
}

/// Suggestions, name searches and name sorts agree between a 2CBH and a
/// classic copy of the fixture with more guiding texts, whose titles and
/// authors the two formats keep in different places.
#[test]
fn classic_and_2cbh_copies_agree() {
    let extra = [
        "11 | text | - | - | Aaa survey | ????.??.?? | - | * | - | 0 | 0 | 0 | Steinitz, Wilhelm",
        "12 | game | Tal, Mikhail | Morphy, Paul | Aaa survey | 1960.05.07 | 3 | 1-0 | A00 | 12 | 2700 | 0 | Tal, Mikhail",
        "13 | text | - | - | Zugzwang | ????.??.?? | - | * | - | 0 | 0 | 0 | -",
    ];
    let (f2, fc) = (fixture("search-pair-2cbh", &extra), classic_fixture("search-pair-cbh", &extra));
    let two = Database::open(f2.dir().join("db.2cbh")).unwrap();
    let classic = cbh::Database::open(fc.dir().join("db.cbh")).unwrap();
    let (i2, ic) = (Indexes::default(), Indexes::default());
    for field in [SuggestField::Player, SuggestField::Event, SuggestField::Annotator] {
        for prefix in ["", "a", "c", "l", "m", "mik", "n", "p", "r", "s", "st", "t", "w", "z"] {
            let (a, b) = (suggested(&two, &i2, field, prefix, 20), suggested(&classic, &ic, field, prefix, 20));
            assert_eq!(a.unwrap(), b.unwrap(), "{field:?} {prefix:?}");
        }
    }
    let queries = [
        "survey",
        "zugzwang",
        "event:survey",
        "annotator:steinitz",
        "annotator:tal",
        "steinitz",
        "tal",
        "-annotator:nimzo",
        "result:1-0 tal",
        "sort:tournament",
        "sort:tournament-desc",
        "sort:annotator",
        "sort:annotator-desc",
        "sort:white",
        "sort:black-desc",
        "sort:date",
        "sort:round",
        "sort:eco-desc",
        "player:tal sort:tournament",
    ];
    for q in queries {
        assert_eq!(numbers(&two, &i2, q), numbers(&classic, &ic, q), "{q}");
    }
    assert_eq!(numbers(&classic, &ic, "event:survey").unwrap(), [11, 12]);
    assert_eq!(numbers(&classic, &ic, "annotator:steinitz").unwrap(), [11]);
    assert_eq!(numbers(&classic, &ic, "sort:tournament").unwrap()[..3], [11, 12, 8], "titles sort among tournaments");
    assert_eq!(suggested(&classic, &ic, SuggestField::Annotator, "t", 20).unwrap(), [("Tal, Mikhail".into(), 2)]);
}
