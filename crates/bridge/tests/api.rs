//! The server against `docs/api.md`, over real loopback connections.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::server;
use cbformat::fixture::{Builder, TempDb, lid_header, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const ORIGIN: &str = "https://oschess.org";

/// `games` games of 1.e4 won by white, white and black being "Morphy, Paul";
/// record `text` (1-based, 0 for none) is a guiding text and record `broken`
/// points at no move record.
fn database(name: &str, games: u32, text: u32, broken: u32) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for g in 1..=games {
        let rec = b.game(if g == broken { 5 } else { e4 });
        if g == text {
            rec[0] |= 2;
        }
    }
    let mut player = Vec::new();
    for s in [&b"Morphy"[..], b"Paul"] {
        player.extend((s.len() as i32).to_le_bytes());
        player.extend(s);
    }
    let mut lid = lid_header(1024, 1);
    lid.extend((player.len() as i32).to_le_bytes());
    lid.extend(&player);
    b.lid(lid);
    b.write(name)
}

struct Running {
    port: u16,
    id: String,
}

fn start(db: &TempDb, extra: Vec<PathBuf>, hook: Option<Box<dyn Fn() + Send + Sync>>) -> Running {
    let listener = server::bind(0).unwrap();
    let port = listener.local_addr().unwrap().port();
    let path = db.dir().join("db.2cbh");
    let mut paths = vec![path.clone()];
    paths.extend(extra);
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new(paths),
        between_reads: hook,
    };
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listener, app));
    Running { port, id: id_of(&path) }
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

fn exchange(port: u16, raw: &[u8]) -> Reply {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(raw).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    parse_reply(&String::from_utf8(out).unwrap())
}

fn parse_reply(text: &str) -> Reply {
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let (status_line, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    let status = status_line.split(' ').nth(1).unwrap().parse().unwrap();
    Reply { status, headers: headers.to_string(), body: body.to_string() }
}

/// A GET with the token, an allowed Origin and `Connection: close`, plus `extra` header lines.
fn get(port: u16, path: &str, extra: &str) -> Reply {
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {ORIGIN}\r\n{extra}Connection: close\r\n\r\n"
    );
    exchange(port, raw.as_bytes())
}

fn plain(port: u16, head: &str) -> Reply {
    exchange(port, format!("{head}\r\nConnection: close\r\n\r\n").as_bytes())
}

