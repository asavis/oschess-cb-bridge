//! The heads file (#106): a pass over its rows answers every search, sort and
//! suggestion as a pass over the database's own records does, and a file that
//! is damaged, of another generation or of another size is never believed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bridge::catalog::{Catalog, Opened};
use bridge::search::heads::{self, BLOCK_ROWS, Built, Heads, ROWS_READ};
use bridge::search::{self, Indexes, SearchError, Selection, SuggestField};
use cbformat::codepage::CodePage;
use cbformat::pgnfile;
use cbformat::view::Base;

mod common;
use common::{block, classic_fixture, fixture_of, pgn_fixture, rows};

const ID: &str = "0123456789abcdef";

/// The document's fixture, then `n` more records, with players, events,
/// dates, rounds, results, codes and ratings varying among them; every 97th
/// record from the first is one of `kinds` in turn, the rest games.
fn many(n: usize, kinds: &[&str]) -> Vec<String> {
    let mut out = rows(&[]);
    let base = out.len();
    let players = ["Kasparov, Garry", "Karpov, Anatoly", "Carlsen, Magnus", "Anand, Viswanathan", "Tal, Mikhail"];
    let events = ["Linares", "Wijk aan Zee", "Moscow ch", "Dortmund"];
    let results = ["1-0", "0-1", "1/2-1/2", "*"];
    for i in 0..n {
        let number = base + i + 1;
        let kind = match i % 97 {
            k if k < kinds.len() => kinds[k],
            _ => "game",
        };
        let (white, black) = (players[i % 5], players[(i * 3 + 1) % 5]);
        let round = match i % 7 {
            0 => "-".to_string(),
            1 => format!("{}({})", i % 13 + 1, i % 3 + 1),
            _ => (i % 13 + 1).to_string(),
        };
        let eco = match i % 11 {
            0 => "-".to_string(),
            k => format!("{}{:02}", ["A", "B", "C", "D", "E"][k % 5], i % 100),
        };
        out.push(format!(
            "{number} | {kind} | {white} | {black} | {} | {}.{:02}.{:02} | {round} | {} | {eco} | {} | {} | {} | {}",
            events[i % 4],
            1950 + i % 70,
            i % 12 + 1,
            i % 28 + 1,
            results[i % 4],
            i % 90,
            if i % 5 == 0 { 0 } else { 2200 + i % 600 },
            if i % 6 == 0 { 0 } else { 2150 + i % 650 },
            if i % 9 == 0 { players[(i + 2) % 5] } else { "-" },
        ));
    }
    out
}

/// Every answer the database gives: the corpus's queries, sorts and suggestions.
fn answers(db: &Base, idx: &Indexes) -> Vec<String> {
    let mut out = Vec::new();
    let queries = block("corpus").into_iter().map(|l| l.rsplit_once("=>").unwrap().0.trim().to_string());
    let sorts = [
        "white",
        "black-desc",
        "whiteElo",
        "blackElo-desc",
        "tournament",
        "annotator-desc",
        "date",
        "round",
        "eco-desc",
        "moves",
        "result",
    ];
    let extra = ["kasparov", "player:carlsen", "event:linares", "date:1960..1980", "result:1-0 tal", "-annotator:tal"];
    let all = queries
        .chain(sorts.iter().map(|s| format!("sort:{s}")))
        .chain(extra.iter().map(|q| q.to_string()))
        .chain(sorts.iter().map(|s| format!("anand sort:{s}")));
    for q in all {
        let got = match search::select(db, idx, Some(&q), None, None) {
            Ok((Selection::All { descending }, _)) => format!("all {descending}"),
            Ok((Selection::Numbers(v), _)) => format!("{:?}", v.to_vec()),
            Err(SearchError::Unsupported(u)) => format!("unsupported {u}"),
            Err(e) => panic!("{q}: {e:?}"),
        };
        out.push(format!("{q} => {got}"));
    }
    for field in [SuggestField::Player, SuggestField::Event, SuggestField::Annotator] {
        for prefix in ["", "a", "c", "k", "ka", "l", "m", "t", "w", "z"] {
            let got = search::suggest(db, idx, field, prefix, 20).map(|s| format!("{:?}", s.to_vec()));
            out.push(format!("{field:?} {prefix:?} => {got:?}"));
        }
    }
    out
}

