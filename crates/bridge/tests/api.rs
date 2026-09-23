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
use cbformat::fixture::{Builder, TempDb, annotations, arrows, lid_header, quiet, squares, symbols, text};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::language;

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
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
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
    std::thread::spawn(move || server::serve(listeners, app));
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

/// A `.2lid` with six entity types (players, tournaments, sources, the unused
/// type 3, teams, game tags), `count` entities each in containers of `size`
/// bytes, holding `entities` as (type, id, record after its length field).
fn lid_with(size: usize, count: usize, entities: &[(usize, usize, Vec<u8>)]) -> Vec<u8> {
    const TYPES: usize = 6;
    const HEADER: usize = 184;
    let mut d = Vec::new();
    d.extend((HEADER as i32).to_be_bytes());
    d.extend((TYPES as i32).to_be_bytes());
    for _ in 0..TYPES {
        d.extend((size as i32).to_be_bytes());
        d.extend((count as i64).to_be_bytes());
        d.extend((-1i64).to_be_bytes());
    }
    d.resize(HEADER + size * TYPES * count, 0);
    for (typ, id, record) in entities {
        let o = HEADER + id * size * TYPES + typ * size;
        d[o..o + 4].copy_from_slice(&(record.len() as i32).to_le_bytes());
        d[o + 4..o + 4 + record.len()].copy_from_slice(record);
    }
    d
}

fn strings(parts: &[&str]) -> Vec<u8> {
    parts.iter().flat_map(|s| (s.len() as i32).to_le_bytes().into_iter().chain(s.bytes())).collect()
}

/// A game tag: one title in language 0, one empty one in language 1.
fn titles(title: &str) -> Vec<u8> {
    let mut r = 2i32.to_le_bytes().to_vec();
    r.extend(0i32.to_le_bytes());
    r.extend(strings(&[title]));
    r.extend(1i32.to_le_bytes());
    r.extend(strings(&[""]));
    r
}

