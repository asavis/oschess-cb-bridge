//! The search memory budget is process-wide, so taking most of it on purpose
//! has its own test binary. Sparse files need a Unix file system.
#![cfg(unix)]

use std::sync::Arc;

use bridge::search::memory::{Hold, budget, held};
use bridge::search::query::Sort;
use bridge::search::workers::{taken, threads};
use bridge::search::{self, BATCH_BYTES, Indexes, SearchError, Selection, SuggestField, Suggestion};
use cbformat::fixture::{Builder, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;

/// The budget is one per process: its checks run one after another.
#[test]
fn the_search_budget() {
    retained_orders_are_evicted_and_a_full_budget_answers_busy();
    suggestion_copies_hold_their_bytes();
    concurrent_searches_share_the_workers_and_the_budget();
}

/// Retained sort orders are evicted when a new one would not fit, and a
/// search that cannot fit even then answers `Busy`; it is served once the
/// memory is returned.
fn retained_orders_are_evicted_and_a_full_budget_answers_busy() {
    const RECORDS: u64 = 1_000_000;
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    let f = b.write("budget-evict");
    let file = std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2cbh")).unwrap();
    file.set_len((RECORDS + 1) * 192).unwrap();
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::shared();
    let order = |idx: &Arc<Indexes>, sort: &str| match search::select(&db, idx, None, None, Sort::parse(sort)) {
        Ok((Selection::Numbers(v), _)) => Ok(v.len()),
        Ok(_) => panic!("an order expected"),
        Err(e) => Err(e),
    };

    // A date order keeps 4 MB; building one takes 12 MB.
    assert_eq!(order(&idx, "date").unwrap(), RECORDS as usize);
    let retained = held();
    assert!(retained >= RECORDS as usize * 4);

    // Leave 10 MB free besides the workers' batch buffers: an ECO order fits
    // only once the date order is evicted, after which the budget holds the
    // ECO order in its place.
    let buffers = threads() * BATCH_BYTES;
    let taken = Hold::reserve(budget() - retained - buffers - (10 << 20)).unwrap();
    let scanned = idx.scanned();
    assert_eq!(order(&idx, "eco").unwrap(), RECORDS as usize);
    assert_eq!(held(), taken.bytes() + retained, "the date order made room");

    // With 5 MB left, evicting the ECO order is not enough for another.
    let more = Hold::reserve(budget() - held() - (5 << 20)).unwrap();
    assert!(matches!(order(&idx, "date"), Err(SearchError::Busy)));
    assert_eq!(held(), taken.bytes() + more.bytes(), "the refused build returned what it held");

    // Returned memory serves the next request, which rebuilds the date order.
    drop((taken, more));
    assert_eq!(order(&idx, "date").unwrap(), RECORDS as usize);
    assert!(idx.scanned() >= scanned + 2 * RECORDS, "the eco and date orders were built again");
}

/// The names a suggestion returns are copies whose bytes are held in the
/// budget while they live; without room for them the answer is `Busy`.
fn suggestion_copies_hold_their_bytes() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    let names = ["Morphy, Paul", "Morphy, Alonzo"];
    for id in 0..2i64 {
        let g = b.game(e4);
        g[0x18..0x20].copy_from_slice(&id.to_le_bytes());
        g[0x20..0x28].copy_from_slice(&(-1i64).to_le_bytes());
    }
    let mut lid = Vec::new();
    lid.extend(184i32.to_be_bytes());
    lid.extend(2i32.to_be_bytes());
    for count in [2i64, 0] {
        lid.extend(64i32.to_be_bytes());
        lid.extend(count.to_be_bytes());
        lid.extend((-1i64).to_be_bytes());
    }
    lid.resize(184, 0);
    for name in names {
        let (last, first) = name.split_once(", ").unwrap();
        let mut r = Vec::new();
        for part in [last, first] {
            r.extend((part.len() as i32).to_le_bytes());
            r.extend(part.as_bytes());
        }
        let mut c = (r.len() as i32).to_le_bytes().to_vec();
        c.extend(r);
        c.resize(64, 0);
        lid.extend(c);
        lid.extend([0u8; 64]);
    }
    b.lid(lid);
    let f = b.write("budget-suggest");
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    let idx = Indexes::shared();
    // The first call builds the names, groups and counts, which stay.
    drop(search::suggest(&db, &idx, SuggestField::Player, "mor", 20).ok().unwrap());
    let before = held();
    let list = search::suggest(&db, &idx, SuggestField::Player, "mor", 20).ok().unwrap();
    let copies: usize = list.iter().map(|s| s.name.len() + std::mem::size_of::<Suggestion>()).sum();
    assert_eq!(list.len(), 2);
    assert_eq!(held(), before + copies);
    drop(list);
    assert_eq!(held(), before);
    let taken = Hold::reserve(budget() - before - copies + 1).unwrap();
    assert!(matches!(search::suggest(&db, &idx, SuggestField::Player, "mor", 20), Err(SearchError::Busy)));
    drop(taken);
}

/// Searches running at once share one set of workers and the budget: with room
/// for two workers' buffers, eight non-matching searches each finish or are
/// answered `Busy`, every byte and worker comes back, and with no room for a
/// single buffer a search is refused at once.
fn concurrent_searches_share_the_workers_and_the_budget() {
    const RECORDS: u64 = 1_000_000;
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    let f = b.write("budget-concurrent");
    let file = std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2cbh")).unwrap();
    file.set_len((RECORDS + 1) * 192).unwrap();
    let db = Database::open(f.dir().join("db.2cbh")).unwrap();
    // Not registered for eviction, so that the accounting below is exact.
    let idx = Indexes::default();
    // The first search loads the names; after it, only scans need memory.
    assert!(search::select(&db, &idx, Some("needle"), None, None).is_ok());
    let before = held();
    let room = Hold::reserve(budget() - before - 2 * BATCH_BYTES).unwrap();
    let results: Vec<Result<(), SearchError>> = std::thread::scope(|s| {
        let running: Vec<_> = (0..8)
            .map(|i| {
                let (db, idx) = (&db, &idx);
                s.spawn(move || search::select(db, idx, Some(&format!("needle{i}")), None, None).map(|_| ()))
            })
            .collect();
        running.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(results.iter().all(|r| matches!(r, Ok(()) | Err(SearchError::Busy))), "{results:?}");
    assert!(results.iter().any(Result::is_ok), "{results:?}");
    assert_eq!(taken(), 0, "every worker came back");
    assert_eq!(held(), before + room.bytes(), "every byte came back");
    drop(room);
    let all = Hold::reserve(budget() - held()).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(search::select(&db, &idx, Some("needle-last"), None, None), Err(SearchError::Busy)));
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "refused without waiting");
    drop(all);
    assert_eq!(held(), before);
}