fn built(db: &Base, path: &Path) -> Heads {
    match heads::build_base(db, 7, path, &|| true).unwrap() {
        Built::Ready(h) => h,
        _ => panic!("every record fits a row"),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-heads-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// With the heads file set, `db` answers as without it, and its rows were read.
fn same_with_heads(db: &Base, name: &str) {
    let plain = answers(db, &Indexes::default());
    let dir = scratch(name);
    let h = Arc::new(built(db, &heads::path(&dir, ID)));
    assert!(h.blocks() >= 2 || db.record_count() <= BLOCK_ROWS, "{name}: the file spans blocks");
    let idx = Indexes::default();
    idx.set_heads(Arc::clone(&h));
    let before = ROWS_READ.load(Ordering::Relaxed);
    assert_eq!(answers(db, &idx), plain, "{name}");
    assert!(ROWS_READ.load(Ordering::Relaxed) - before >= u64::from(db.record_count()), "{name}: the rows were read");
    assert!(h.usable());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_answer_is_the_same_from_the_heads_file() {
    let two = fixture_of("heads-2cbh", &many(40_000, &["text", "analysis", "deleted"]));
    same_with_heads(&Base::open(two.dir().join("db.2cbh")).unwrap(), "2cbh");
    // The classic format has no analyses.
    let all = many(40_000, &["text", "deleted"]);
    let extra: Vec<&str> = all[rows(&[]).len()..].iter().map(String::as_str).collect();
    let classic = classic_fixture("heads-cbh", &extra);
    same_with_heads(&Base::open(classic.dir().join("db.cbh")).unwrap(), "cbh");
    // A PGN file holds games only.
    let games: Vec<String> =
        many(20_000, &[]).into_iter().filter(|l| l.split('|').nth(1).unwrap().trim() == "game").collect();
    let fp = pgn_fixture("heads-pgn", &games);
    let (pgn, index) = (fp.dir().join("db.pgn"), fp.dir().join("db.head"));
    pgnfile::build(&pgn, &index, 1, CodePage::WESTERN, &mut |_| true).unwrap();
    same_with_heads(&Base::Pgn(pgnfile::Database::open(&pgn, &index, 1, CodePage::WESTERN).unwrap()), "pgn");
}

#[test]
fn a_damaged_block_is_read_from_the_database() {
    let two = fixture_of("heads-damaged", &many(40_000, &["text", "analysis", "deleted"]));
    let db = Base::open(two.dir().join("db.2cbh")).unwrap();
    let plain = answers(&db, &Indexes::default());
    let dir = scratch("damaged");
    let path = heads::path(&dir, ID);
    drop(built(&db, &path));
    // A byte inside the second block's rows.
    let mut bytes = std::fs::read(&path).unwrap();
    let at = 64 + (BLOCK_ROWS as usize + 5) * 36 + 3;
    bytes[at] ^= 0x5a;
    std::fs::write(&path, &bytes).unwrap();
    let h = Arc::new(Heads::open(&path, 7, db.record_count()).expect("the header and table are whole"));
    let idx = Indexes::default();
    idx.set_heads(Arc::clone(&h));
    assert_eq!(answers(&db, &idx), plain);
    assert!(!h.usable(), "the file is marked broken, to be built again");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_of_another_generation_or_size_is_never_opened() {
    let two = fixture_of("heads-other", &many(20_000, &["text", "analysis", "deleted"]));
    let db = Base::open(two.dir().join("db.2cbh")).unwrap();
    let dir = scratch("other");
    let path = heads::path(&dir, ID);
    drop(built(&db, &path));
    let n = db.record_count();
    assert!(Heads::open(&path, 7, n).is_some());
    assert!(Heads::open(&path, 8, n).is_none(), "another generation");
    assert!(Heads::open(&path, 7, n + 1).is_none(), "another record count");
    let whole = std::fs::read(&path).unwrap();
    std::fs::write(&path, &whole[..whole.len() - 1]).unwrap();
    assert!(Heads::open(&path, 7, n).is_none(), "a truncated file");
    let mut bad = whole.clone();
    bad[20] ^= 1;
    std::fs::write(&path, &bad).unwrap();
    assert!(Heads::open(&path, 7, n).is_none(), "a damaged header");
    let mut bad = whole.clone();
    let table = bad.len() - 2;
    bad[table] ^= 1;
    std::fs::write(&path, &bad).unwrap();
    assert!(Heads::open(&path, 7, n).is_none(), "a damaged table");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_build_whose_database_changes_keeps_nothing() {
    let two = fixture_of("heads-changing", &many(40_000, &[]));
    let db = Base::open(two.dir().join("db.2cbh")).unwrap();
    let dir = scratch("changing");
    let path = heads::path(&dir, ID);
    let asked = std::sync::atomic::AtomicU32::new(0);
    let still = || asked.fetch_add(1, Ordering::Relaxed) < 1;
    assert!(matches!(heads::build_base(&db, 7, &path, &still), Ok(Built::Changed)));
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "no file, whole or partial, is left");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every read of a block is checked, not only the first: a block damaged after
/// a pass has read it whole is read from the database, and the answer stays.
#[test]
fn a_block_damaged_after_a_pass_is_read_from_the_database() {
    let two = fixture_of("heads-later", &many(40_000, &["text", "analysis", "deleted"]));
    let db = Base::open(two.dir().join("db.2cbh")).unwrap();
    let plain = answers(&db, &Indexes::default());
    let dir = scratch("later");
    let path = heads::path(&dir, ID);
    let h = Arc::new(built(&db, &path));
    let idx = Indexes::default();
    idx.set_heads(Arc::clone(&h));
    assert_eq!(answers(&db, &idx), plain);
    assert!(h.usable());
    // The first record's result, in place, with the CRC table left as it was.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[64 + 34] ^= 0x03;
    std::fs::write(&path, &bytes).unwrap();
    let fresh = Indexes::default();
    fresh.set_heads(Arc::clone(&h));
    assert_eq!(answers(&db, &fresh), plain);
    assert!(!h.usable(), "the file is marked broken, to be built again");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Through the catalog: a heads file found broken is dropped and built again,
/// and passes read the new one.
#[test]
fn a_broken_heads_file_is_built_again() {
    let two = fixture_of("heads-rebuilt", &many(40_000, &["text", "analysis", "deleted"]));
    let db_path = two.dir().join("db.2cbh");
    let plain = answers(&Base::open(&db_path).unwrap(), &Indexes::default());
    let dir = scratch("rebuilt");
    let catalog = Catalog::new([db_path]);
    catalog.explorer.set_dir(dir.clone());
    catalog.heads.set_min_records(1);
    let entry = catalog.entries().into_iter().next().unwrap();
    let path = heads::path(&dir, &entry.id);
    let open = entry.open().unwrap();
    let ready = |open: &Opened| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            catalog.attach_heads(&entry, open);
            if open.indexes.has_usable_heads() {
                return;
            }
            assert!(std::time::Instant::now() < until, "a heads file was attached");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    ready(&open);
    let mut bytes = std::fs::read(&path).unwrap();
    let damaged = 64 + (BLOCK_ROWS as usize + 5) * 36 + 3;
    bytes[damaged] ^= 0x5a;
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(answers(&open.db, &open.indexes), plain, "the damaged block is read from the database");
    assert!(!open.indexes.has_usable_heads(), "the file is marked broken");
    // The next requests drop it and build it again; the rows are read again.
    ready(&open);
    assert_ne!(std::fs::read(&path).unwrap()[damaged], bytes[damaged], "the file was built again");
    let before = ROWS_READ.load(Ordering::Relaxed);
    assert_eq!(answers(&open.db, &open.indexes), plain);
    assert!(ROWS_READ.load(Ordering::Relaxed) > before, "the new file's rows were read");
    let _ = std::fs::remove_dir_all(&dir);
}
