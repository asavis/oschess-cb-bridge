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
        ("?sort=bogus", "sort"),
        ("?sort=name", "sort"),
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

/// 500 rows sharing two players: one whose name fills a 1 MiB container and
/// one with a 3,000-character name. A name record is read to at most 4 KiB,
/// so the first is an empty name; the second is cut to 200 characters. Both
/// are looked up once, and the window stays small.
#[test]
fn a_huge_shared_entity_does_not_blow_up_a_window() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for n in 0..500i64 {
        b.game(e4)[0x18..0x20].copy_from_slice(&(n % 2).to_le_bytes());
    }
    let container = 1usize << 20;
    let mut lid = lid_header(container as i32, 2);
    for last in ["M".repeat(container - 16), "L".repeat(3000)] {
        let player = strings(&[&last, ""]);
        let mut slot = (player.len() as i32).to_le_bytes().to_vec();
        slot.extend(&player);
        slot.resize(container, 0);
        lid.extend(slot);
    }
    b.lid(lid);
    let db = b.write("api-huge-entity");
    let r = start(&db, vec![], None);
    let w = get(r.port, &format!("/v1/databases/{}/games?limit=500", r.id), "");
    assert_eq!(w.status, 200);
    assert!(w.body.len() < 2 << 20, "window of {} bytes", w.body.len());
    let whites: Vec<&str> =
        w.body.split(r#""white":""#).skip(1).map(|s| s.split('"').next().unwrap()).take(2).collect();
    assert_eq!(whites[0], "", "a 1 MiB name record is not read");
    assert_eq!(whites[1].chars().count(), bridge::api::MAX_FIELD_CHARS + 1);
    assert!(whites[1].starts_with('L') && whites[1].ends_with('…'));
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
    // That connection's slot was freed before it closed, so the next ones
    // are the only ones counted.
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

/// An annotation on a move the game does not have is a damaged record.
#[test]
fn an_annotation_on_no_move_is_an_unreadable_game() {
    let content = annotations(&[(2, vec![text(false, language::ENGLISH, "past the end")])]);
    let db = annotated_database("api-annotation-no-move", &content);
    let r = start(&db, vec![], None);
    let g = get(r.port, &format!("/v1/databases/{}/games/1", r.id), "");
    assert_eq!(g.status, 422, "{}", g.body);
    assert!(g.body.contains(r#""code":"unreadable_game""#) && g.body.contains("position 2"), "{}", g.body);
}

#[test]
fn search_sort_and_unsupported_qualifiers() {
    let db = database("api-search", 6, 3, 0);
    let r = start(&db, vec![], None);
    let list = |q: &str| get(r.port, &format!("/v1/databases/{}/games{q}", r.id), "");
    let numbers = |reply: &Reply| -> Vec<u32> {
        reply.body.split(r#""number":"#).skip(1).map(|s| s.split(',').next().unwrap().parse().unwrap()).collect()
    };
    // Record 3 is a guiding text: a qualifier keeps it out, a bare word does not.
    let w = list("?q=player:morphy&limit=2&offset=1");
    assert_eq!(w.status, 200, "{}", w.body);
    assert!(w.body.contains(r#""total":5,"offset":1,"sort":"number-asc""#), "{}", w.body);
    assert_eq!(numbers(&w), [2, 4]);
    assert_eq!(numbers(&list("?q=result:1-0+sort:number-desc")), [6, 5, 4, 2, 1]);
    let by_param = list("?q=result:1-0+sort:number-desc&sort=number");
    assert!(by_param.body.contains(r#""sort":"number-asc""#), "the URL's sort wins: {}", by_param.body);
    assert_eq!(numbers(&list("?sort=moves")).len(), 6);
    let e = list("?q=tag:endgame");
    assert_eq!(e.status, 400);
    assert!(
        e.body.contains(r#""code":"unsupported_qualifier""#) && e.body.contains(r#""qualifier":"tag""#),
        "{}",
        e.body
    );
    let s = get(r.port, &format!("/v1/databases/{}/suggest?field=player&prefix=mor", r.id), "");
    assert_eq!(s.status, 200, "{}", s.body);
    assert!(
        s.body.contains(
            r#"{"field":"player","suggestions":[{"value":"Morphy, Paul","label":"Morphy, Paul","games":5}]}"#
        ),
        "{}",
        s.body
    );
    for (q, parameter) in [
        ("?field=colour&prefix=m", "field"),
        ("?field=player&prefix=+", "prefix"),
        ("?field=event&prefix=p&limit=21", "limit"),
    ] {
        let e = get(r.port, &format!("/v1/databases/{}/suggest{q}", r.id), "");
        assert!(e.status == 400 && e.body.contains(&format!(r#""parameter":"{parameter}""#)), "{q}: {}", e.body);
    }
}

/// An event's name is matched from its start: a comma in it does not begin a
/// first name, as it does for people. Twenty events seen more often, whose
/// names end in ", Paris", do not push «Paris Open» out of a `Paris` prefix.
#[test]
fn event_suggestions_match_the_start_of_the_name() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    let mut entities = Vec::new();
    for t in 0..21usize {
        let title = if t == 20 { "Paris Open".to_string() } else { format!("Other event {t:02}, Paris") };
        let mut record = strings(&["", &title]);
        record.extend(0i32.to_le_bytes());
        entities.push((1, t, record));
        for _ in 0..if t == 20 { 1 } else { 2 } {
            b.game(e4)[0x28..0x30].copy_from_slice(&(t as i64).to_le_bytes());
        }
    }
    b.lid(lid_with(64, 21, &entities));
    let db = b.write("api-event-prefix");
    let r = start(&db, vec![], None);
    let s = get(r.port, &format!("/v1/databases/{}/suggest?field=event&prefix=Paris&limit=20", r.id), "");
    assert_eq!(s.status, 200, "{}", s.body);
    assert!(s.body.contains(r#"{"value":"Paris Open","label":"Paris Open","games":1}"#), "{}", s.body);
    assert!(!s.body.contains("Other event"), "{}", s.body);
}

/// A person's first name comes from its own field: a last name with a comma in
/// it, `Smith, Jr.` with first name `Alex`, is offered for `Alex`, not for
/// `Jr.`, as a player and as an annotator.
#[test]
fn first_names_come_from_their_own_field() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    let rec = b.game(e4);
    rec[0x18..0x20].copy_from_slice(&1i64.to_le_bytes());
    rec[0x30..0x38].copy_from_slice(&1i64.to_le_bytes());
    b.lid(lid_with(64, 2, &[(0, 1, strings(&["Smith, Jr.", "Alex"]))]));
    let db = b.write("api-first-name-field");
    let r = start(&db, vec![], None);
    for field in ["player", "annotator"] {
        let s = get(r.port, &format!("/v1/databases/{}/suggest?field={field}&prefix=Alex", r.id), "");
        assert_eq!(s.status, 200, "{}", s.body);
        assert!(s.body.contains(r#""value":"Smith, Jr., Alex""#), "{field}: {}", s.body);
        let s = get(r.port, &format!("/v1/databases/{}/suggest?field={field}&prefix=Jr.", r.id), "");
        assert!(s.body.contains(r#""suggestions":[]"#), "{field}: {}", s.body);
    }
}

#[test]
fn a_changed_database_is_searched_afresh() {
    let db = database("api-search-fresh", 2, 0, 0);
    let r = start(&db, vec![], None);
    let total = |reply: Reply| reply.body.split(r#""total":"#).nth(1).unwrap().split(',').next().unwrap().to_string();
    let path = format!("/v1/databases/{}/games?q=player:morphy+sort:white", r.id);
    assert_eq!(total(get(r.port, &path, "")), "2");
    let bigger = database("api-search-fresh-bigger", 5, 0, 0);
    for f in ["db.2cbh", "db.2cbg", "db.2lid"] {
        std::fs::copy(bigger.dir().join(f), db.dir().join(f)).unwrap();
    }
    assert_eq!(total(get(r.port, &path, "")), "5", "the kept result and sort order belong to the old generation");
}

/// Percent-encodes everything but unreserved characters, for a query value.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A suggestion's `value` is the complete name, usable verbatim in a quoted
/// qualifier, and its `label` is clipped for display: two names that share
/// their first 215 characters are two values, each finding its own game. A
/// name longer than a query value can hold is not offered.
#[test]
fn suggestions_carry_the_complete_value_and_a_clipped_label() {
    let prefix = "é".repeat(210);
    let (a, b, long) = (format!("{prefix}TailA"), format!("{prefix}TailB"), format!("{prefix}{}", "x".repeat(60)));
    let mut builder = Builder::new();
    let e4 = builder.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for id in 0..3i64 {
        let g = builder.game(e4);
        g[0x18..0x20].copy_from_slice(&id.to_le_bytes());
        g[0x20..0x28].copy_from_slice(&id.to_le_bytes());
    }
    let players: Vec<(usize, usize, Vec<u8>)> =
        [&a, &b, &long].iter().enumerate().map(|(id, name)| (0, id, strings(&[name, ""]))).collect();
    builder.lid(lid_with(1024, 3, &players));
    let db = builder.write("api-suggest-long");
    let r = start(&db, vec![], None);
    let s = get(r.port, &format!("/v1/databases/{}/suggest?field=player&prefix={}", r.id, encode("éé")), "");
    assert_eq!(s.status, 200, "{}", s.body);
    let label: String = prefix.chars().take(200).collect::<String>() + "…";
    for name in [&a, &b] {
        let item = format!(r#"{{"value":"{name}","label":"{label}","games":1}}"#);
        assert!(s.body.contains(&item), "{name}: {}", s.body);
        let q = encode(&format!("player:\"{name}\""));
        let w = get(r.port, &format!("/v1/databases/{}/games?q={q}", r.id), "");
        assert!(w.body.contains(r#""total":1,"#), "{}", w.body);
    }
    assert!(!s.body.contains("xxx"), "a name of 270 characters is not offered: {}", s.body);
}