#[test]
fn status_and_databases() {
    let db = database("api-status", 3, 0, 0);
    let pgn = db.dir().join("games.pgn");
    std::fs::write(&pgn, "[Event \"x\"]\n\n1. e4 *\n").unwrap();
    let r = start(&db, vec![pgn, PathBuf::from("/no/such/base.2cbh")], None);
    let s = get(r.port, "/v1/status", "");
    assert_eq!(s.status, 200, "{}", s.body);
    assert!(s.body.contains(r#""bridge":{"version":"test","api":1}"#), "{}", s.body);
    assert!(
        s.body.contains(r#""ready":1"#) && s.body.contains(r#""missing":1"#) && s.body.contains(r#""unsupported":1"#)
    );
    let d = get(r.port, "/v1/databases", "");
    assert_eq!(d.status, 200);
    assert!(
        d.body.contains(&format!(r#""id":"{}","name":"db","format":"2cbh","state":"ready","records":3"#, r.id)),
        "{}",
        d.body
    );
    assert!(d.body.contains(r#""name":"games","format":"pgn","state":"unsupported""#), "{}", d.body);
    assert!(d.body.contains(r#""name":"base","format":"2cbh","state":"missing""#), "{}", d.body);
    assert!(!d.body.contains(db.dir().to_str().unwrap()), "paths are never sent");
    assert_eq!(d.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(d.header("access-control-expose-headers"), Some("Retry-After"));
}

#[test]
fn access_checks() {
    let db = database("api-access", 1, 0, 0);
    let p = start(&db, vec![], None).port;
    let host = format!("Host: 127.0.0.1:{p}");
    let auth = format!("Authorization: Bearer {TOKEN}");
    let r = plain(p, &format!("GET /v1/status HTTP/1.1\r\n{host}\r\nOrigin: {ORIGIN}"));
    assert_eq!(r.status, 401);
    assert!(r.body.contains(r#""code":"unauthorized""#));
    assert_eq!(r.header("access-control-allow-origin"), Some(ORIGIN), "errors are readable by the page");
    assert_eq!(
        plain(p, &format!("GET /v1/status HTTP/1.1\r\n{host}\r\nAuthorization: Bearer {}x", &TOKEN[1..])).status,
        401
    );
    assert_eq!(plain(p, &format!("GET /v1/status HTTP/1.1\r\n{host}\r\nAuthorization: Basic {TOKEN}")).status, 401);
    let r = plain(p, &format!("GET /v1/status HTTP/1.1\r\nHost: evil.example:{p}\r\n{auth}"));
    assert_eq!((r.status, r.body.contains("misdirected_host")), (421, true));
    assert_eq!(plain(p, &format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:1\r\n{auth}")).status, 421);
    let r = plain(p, &format!("GET /v1/status HTTP/1.1\r\n{host}\r\n{auth}\r\nOrigin: https://evil.example"));
    assert_eq!((r.status, r.header("access-control-allow-origin")), (403, None));
    let r = plain(p, &format!("POST /v1/status HTTP/1.1\r\n{host}\r\n{auth}"));
    assert_eq!((r.status, r.header("allow")), (405, Some("GET, OPTIONS")));
    // No Origin: a program, not a page; the token is enough.
    assert_eq!(plain(p, &format!("GET /v1/status HTTP/1.1\r\nHost: localhost:{p}\r\n{auth}")).status, 200);
    let pre = plain(
        p,
        &format!(
            "OPTIONS /v1/status HTTP/1.1\r\n{host}\r\nOrigin: {ORIGIN}\r\nAccess-Control-Request-Method: GET\r\nAccess-Control-Request-Private-Network: true"
        ),
    );
    assert_eq!(pre.status, 204);
    assert_eq!(pre.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(pre.header("access-control-allow-headers"), Some("Authorization"));
    assert_eq!(pre.header("access-control-allow-private-network"), Some("true"));
    let pre = plain(p, &format!("OPTIONS /v1/status HTTP/1.1\r\n{host}\r\nOrigin: {ORIGIN}"));
    assert_eq!((pre.status, pre.header("access-control-allow-private-network")), (204, None));
    assert_eq!(plain(p, &format!("OPTIONS /v1/status HTTP/1.1\r\n{host}\r\nOrigin: https://evil.example")).status, 403);
    assert_eq!(plain(p, &format!("OPTIONS /v1/status HTTP/1.1\r\n{host}")).status, 403);
}

#[test]
fn malformed_requests_bodies_and_oversized_headers() {
    let db = database("api-malformed", 1, 0, 0);
    let p = start(&db, vec![], None).port;
    let r = plain(p, &format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nContent-Length: 5"));
    assert_eq!((r.status, r.body.contains("body_not_allowed")), (413, true));
    let big = "x".repeat(17 << 10);
    let r = plain(p, &format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nX-Big: {big}"));
    assert_eq!((r.status, r.body.contains("headers_too_large")), (431, true));
    assert_eq!(plain(p, "BLAH").status, 400);
    assert_eq!(get(p, "/v1/nothing", "").status, 404);
}

#[test]
fn game_windows() {
    let db = database("api-windows", 30, 0, 0);
    let r = start(&db, vec![], None);
    let path = |q: &str| format!("/v1/databases/{}/games{q}", r.id);
    let numbers = |reply: &Reply| -> Vec<u32> {
        reply.body.split(r#""number":"#).skip(1).map(|s| s.split(',').next().unwrap().parse().unwrap()).collect()
    };
    let w = get(r.port, &path("?limit=10"), "");
    assert_eq!(w.status, 200, "{}", w.body);
    assert!(w.body.contains(r#""total":30,"offset":0,"sort":"number-asc""#), "{}", w.body);
    assert_eq!(numbers(&w), (1..=10).collect::<Vec<_>>());
    assert!(w.body.contains(r#""white":"Morphy, Paul","whiteElo":0,"black":"Morphy, Paul""#), "{}", w.body);
    assert!(w.body.contains(r#""result":"1-0""#) && w.body.contains(r#""flags":{"deleted":false,"chess960":false}"#));
    assert_eq!(numbers(&get(r.port, &path("?offset=25&limit=10"), "")), [26, 27, 28, 29, 30]);
    assert_eq!(numbers(&get(r.port, &path("?sort=number-desc&limit=3"), "")), [30, 29, 28]);
    assert_eq!(numbers(&get(r.port, &path("?sort=number-desc&offset=28"), "")), [2, 1]);
    assert_eq!(numbers(&get(r.port, &path("?offset=30"), "")), Vec::<u32>::new());
    assert_eq!(numbers(&get(r.port, &path(""), "")).len(), 30);
    for (q, parameter) in [
        ("?limit=0", "limit"),
        ("?limit=501", "limit"),
        ("?offset=-1", "offset"),
        ("?sort=white", "sort"),
        ("?q=player:morphy", "q"),
    ] {
        let e = get(r.port, &path(q), "");
        assert_eq!(e.status, 400, "{q}");
        assert!(e.body.contains(&format!(r#""parameter":"{parameter}""#)), "{q}: {}", e.body);
    }
    assert_eq!(get(r.port, "/v1/databases/0000000000000000/games", "").status, 404);
}

#[test]
fn one_game_and_its_errors() {
    let db = database("api-game", 4, 3, 4);
    let r = start(&db, vec![], None);
    let game = |n: &str| get(r.port, &format!("/v1/databases/{}/games/{n}", r.id), "");
    let g = game("1");
    assert_eq!(g.status, 200, "{}", g.body);
    assert!(g.body.contains(r#""number":1,"pgn":"[Event "#) && g.body.contains(r#"1. e4 1-0\n""#), "{}", g.body);
    for n in ["0", "5", "x", "4294967296"] {
        assert_eq!(game(n).status, 404, "{n}");
    }
    let t = game("3");
    assert_eq!((t.status, t.body.contains("not_a_game")), (422, true));
    let b = game("4");
    assert_eq!(
        (b.status, b.body.contains(r#""code":"unreadable_game""#), b.body.contains("reason")),
        (422, true, true)
    );
}

fn write_at(path: &Path, offset: u64, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset)).unwrap();
    f.write_all(bytes).unwrap();
}

/// Rewrites game 1's result byte, as a save of that game would.
fn flip_result(dir: &Path, result: u8) {
    write_at(&dir.join("db.2cbh"), 192 + 0x58, &[result]);
}

#[test]
fn a_change_during_the_read_is_retried() {
    let db = database("api-retry", 1, 0, 0);
    let dir = db.dir().to_path_buf();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    // The first read is interrupted by a save that turns 1-0 into 0-1.
    let hook = move || {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            flip_result(&dir, 0);
        }
    };
    let r = start(&db, vec![], Some(Box::new(hook)));
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 200, "{}", g.body);
    assert!(g.body.contains(r#"1. e4 0-1\n""#), "the retry serves the saved game: {}", g.body);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_database_that_keeps_changing_is_reported() {
    let db = database("api-changing", 1, 0, 0);
    let dir = db.dir().to_path_buf();
    let n = Arc::new(AtomicUsize::new(0));
    let hook = move || flip_result(&dir, n.fetch_add(1, Ordering::SeqCst) as u8 % 2);
    let r = start(&db, vec![], Some(Box::new(hook)));
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 503, "{}", g.body);
    assert!(g.body.contains(r#""code":"database_changing""#));
    assert_eq!(g.header("retry-after"), Some("1"));
    assert_eq!(g.header("access-control-expose-headers"), Some("Retry-After"));
}

/// The documented limitation: a save paused between its move record and its
/// header is not detected, and the game served combines the two. It is still
/// a valid game.
#[test]
fn a_save_paused_between_its_steps_serves_a_valid_game() {
    let db = database("api-limitation", 1, 0, 0);
    let r = start(&db, vec![], None);
    assert!(get(r.port, &format!("/v1/databases/{}/games/1", r.id), "").body.contains("1. e4 1-0"));
    // The save writes 1.d4 into the move record and has not yet written the new
    // result into the header.
    let words = [MOVES, quiet(Color::White, Piece::Pawn, "d2", "d4"), END_OF_LINE];
    let content: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    write_at(&db.dir().join("db.2cbg"), 12, &cbformat::fixture::framed(1, &content));
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 200, "{}", g.body);
    assert!(g.body.contains("1. d4 1-0"), "new moves, old header: {}", g.body);
}

#[test]
fn appended_games_appear_on_the_next_request() {
    let db = database("api-append", 2, 0, 0);
    let r = start(&db, vec![], None);
    let total = |reply: Reply| reply.body.split(r#""total":"#).nth(1).unwrap().split(',').next().unwrap().to_string();
    let path = format!("/v1/databases/{}/games", r.id);
    assert_eq!(total(get(r.port, &path, "")), "2");
    let bigger = database("api-append-bigger", 5, 0, 0);
    for f in ["db.2cbh", "db.2cbg", "db.2lid"] {
        std::fs::copy(bigger.dir().join(f), db.dir().join(f)).unwrap();
    }
    assert_eq!(total(get(r.port, &path, "")), "5");
}

#[test]
fn a_connection_serves_several_requests() {
    let db = database("api-keepalive", 1, 0, 0);
    let p = start(&db, vec![], None).port;
    let one = format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nAuthorization: Bearer {TOKEN}\r\n\r\n");
    let mut s = TcpStream::connect(("127.0.0.1", p)).unwrap();
    s.write_all(format!("{one}{one}").as_bytes()).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert_eq!(out.matches("HTTP/1.1 200 OK").count(), 2, "{out}");
}
