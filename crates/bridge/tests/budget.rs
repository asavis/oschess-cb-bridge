//! The response budget is process-wide, so exhausting it on purpose has its
//! own test binary: other tests in the same process would see `busy` too.

use bridge::api::App;
use bridge::budget::{RESPONSE_BUDGET, reserve};
use bridge::catalog::{Catalog, id_of};
use cbformat::fixture::{Builder, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

mod common;
use common::{exchange, policy, request, serve};

/// The status and the whole answer, head and body, of [`request`]`(port, path)`.
fn get_whole(port: u16, path: &str) -> (u16, String) {
    let out = exchange(port, &request(port, path)).unwrap();
    let status = out.split(' ').nth(1).unwrap().parse().unwrap();
    (status, out)
}

/// With the budget taken, large answers (a game, a list window) are refused
/// `503 busy` before they are built; once it is returned they are served.
#[test]
fn an_exhausted_budget_answers_busy_until_it_is_returned() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    let db = b.write("budget");
    let path = db.dir().join("db.2cbh");
    let port = serve(App::new("test", policy(), Catalog::new([path.clone()])));
    let id = id_of(&path);
    let taken = reserve(RESPONSE_BUDGET - 64).unwrap();
    for p in [format!("/v1/databases/{id}/games/1"), format!("/v1/databases/{id}/games")] {
        let (status, out) = get_whole(port, &p);
        assert_eq!(status, 503, "{p}: {out}");
        assert!(out.contains(r#""code":"busy""#) && out.contains("Retry-After: 1"), "{out}");
    }
    assert_eq!(get_whole(port, "/v1/status").0, 200, "small answers are not budgeted");
    drop(taken);
    assert_eq!(get_whole(port, &format!("/v1/databases/{id}/games/1")).0, 200);
    assert_eq!(get_whole(port, &format!("/v1/databases/{id}/games")).0, 200);
}
