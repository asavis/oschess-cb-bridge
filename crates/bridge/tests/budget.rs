//! The response budget is process-wide, so exhausting it on purpose has its
//! own test binary: other tests in the same process would see `busy` too.

use bridge::budget::{RESPONSE_BUDGET, reserve};
use bridge::catalog::id_of;
use cbformat::fixture::{Builder, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

mod common;
use common::{TestBridge, app_of, get_reply};

/// With the budget taken, large answers (a game, a list window) are refused
/// `503 busy` before they are built; once it is returned they are served.
#[test]
fn an_exhausted_budget_answers_busy_until_it_is_returned() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    let db = b.write("budget");
    let path = db.dir().join("db.2cbh");
    let bridge = TestBridge::new(app_of([path.clone()]));
    let port = bridge.port;
    let id = id_of(&path);
    let taken = reserve(RESPONSE_BUDGET - 64).unwrap();
    for p in [format!("/v1/databases/{id}/games/1"), format!("/v1/databases/{id}/games")] {
        let r = get_reply(port, &p);
        assert_eq!(r.status, 503, "{p}: {}", r.body);
        assert!(r.body.contains(r#""code":"busy""#) && r.header("retry-after") == Some("1"), "{}", r.body);
    }
    assert_eq!(get_reply(port, "/v1/status").status, 200, "small answers are not budgeted");
    drop(taken);
    assert_eq!(get_reply(port, &format!("/v1/databases/{id}/games/1")).status, 200);
    assert_eq!(get_reply(port, &format!("/v1/databases/{id}/games")).status, 200);
}
