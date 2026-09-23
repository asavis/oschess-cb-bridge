//! The response budget is process-wide, so exhausting it on purpose has its
//! own test binary: other tests in the same process would see `busy` too.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::budget::{RESPONSE_BUDGET, reserve};
use bridge::catalog::{Catalog, id_of};
use bridge::server;
use cbformat::fixture::{Builder, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
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
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let path = db.dir().join("db.2cbh");
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new([path.clone()]),
        between_reads: None,
    };
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listeners, app));
    let id = id_of(&path);
    let taken = reserve(RESPONSE_BUDGET - 64).unwrap();
    for p in [format!("/v1/databases/{id}/games/1"), format!("/v1/databases/{id}/games")] {
        let (status, out) = get(port, &p);
        assert_eq!(status, 503, "{p}: {out}");
        assert!(out.contains(r#""code":"busy""#) && out.contains("Retry-After: 1"), "{out}");
    }
    assert_eq!(get(port, "/v1/status").0, 200, "small answers are not budgeted");
    drop(taken);
    assert_eq!(get(port, &format!("/v1/databases/{id}/games/1")).0, 200);
    assert_eq!(get(port, &format!("/v1/databases/{id}/games")).0, 200);
}
