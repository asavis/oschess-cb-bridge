//! PGN files through the HTTP API: opened in the background while their index
//! is built, then listed, searched and served like the other formats, and
//! answering as a 2CBH copy of the same games answers.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::{App, MAX_GAME_BYTES};
use bridge::catalog::{Catalog, State, id_of};
use bridge::search::{self, Indexes, SearchError, Selection};
use bridge::server;
use bridge::store::Any;
use cbformat::codepage::CodePage;
use cbformat::fixture::pgn_file;
use cbformat::pgnfile;

mod common;
use common::{block, fixture_of, pgn_fixture, rows};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-pgn-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Serves `paths`, with the PGN and position indexes in `dir`.
fn start(paths: &[PathBuf], dir: &Path) -> (u16, Arc<App>) {
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new(paths.to_vec()),
        between_reads: None,
        engine: bridge::engine::Engine::none(),
    };
    app.catalog.explorer.set_dir(dir.join("index"));
    app.catalog.pgn().set_dir(dir.join("pgn"));
    let app = Arc::new(app);
    let served = Arc::clone(&app);
    std::thread::spawn(move || server::serve(listeners, served));
    (port, app)
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

/// Asks for `path` until the answer is no longer a `409` for a database or an
/// index still being prepared.
fn get_ready(port: u16, path: &str) -> (u16, String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let answer = get(port, path);
        if answer.0 != 409 {
            return answer;
        }
        assert!(Instant::now() < deadline, "{path} stayed {}", answer.1);
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_ready(catalog: &Catalog, path: &Path) {
    let entry = catalog.get(&id_of(path)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while entry.state() != State::Ready {
        assert!(Instant::now() < deadline, "{} stayed {:?}", path.display(), entry.state());
        std::thread::sleep(Duration::from_millis(20));
    }
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

/// The fixture of `docs/search-grammar.md` with what PGN cannot hold made a
/// game: the guiding text and the deleted game are games, and every game has
/// at least one move, so that both copies play `1. e4` in each.
fn pgn_rows() -> Vec<String> {
    rows(&[])
        .into_iter()
        .map(|line| {
            let mut f: Vec<String> = line.split('|').map(|x| x.trim().to_string()).collect();
            f[1] = "game".into();
            if f[9] == "0" {
                f[9] = "1".into();
            }
            f.join(" | ")
        })
        .collect()
}

/// The same games as PGN and as 2CBH, served together: the PGN copy is first
/// `opening`, then `ready`, and its lists, searches, sorts, suggestions and
/// explorer answers are the 2CBH copy's.
#[test]
fn a_pgn_copy_answers_as_its_2cbh_copy() {
    let rows = pgn_rows();
    let (fp, f2) = (pgn_fixture("api-pgn", &rows), fixture_of("pgn-api-2cbh", &rows));
    let (pp, p2) = (fp.dir().join("db.pgn"), f2.dir().join("db.2cbh"));
    let dir = scratch("copies");
    let (port, app) = start(&[pp.clone(), p2.clone()], &dir);
    let (ip, i2) = (id_of(&pp), id_of(&p2));
    wait_ready(&app.catalog, &pp);
    let (status, body) = get(port, "/v1/databases");
    assert_eq!(status, 200);
    let listed = format!(r#""id":"{ip}","name":"db","format":"pgn","state":"ready","records":10,"generation":""#);
    assert!(body.contains(&listed), "{body}");
    assert!(get(port, "/v1/status").1.contains(r#""ready":2,"opening":0"#));

    let both = |path: &str| {
        let (a, b) = (get(port, &path.replace("{id}", &ip)), get(port, &path.replace("{id}", &i2)));
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
        "q=moves%3A%3E30&sort=moves",
        "q=elo%3A%3E%3D2700",
        "q=date%3A1914&sort=black-desc",
        "q=result%3A1-0",
        "sort=tournament",
        "sort=annotator-desc",
        "sort=round",
        "sort=result",
        "q=tag%3Ax",
    ] {
        both(&format!("/v1/databases/{{id}}/games?{query}"));
    }
    for field in ["player", "event", "annotator"] {
        for prefix in ["a", "c", "l", "m", "mik", "n", "st", "t"] {
            both(&format!("/v1/databases/{{id}}/suggest?field={field}&prefix={prefix}"));
        }
    }
    let start_fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR%20w%20KQkq%20-%200%201";
    let url = format!("/v1/databases/{{id}}/explorer?fen={start_fen}");
    get_ready(port, &url.replace("{id}", &ip));
    get_ready(port, &url.replace("{id}", &i2));
    let (status, body) = both(&url);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""index":{"records":10,"games":10,"#), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn numbers<'a>(db: impl Into<Any<'a>>, idx: &Indexes, q: &str) -> String {
    let db = db.into();
    match search::select(db, idx, Some(q), None, None) {
        Ok((Selection::All { descending }, _)) => {
            let all: Vec<u32> =
                if descending { (1..=db.record_count()).rev().collect() } else { (1..=db.record_count()).collect() };
            format!("{all:?}")
        }
        Ok((Selection::Numbers(v), _)) => format!("{:?}", v.to_vec()),
        Err(SearchError::Unsupported(q)) => format!("unsupported {q}"),
        Err(e) => panic!("{e:?}"),
    }
}

/// Every query of the conformance corpus finds on the PGN copy what it finds
/// on the 2CBH copy.
#[test]
fn the_conformance_corpus_holds_on_a_pgn_copy() {
    let rows = pgn_rows();
    let (fp, f2) = (pgn_fixture("corpus-pgn", &rows), fixture_of("pgn-corpus-2cbh", &rows));
    let (pgn, index) = (fp.dir().join("db.pgn"), fp.dir().join("db.head"));
    pgnfile::build(&pgn, &index, 1, CodePage::WESTERN, &mut |_| true).unwrap();
    let dp = pgnfile::Database::open(&pgn, &index, 1, CodePage::WESTERN).unwrap();
    let d2 = cbformat::v2::Database::open(f2.dir().join("db.2cbh")).unwrap();
    let (ip, i2) = (Indexes::default(), Indexes::default());
    let lines = block("corpus");
    assert!(lines.len() > 50, "the corpus was read");
    let failures: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            let q = line.rsplit_once("=>").unwrap().0.trim();
            let (a, b) = (numbers(&dp, &ip, q), numbers(&d2, &i2, q));
            (a != b).then(|| format!("{q:?}: pgn {a}, 2cbh {b}"))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A game is served as the file writes it, whatever the languages asked for;
/// one over the limit is refused before it is read.
#[test]
fn games_are_served_as_written() {
    let long = format!("[Event \"Long\"]\n\n1. e4 {{{}}} *\n", "x".repeat(MAX_GAME_BYTES));
    let text = format!(
        "[Event \"Paris\"]\r\n[White \"Morphy, Paul\"]\r\n\r\n1. e4 e5 {{A comment}} (1... c5) 2. Nf3 1-0\r\n\r\n{long}"
    );
    let f = pgn_file("served", text.as_bytes());
    let path = f.dir().join("db.pgn");
    let dir = scratch("served");
    let (port, app) = start(std::slice::from_ref(&path), &dir);
    let id = id_of(&path);
    wait_ready(&app.catalog, &path);
    for query in ["", "?lang=de", "?annotations=full"] {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games/1{query}"));
        assert_eq!(status, 200, "{body}");
        let want = r#""pgn":"[Event \"Paris\"]\n[White \"Morphy, Paul\"]\n\n1. e4 e5 {A comment} (1... c5) 2. Nf3 1-0\n","annotations":"complete""#;
        assert!(body.contains(want), "{body}");
    }
    let (status, body) = get(port, &format!("/v1/databases/{id}/games/2"));
    assert_eq!(status, 422, "{body}");
    assert!(body.contains(r#""code":"unreadable_game""#) && body.contains("over the"), "{body}");
    let (status, body) = get(port, &format!("/v1/databases/{id}/games?limit=5"));
    assert_eq!(status, 200);
    assert!(body.contains(r#""number":1,"kind":"game","white":"Morphy, Paul","#), "{body}");
    assert!(body.contains(r#""moves":2,"eco":"","event":"Paris","site":"""#), "{body}");
    assert_eq!(get(port, &format!("/v1/databases/{id}/games/3")).0, 404);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A PGN file is `opening`, with the bytes read, until its index is built;
/// a bridge started again opens it at once; a change reads it again.
#[test]
fn opening_restarting_and_changing() {
    let game = "[Event \"E\"]\n[White \"W\"]\n[Black \"B\"]\n\n1. e4 e5 *\n\n";
    let f = pgn_file("opening", game.repeat(3).as_bytes());
    let path = f.dir().join("db.pgn");
    let dir = scratch("opening");
    let (port, app) = start(std::slice::from_ref(&path), &dir);
    let id = id_of(&path);
    // The build waits behind a job that holds the queue.
    let (release, hold) = mpsc::channel::<()>();
    assert!(app.catalog.pgn().queue().submit(Box::new(move || {
        let _ = hold.recv();
    })));
    let (_, body) = get(port, "/v1/databases");
    let size = game.len() * 3;
    let want = format!(
        r#""id":"{id}","name":"db","format":"pgn","state":"opening","progress":{{"present":0,"total":{size}}}"#
    );
    assert!(body.contains(&want), "{body}");
    assert!(get(port, "/v1/status").1.contains(r#""opening":1"#));
    let (status, body) = get(port, &format!("/v1/databases/{id}/games"));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""state":"opening""#), "{body}");
    release.send(()).unwrap();
    wait_ready(&app.catalog, &path);
    assert!(get(port, "/v1/databases").1.contains(r#""state":"ready","records":3"#));

    // Started again, the bridge reads the index built before: no build runs.
    let catalog = Catalog::new(vec![path.clone()]);
    catalog.pgn().set_dir(dir.join("pgn"));
    catalog.pgn().queue().refuse_starts(true);
    assert_eq!(catalog.get(&id).unwrap().state(), State::Ready);

    // A game more: read again, then ready with it.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&path, game.repeat(4)).unwrap();
    let (_, body) = get(port, "/v1/databases");
    assert!(body.contains(r#""state":"opening""#) || body.contains(r#""records":4"#), "{body}");
    wait_ready(&app.catalog, &path);
    assert!(get(port, "/v1/databases").1.contains(r#""records":4"#));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder in `bridge.toml` serves its PGN files as it serves its ChessBase
/// databases.
#[test]
fn folders_serve_their_pgn_files() {
    let dir = scratch("folder");
    std::fs::create_dir_all(dir.join("sub.pgn")).unwrap();
    for name in ["A.pgn", "B.PGN", "notes.txt"] {
        std::fs::write(dir.join(name), "[Event \"x\"]\n\n*\n").unwrap();
    }
    let found: Vec<String> = bridge::sources::expand(&dir).unwrap().into_iter().map(|l| l.name).collect();
    assert_eq!(found, ["A", "B"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The position index plays each game's main line as written: from its
/// `FEN`, up to a move it cannot play; Chess960 and other variants stay out.
#[test]
fn the_position_index_reads_main_lines() {
    let text = "\
[Event \"1\"]\n[Result \"1-0\"]\n\n1. e4 e5 (1... c5 2. Nf3) 2. Nf3 {x} Nc6 1-0\n\n\
[Event \"2\"]\n[Result \"0-1\"]\n[SetUp \"1\"]\n[FEN \"rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1\"]\n\n1... c5 0-1\n\n\
[Event \"3\"]\n[Result \"1/2-1/2\"]\n\n1. e4 Ke7 2. d4 1/2-1/2\n\n\
[Event \"4\"]\n[Variant \"Chess960\"]\n\n1. e4 e5 *\n\n\
[Event \"5\"]\n[Variant \"Crazyhouse\"]\n\n1. e4 e5 *\n\n\
[Event \"6\"]\n\n1. d4 -- 2. c4 *\n";
    let f = pgn_file("explorer", text.as_bytes());
    let path = f.dir().join("db.pgn");
    let dir = scratch("explorer");
    let (port, _app) = start(std::slice::from_ref(&path), &dir);
    let id = id_of(&path);
    let after_e4 = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR%20b%20KQkq%20-%200%201";
    let (status, body) = get_ready(port, &format!("/v1/databases/{id}/explorer?fen={after_e4}"));
    assert_eq!(status, 200, "{body}");
    // Games 1, 2 and 3 reach it; 3's Ke7 is illegal, so no move of it counts.
    assert!(body.contains(r#""games":3,"white":1,"draws":1,"black":1,"#), "{body}");
    assert!(body.contains(r#""uci":"e7e5","san":"e5","games":1,"white":1,"#), "{body}");
    assert!(body.contains(r#""uci":"c7c5","san":"c5","games":1,"white":0,"draws":0,"black":1"#), "{body}");
    assert!(body.contains(r#""index":{"records":6,"games":4,"#), "{body}");
    // The variation's 2. Nf3 is not the main line's.
    let after_c5 = "rnbqkbnr/pp1ppppp/8/2p5/4P3/8/PPPP1PPP/RNBQKBNR%20w%20KQkq%20-%200%202";
    let (_, body) = get(port, &format!("/v1/databases/{id}/explorer?fen={after_c5}"));
    assert!(body.contains(r#""games":1,"#) && body.contains(r#""moves":[]"#), "{body}");
    // The null move ends game 6's line after 1. d4.
    let after_d4 = "rnbqkbnr/pppppppp/8/8/3P4/8/PPP1PPPP/RNBQKBNR%20b%20KQkq%20-%200%201";
    let (_, body) = get(port, &format!("/v1/databases/{id}/explorer?fen={after_d4}"));
    assert!(body.contains(r#""games":1,"#) && body.contains(r#""moves":[]"#), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `line` members of a games window, in order: `None` for `null`.
fn lines_of(body: &str) -> Vec<Option<String>> {
    body.split(r#""line":"#)
        .skip(1)
        .map(|rest| rest.strip_prefix('"').map(|s| s[..s.find('"').unwrap()].to_string()))
        .collect()
}

/// A PGN game's row carries its main line on request, as its text writes it
/// and as the other formats give it (#81).
#[test]
fn rows_carry_the_main_line() {
    let text = "\
[Event \"1\"]\n\n1. e4 e5 2. Nf3 Nc6 3. Bc4 Nf6 4. O-O Nxe4 5. d3 Nf6 6. Nbd2 *\n\n\
[Event \"2\"]\n\n1. e4 d5 2. exd5 c6 3. dxc6 Qd7 4. cxb7 Nf6 5. bxc8=Q+ Qd8 *\n\n\
[Event \"3\"]\n\n1. f3 e5 2. g4 Qh4# 0-1\n\n\
[Event \"4\"]\n\n1. e4 c5 (1... c6 2. d4) 2. Nf3 *\n\n\
[Event \"5\"]\n\n1. e4 -- 2. d4 *\n\n\
[Event \"6\"]\n[FEN \"4k3/8/8/8/8/8/8/4K2R w K - 0 1\"]\n\n1. O-O *\n\n\
[Event \"7\"]\n[Variant \"Chess960\"]\n\n1. e4 *\n\n\
[Event \"8\"]\n\n1. e4 e5 2. Ke7 *\n\n\
[Event \"9\"]\n\n1. Ke2 *\n\n\
[Event \"10\"]\n\n*\n";
    let f = pgn_file("lines", text.as_bytes());
    let path = f.dir().join("db.pgn");
    let dir = scratch("lines");
    let (port, app) = start(std::slice::from_ref(&path), &dir);
    let id = id_of(&path);
    wait_ready(&app.catalog, &path);
    let (status, body) = get(port, &format!("/v1/databases/{id}/games?limit=20&line=60"));
    assert_eq!(status, 200, "{body}");
    let want: [Option<&str>; 10] = [
        Some("e4 e5 Nf3 Nc6 Bc4 Nf6 O-O Nxe4 d3 Nf6 Nbd2"),
        Some("e4 d5 exd5 c6 dxc6 Qd7 cxb7 Nf6 bxc8=Q+ Qd8"),
        Some("f3 e5 g4 Qh4#"),
        // A variation is not the main line; a null move ends it.
        Some("e4 c5 Nf3"),
        Some("e4"),
        // A set-up position and Chess960 have no line.
        None,
        None,
        // Damage ends the line before it, and before the first move leaves none.
        Some("e4 e5"),
        None,
        Some(""),
    ];
    assert_eq!(lines_of(&body), want.map(|w| w.map(String::from)), "{body}");
    let (_, body) = get(port, &format!("/v1/databases/{id}/games?limit=1&line=2"));
    assert_eq!(lines_of(&body), [Some("e4 e5".to_string())], "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}
