//! The search memory budget is process-wide, so taking most of it on purpose
//! has its own test binary. Sparse files need a Unix file system.
#![cfg(unix)]

use std::sync::Arc;

use bridge::search::memory::{Hold, budget, held};
use bridge::search::query::Sort;
use bridge::search::{self, Indexes, SearchError, Selection};
use cbformat::fixture::{Builder, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;

/// Retained sort orders are evicted when a new one would not fit, and a
/// search that cannot fit even then answers `Busy`; it is served once the
/// memory is returned.
#[test]
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
    let order = |idx: &Arc<Indexes>, sort: &str| match search::select(&db, idx, "", Sort::parse(sort)) {
        Ok((Selection::Numbers(v), _)) => Ok(v.len()),
        Ok(_) => panic!("an order expected"),
        Err(e) => Err(e),
    };

    // A date order keeps 4 MB; building one takes 12 MB.
    assert_eq!(order(&idx, "date").unwrap(), RECORDS as usize);
    let retained = held();
    assert!(retained >= RECORDS as usize * 4);

    // Leave 10 MB free: an ECO order fits only once the date order is evicted,
    // after which the budget holds the ECO order in its place.
    let taken = Hold::reserve(budget() - retained - (10 << 20)).unwrap();
    let scanned = idx.scanned();
    assert_eq!(order(&idx, "eco").unwrap(), RECORDS as usize);
    assert_eq!(held(), taken.bytes() + retained, "the date order made room");

    // With 5 MB more taken, evicting the ECO order is not enough for another.
    let more = Hold::reserve(5 << 20).unwrap();
    assert!(matches!(order(&idx, "date"), Err(SearchError::Busy)));
    assert_eq!(held(), taken.bytes() + more.bytes(), "the refused build returned what it held");

    // Returned memory serves the next request, which rebuilds the date order.
    drop((taken, more));
    assert_eq!(order(&idx, "date").unwrap(), RECORDS as usize);
    assert!(idx.scanned() >= scanned + 2 * RECORDS, "the eco and date orders were built again");
}
