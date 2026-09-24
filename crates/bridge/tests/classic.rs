//! Classic databases through the HTTP API: listed, searched and served like
//! 2CBH ones, and answering as a 2CBH copy of the same content answers.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::{App, MAX_GAME_BYTES};
use bridge::catalog::{Catalog, id_of};
use bridge::server;
use cbformat::fixture_cbh::{Builder, Tok, annotation_record, encode, move_record};
use chesscore::Board;

mod common;
use common::{classic_fixture, fixture};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

/// Serves `paths`, with the position indexes in `index_dir`.
fn start(paths: &[PathBuf], index_dir: &Path) -> u16 {
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new(paths.to_vec()),
        between_reads: None,
        engine: bridge::engine::Engine::none(),
    };
    app.catalog.explorer.set_dir(index_dir.to_path_buf());
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listeners, app));
    port
}

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {}\r\nConnection: close\r\n\r\n",
        DEFAULT_ORIGINS[0]
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split(' ').nth(1).unwrap().parse().unwrap();
    (status, out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

/// A body without its `"generation":"…"` member, which differs between copies.
fn without_generation(body: &str) -> String {
    match body.find(r#""generation":""#) {
        Some(at) => {
            let end = at + 14 + body[at + 14..].find('"').unwrap() + 1;
            let end = if body[end..].starts_with(',') { end + 1 } else { end };
            format!("{}{}", &body[..at], &body[end..])
        }
        None => body.to_string(),
    }
}

fn index_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-classic-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The fixture of `docs/search-grammar.md` in both formats, served together:
/// the classic copy is listed as `cbh` and ready, and its lists, searches,
/// sorts, suggestions, games and explorer answers are the 2CBH copy's.
#[test]
fn a_classic_copy_answers_as_its_2cbh_copy() {
    let (f2, fc) = (fixture("classic-api-2cbh", &[]), classic_fixture("classic-api-cbh", &[]));
    let (p2, pc) = (f2.dir().join("db.2cbh"), fc.dir().join("db.cbh"));
    let dir = index_dir("copies");
    let port = start(&[pc.clone(), p2.clone()], &dir);
    let (ic, i2) = (id_of(&pc), id_of(&p2));

    let (status, body) = get(port, "/v1/databases");
    assert_eq!(status, 200);
    let listed = format!(r#""id":"{ic}","name":"db","format":"cbh","state":"ready","records":10,"generation":""#);
    assert!(body.contains(&listed), "{body}");
    assert!(get(port, "/v1/status").1.contains(r#""ready":2"#));

    let both = |path: &str| {
        let (a, b) = (get(port, &path.replace("{id}", &ic)), get(port, &path.replace("{id}", &i2)));
        assert_eq!(a.0, b.0, "{path}: {}", a.1);
        assert_eq!(without_generation(&a.1), without_generation(&b.1), "{path}");
        a
    };
    for query in [
        "",
        "offset=3&limit=4",
        "sort=number-desc&offset=2&limit=5",
        "q=morphy",
        "q=london",
        "q=annotator%3Animzo&sort=date",
        "q=eco%3AC5&sort=eco-desc",
        "q=event%3Apetersburg&sort=white",
        "sort=tournament",
        "sort=annotator-desc",
        "sort=round",
        "q=tag%3Ax",
    ] {
        both(&format!("/v1/databases/{{id}}/games?{query}"));
    }
    for field in ["player", "event", "annotator"] {
        for prefix in ["a", "c", "l", "m", "mik", "n", "st", "t"] {
            both(&format!("/v1/databases/{{id}}/suggest?field={field}&prefix={prefix}"));
        }
    }
    // The guiding text: its title as the event, and no game fields.
    let (_, row) = both("/v1/databases/{id}/games?offset=7&limit=1");
    assert!(row.contains(r#""number":8,"kind":"text","white":"","#) && row.contains(r#""event":"London""#), "{row}");
    assert_eq!(both("/v1/databases/{id}/games/8").0, 422);
    assert_eq!(both("/v1/databases/{id}/games/11").0, 404);
    for number in [1, 3, 5, 9, 10] {
        let (status, body) = both(&format!("/v1/databases/{{id}}/games/{number}"));
        assert_eq!(status, 200, "{body}");
    }

    let start_fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR%20w%20KQkq%20-%200%201";
    let after_e4 = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR%20b%20KQkq%20-%200%201";
    for fen in [start_fen, after_e4] {
        let url = format!("/v1/databases/{{id}}/explorer?fen={fen}");
        let deadline = Instant::now() + Duration::from_secs(30);
        while [&ic, &i2].iter().any(|id| get(port, &url.replace("{id}", id)).0 == 409) {
            assert!(Instant::now() < deadline, "the indexes were not built");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (status, body) = both(&url);
        assert_eq!(status, 200, "{body}");
    }
    let (_, body) = get(port, &format!("/v1/databases/{ic}/explorer?fen={start_fen}"));
    // Games 1-7 and 10: not the guiding text, and not the deleted game.
    assert!(body.contains(r#""index":{"records":10,"games":8,"#), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A classic game is served with its annotations; a move or annotation record
/// over the rendering limit is refused before it is read.
#[test]
fn classic_games_are_rendered_within_the_limits() {
    let e4 = move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let mut b = Builder::new();
    b.game(&e4);
    b.annotations(&annotation_record(1, &[(0, 0x02, b"\x00\x2afine"), (0, 0x03, &[1])]));
    // A move record over the limit.
    b.game(&move_record(0, None, None, &vec![0; MAX_GAME_BYTES]));
    // An annotation record over the limit: 40 comments of 60,000 bytes.
    b.game(&e4);
    let mut long = b"\x00\x2a".to_vec();
    long.resize(60_000, b'x');
    let items: Vec<(i32, u8, &[u8])> = (0..40).map(|_| (0, 0x02, &long[..])).collect();
    b.annotations(&annotation_record(3, &items));
    let f = b.write("classic-api-limits");
    let path = f.dir().join("db.cbh");
    let dir = index_dir("limits");
    let port = start(std::slice::from_ref(&path), &dir);
    let id = id_of(&path);

    let (status, body) = get(port, &format!("/v1/databases/{id}/games/1?lang=en"));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""annotations":"complete""#), "{body}");
    assert!(body.contains("1. e4 $1 {fine} 1-0"), "{body}");
    for number in [2, 3] {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games/{number}"));
        assert_eq!(status, 422, "{body}");
        assert!(body.contains(r#""code":"unreadable_game""#) && body.contains("over the limit"), "{body}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