/// Guiding texts and analyses have header layouts of their own; their rows
/// carry their title and author, never fields read through the game layout.
#[test]
fn texts_and_analyses_are_read_with_their_own_layouts() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    let game = b.game(e4);
    game[0x28..0x30].copy_from_slice(&1i64.to_le_bytes()); // tournament 1
    let text = b.game(e4);
    text[0] |= 2;
    text[0x10..0x18].copy_from_slice(&1i64.to_le_bytes()); // a text's tournament
    text[0x20..0x28].copy_from_slice(&1i64.to_le_bytes()); // author: player 1
    text[0x28..0x30].copy_from_slice(&0i64.to_le_bytes()); // title: game tag 0
    let analysis = b.game(e4);
    analysis[2] = 2;
    analysis[0x18..0x20].copy_from_slice(&1i64.to_le_bytes()); // title: game tag 1
    analysis[0x28..0x30].copy_from_slice(&1i64.to_le_bytes()); // author: player 1
    b.lid(lid_with(
        256,
        2,
        &[
            (0, 0, strings(&["Morphy", "Paul"])),
            (0, 1, strings(&["Author", "Text"])),
            (1, 1, [strings(&["Paris", "Paris m"]), 0i32.to_le_bytes().to_vec()].concat()),
            (5, 0, titles("Review text")),
            (5, 1, titles("1.d4 d5 2.c4")),
        ],
    ));
    let db = b.write("api-kinds");
    let r = start(&db, vec![], None);
    let w = get(r.port, &format!("/v1/databases/{}/games", r.id), "");
    assert_eq!(w.status, 200, "{}", w.body);
    let rows: Vec<&str> = w.body.split(r#"{"number":"#).skip(1).collect();
    assert!(rows[0].contains(r#""kind":"game","white":"Morphy, Paul""#), "{}", rows[0]);
    assert!(rows[0].contains(r#""event":"Paris m","site":"Paris""#), "{}", rows[0]);
    assert!(rows[1].contains(r#""kind":"text","white":"""#), "{}", rows[1]);
    assert!(rows[1].contains(r#""event":"Review text","site":"""#), "{}", rows[1]);
    assert!(rows[1].contains(r#""annotator":"Author, Text""#), "{}", rows[1]);
    assert!(rows[2].contains(r#""kind":"analysis","white":"""#), "{}", rows[2]);
    assert!(
        rows[2].contains(r#""event":"1.d4 d5 2.c4""#) && rows[2].contains(r#""annotator":"Author, Text""#),
        "{}",
        rows[2]
    );
}

/// 500 rows sharing one player whose name fills a 1 MiB container: names are
/// cut to 200 characters and looked up once, so the window stays small.
#[test]
fn a_huge_shared_entity_does_not_blow_up_a_window() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for _ in 0..500 {
        b.game(e4);
    }
    let last = "M".repeat((1 << 20) - 16);
    let player = strings(&[&last, ""]);
    let mut lid = lid_header(1 << 20, 1);
    lid.extend((player.len() as i32).to_le_bytes());
    lid.extend(&player);
    b.lid(lid);
    let db = b.write("api-huge-entity");
    let r = start(&db, vec![], None);
    let w = get(r.port, &format!("/v1/databases/{}/games?limit=500", r.id), "");
    assert_eq!(w.status, 200);
    assert!(w.body.len() < 2 << 20, "window of {} bytes", w.body.len());
    let white = w.body.split(r#""white":""#).nth(1).unwrap().split('"').next().unwrap();
    assert_eq!(white.chars().count(), bridge::api::MAX_FIELD_CHARS + 1);
    assert!(white.ends_with('…'));
}

/// A window ending at record `u32::MAX` is served. The header file is sparse,
/// which needs a Unix file system.
#[cfg(unix)]
#[test]
fn a_window_at_the_last_record_number() {
    let db = database("api-max", 1, 0, 0);
    let file = std::fs::OpenOptions::new().write(true).open(db.dir().join("db.2cbh")).unwrap();
    file.set_len((u64::from(u32::MAX) + 1) * 192).unwrap();
    let r = start(&db, vec![], None);
    let numbers = |q: &str| -> Vec<u64> {
        let w = get(r.port, &format!("/v1/databases/{}/games{q}", r.id), "");
        assert_eq!(w.status, 200, "{q}: {}", w.body);
        w.body.split(r#""number":"#).skip(1).map(|s| s.split(',').next().unwrap().parse().unwrap()).collect()
    };
    assert_eq!(numbers("?offset=4294967294&limit=1"), [4294967295]);
    assert_eq!(numbers("?offset=4294967293&limit=5"), [4294967294, 4294967295]);
    assert_eq!(numbers("?sort=number-desc&limit=2"), [4294967295, 4294967294]);
}

/// Refusals made before a request is routed carry CORS headers for an allowed
/// origin, so the page can read them.
#[test]
fn refusals_before_routing_are_readable_by_the_page() {
    let db = database("api-refusals", 1, 0, 0);
    let p = start(&db, vec![], None).port;
    let host = format!("Host: 127.0.0.1:{p}");
    for (head, status) in [
        (format!("GET /v1/status HTTP/1.1\r\n{host}\r\nOrigin: {ORIGIN}\r\nContent-Length: 1"), 413),
        (format!("GET /v1/status?q=%zz HTTP/1.1\r\n{host}\r\nOrigin: {ORIGIN}"), 400),
        (format!("GET /v1/status HTTP/1.1\r\nOrigin: {ORIGIN}\r\n{host}\r\nX-Big: {}", "x".repeat(17 << 10)), 431),
    ] {
        let r = plain(p, &head);
        assert_eq!(r.status, status);
        assert_eq!(r.header("access-control-allow-origin"), Some(ORIGIN), "{status}");
        assert_eq!(r.header("access-control-expose-headers"), Some("Retry-After"), "{status}");
    }
    let r =
        plain(p, &format!("GET /v1/status HTTP/1.1\r\n{host}\r\nOrigin: https://evil.example\r\nContent-Length: 1"));
    assert_eq!((r.status, r.header("access-control-allow-origin")), (413, None));
}

/// The bridge also answers on `[::1]`, where the machine has IPv6 loopback.
#[test]
fn the_ipv6_loopback_is_served() {
    if std::net::TcpListener::bind(("::1", 0)).is_err() {
        eprintln!("no IPv6 loopback on this machine; nothing to test");
        return;
    }
    let db = database("api-ipv6", 1, 0, 0);
    let p = start(&db, vec![], None).port;
    let mut s = TcpStream::connect(("::1", p)).unwrap();
    let raw = format!(
        "GET /v1/status HTTP/1.1\r\nHost: [::1]:{p}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
}

/// A second instance fails on the port before it touches the token, which the
/// running bridge still accepts.
#[test]
fn a_second_instance_leaves_the_token_alone() {
    let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = busy.local_addr().unwrap().port();
    let home = std::env::temp_dir().join(format!("bridge-second-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
    std::fs::write(home.join("token"), TOKEN).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oschess-bridge"))
        .arg("--new-token")
        .env("OSCHESS_BRIDGE_HOME", &home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(home.join("token")).unwrap(), TOKEN);
    let _ = std::fs::remove_dir_all(&home);
}

/// A move record over the rendering limit is refused before it is read, and
/// the server goes on serving.
#[test]
fn a_game_too_large_to_render_is_refused() {
    let mut b = Builder::new();
    let mut words = vec![MOVES];
    words.extend(std::iter::repeat_n(cbformat::movetable::NULL_MOVE, bridge::api::MAX_GAME_BYTES / 2 + 1));
    words.push(END_OF_LINE);
    let huge = b.moves(1, &words);
    b.game(huge);
    let db = b.write("api-huge-game");
    let r = start(&db, vec![], None);
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 422, "{}", g.body);
    assert!(g.body.contains(r#""code":"unreadable_game""#) && g.body.contains("limit"), "{}", g.body);
    assert_eq!(get(r.port, "/v1/status", "").status, 200);
}

/// Over the connection cap, the `busy` answer is readable by an allowed page,
/// and so is a refused `Host`.
#[test]
fn busy_and_misdirected_answers_carry_cors() {
    let db = database("api-busy", 1, 0, 0);
    let p = start(&db, vec![], None).port;
    let r = plain(p, &format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:1\r\nOrigin: {ORIGIN}"));
    assert_eq!((r.status, r.header("access-control-allow-origin")), (421, Some(ORIGIN)));
    let idle: Vec<TcpStream> =
        (0..server::MAX_CONNECTIONS).map(|_| TcpStream::connect(("127.0.0.1", p)).unwrap()).collect();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let r = get(p, "/v1/status", "");
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains(r#""code":"busy""#));
    assert_eq!(r.header("retry-after"), Some("1"));
    assert_eq!(r.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(r.header("access-control-expose-headers"), Some("Retry-After"));
    drop(idle);
}

/// A tiny game whose players and tournament are megabytes of control
/// characters, each six bytes in JSON: its answer is refused before it is
/// built, and the server goes on serving.
#[test]
fn a_game_whose_answer_would_be_huge_is_refused() {
    let mut b = Builder::new();
    let empty = b.moves(1, &[MOVES, END_OF_LINE]);
    let game = b.game(empty);
    game[0x28..0x30].copy_from_slice(&0i64.to_le_bytes());
    let name = "\u{1}".repeat((1 << 20) - 16);
    b.lid(lid_with(
        1 << 20,
        1,
        &[
            (0, 0, strings(&[&name, ""])),
            (1, 0, [strings(&[&name[..1000], &name[..1000]]), 0i32.to_le_bytes().to_vec()].concat()),
        ],
    ));
    let db = b.write("api-huge-answer");
    let r = start(&db, vec![], None);
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 422, "{}", &g.body[..g.body.len().min(300)]);
    assert!(g.body.contains("too large") && g.body.contains("limit"), "{}", g.body);
    assert_eq!(get(r.port, "/v1/status", "").status, 200);
}

/// Silent connections queued over the cap cannot delay the busy answer of a
/// request behind them: every deadline runs from acceptance.
#[test]
fn silent_queued_connections_do_not_delay_the_busy_answer() {
    let db = database("api-busy-queue", 1, 0, 0);
    let p = start(&db, vec![], None).port;
    let serving: Vec<TcpStream> =
        (0..server::MAX_CONNECTIONS).map(|_| TcpStream::connect(("127.0.0.1", p)).unwrap()).collect();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let silent: Vec<TcpStream> = (0..12).map(|_| TcpStream::connect(("127.0.0.1", p)).unwrap()).collect();
    let started = std::time::Instant::now();
    let r = get(p, "/v1/status", "");
    let waited = started.elapsed();
    assert_eq!(r.status, 503, "{}", r.body);
    assert_eq!(r.header("access-control-allow-origin"), Some(ORIGIN));
    assert!(waited < std::time::Duration::from_millis(2500), "the busy answer took {waited:?}");
    drop(silent);
    drop(serving);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(get(p, "/v1/status", "").status, 200);
}

/// Game 1: 1.e4 e5 with annotation record `content`; game 2: 1.e4 with an
/// empty annotation record.
fn annotated_database(name: &str, content: &[u8]) -> TempDb {
    let mut b = Builder::new();
    let moves = b.moves(
        1,
        &[
            MOVES,
            quiet(Color::White, Piece::Pawn, "e2", "e4"),
            quiet(Color::Black, Piece::Pawn, "e7", "e5"),
            END_OF_LINE,
        ],
    );
    let a = b.annotations(content);
    b.annotated_game(moves, a);
    b.game(moves);
    b.write(name)
}

#[test]
fn annotated_games_are_served_with_their_annotations() {
    let content = annotations(&[
        (-1, vec![text(false, language::ENGLISH, "A classic")]),
        (
            0,
            vec![
                symbols(1, 0, 0),
                squares(&[(2, "e4")]),
                arrows(&[(3, "g1", "f3")]),
                text(false, language::ENGLISH, "Best by test"),
                text(false, language::GERMAN, "Bestens"),
            ],
        ),
        (1, vec![text(true, language::ENGLISH, "Then"), text(true, language::GERMAN, "Dann")]),
    ]);
    let db = annotated_database("api-annotated", &content);
    let r = start(&db, vec![], None);
    let game = |query: &str| get(r.port, &format!("/v1/databases/{}/games/1{query}", r.id), "");
    let g = game("");
    assert_eq!(g.status, 200, "{}", g.body);
    assert!(
        g.body.contains(r#"{A classic} 1. e4 $1 {[%csl Ge4][%cal Yg1f3] Best by test} {Then} 1... e5 1-0\n""#),
        "{}",
        g.body
    );
    assert!(g.body.ends_with(r#""annotations":"complete"}"#), "{}", g.body);
    assert!(!g.body.contains("unreadableAnnotation"));
    // The first preferred language the game has; a language ChessBase does not
    // store is passed over.
    for query in ["?lang=de", "?lang=uk,de,en", "?lang=uk%2Cde"] {
        let g = game(query);
        assert!(g.body.contains("Bestens} {Dann} 1... e5"), "{query}: {}", g.body);
    }
    for query in ["?lang=en,de", "?lang=uk", "?lang="] {
        assert!(game(query).body.contains("Best by test} {Then}"), "{query}");
    }
    // A game with an empty annotation record, and a database without `.2cba`.
    let g = get(r.port, &format!("/v1/databases/{}/games/2", r.id), "");
    assert!(g.body.ends_with(r#""annotations":"none"}"#), "{}", g.body);
    let plain = database("api-no-annotations", 1, 0, 0);
    let r = start(&plain, vec![], None);
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert!(g.body.ends_with(r#""annotations":"none"}"#), "{}", g.body);
}

#[test]
fn an_unknown_annotation_layout_is_reported() {
    // Type 1a has no known layout: the text after it cannot be found.
    let content = annotations(&[
        (0, vec![text(false, language::ENGLISH, "kept"), vec![0x1a, 0, 1, 2, 3], text(false, 0, "lost")]),
        (1, vec![text(false, language::ENGLISH, "lost too")]),
    ]);
    let db = annotated_database("api-unknown-annotation", &content);
    let r = start(&db, vec![], None);
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 200, "{}", g.body);
    assert!(g.body.contains(r#"1. e4 {kept} 1... e5 1-0"#), "{}", g.body);
    assert!(!g.body.contains("lost"), "{}", g.body);
    assert!(g.body.ends_with(r#""annotations":"incomplete","unreadableAnnotation":26}"#), "{}", g.body);
}

#[test]
fn an_oversized_annotation_record_is_refused() {
    let long = "x".repeat(bridge::api::MAX_GAME_BYTES);
    let db = annotated_database("api-huge-annotation", &annotations(&[(0, vec![text(false, 0, &long)])]));
    let r = start(&db, vec![], None);
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 422, "{}", &g.body[..g.body.len().min(300)]);
    assert!(
        g.body.contains(r#""code":"unreadable_game""#)
            && g.body.contains("annotation record")
            && g.body.contains("limit"),
        "{}",
        g.body
    );
    // The other game of that database is served.
    assert_eq!(get(r.port, &format!("/v1/databases/{}/games/2", r.id), "").status, 200);
}

/// A save that rewrites the annotation record during the read is detected
/// like one that rewrites the moves: the read is retried.
#[test]
fn a_change_to_the_annotations_during_the_read_is_retried() {
    let comment = |t: &str| annotations(&[(0, vec![text(false, language::ENGLISH, t)])]);
    let db = annotated_database("api-annotation-retry", &comment("before"));
    let cba = db.dir().join("db.2cba");
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let hook = move || {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            // The first annotation record sits right after the 12-byte header.
            let frame = cbformat::fixture::framed(cbformat::v2::ANNOTATION_TAG, &comment("latest"));
            write_at(&cba, 12, &frame);
        }
    };
    let r = start(&db, vec![], Some(Box::new(hook)));
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 200, "{}", g.body);
    assert!(g.body.contains("1. e4 {latest}"), "the retry serves the saved annotations: {}", g.body);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
