//! Searches over HTTP at their limits: a superseded search, and a database far
//! too large to sort. Both use sparse files, which need a Unix file system.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::server;
use cbformat::fixture::{Builder, TempDb, quiet};
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

/// One game, then `records - 1` headers that are a hole in the file.
fn sparse(name: &str, records: u64) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    let db = b.write(name);
    let file = std::fs::OpenOptions::new().write(true).open(db.dir().join("db.2cbh")).unwrap();
    file.set_len((records + 1) * 192).unwrap();
    db
}

struct Served {
    port: u16,
    id: String,
    app: Arc<App>,
    _db: TempDb,
}

fn serve_sparse(name: &str, records: u64) -> Served {
    let db = sparse(name, records);
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let path = db.dir().join("db.2cbh");
    let app = Arc::new(App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new([path.clone()]),
        between_reads: None,
    });
    let served = app.clone();
    std::thread::spawn(move || server::serve(listeners, served));
    Served { port, id: id_of(&path), app, _db: db }
}

impl Served {
    /// Starts `query` on its own connection, and returns once its scan has read
    /// headers.
    fn start(&self, query: &str) -> std::thread::JoinHandle<(u16, String)> {
        let indexes = self.app.catalog.get(&self.id).unwrap().open().ok().unwrap().indexes;
        let before = indexes.scanned();
        let (port, path) = (self.port, format!("/v1/databases/{}/games?{query}", self.id));
        let running = std::thread::spawn(move || get(port, &path));
        while indexes.scanned() == before && !running.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        running
    }

    fn get(&self, query: &str) -> (u16, String) {
        get(self.port, &format!("/v1/databases/{}/games?{query}", self.id))
    }
}

/// A search with `q` in a stream makes the one still running in the same
/// stream answer `409 superseded`, and the newer one is served.
#[test]
fn a_superseded_search_answers_409() {
    let s = serve_sparse("limits-superseded", 8_000_000);
    let first = s.start("q=needle&stream=tab-1");
    let (status, out) = s.get("q=other&stream=tab-1");
    assert_eq!(status, 200, "{out}");
    let (status, out) = first.join().unwrap();
    assert_eq!(status, 409, "{out}");
    assert!(out.contains(r#""code":"superseded""#), "{out}");
}

/// Clearing the search box is a new query too: an empty `q=` supersedes.
#[test]
fn an_empty_q_supersedes_the_running_search() {
    let s = serve_sparse("limits-empty-q", 8_000_000);
    let first = s.start("q=needle&stream=tab-1");
    let (status, out) = s.get("q=&stream=tab-1");
    assert_eq!(status, 200, "{out}");
    assert_eq!(first.join().unwrap().0, 409);
}

/// Another stream, another origin or no stream at all never stops a search.
#[test]
fn other_streams_do_not_supersede() {
    let s = serve_sparse("limits-streams", 8_000_000);
    let first = s.start("q=needle&stream=oschess-tab");
    let second = s.start("q=other&stream=staging-tab");
    let (status, out) = s.get("q=third");
    assert_eq!(status, 200, "{out}");
    assert_eq!(first.join().unwrap().0, 200);
    assert_eq!(second.join().unwrap().0, 200);
    for bad in ["", "has%20space", "x%2Fy"] {
        let (status, out) = s.get(&format!("q=x&stream={bad}"));
        assert!(status == 400 && out.contains(r#""parameter":"stream""#), "{bad}: {out}");
    }
    let (status, out) = s.get(&format!("q=x&stream={}", "a".repeat(65)));
    assert_eq!(status, 400, "{out}");
}

/// A header file claiming 4,294,967,295 records, of 824 GB but a few bytes on
/// disk, is answered `422 database_too_large` when sorted: the order would
/// need 51 GB, and nothing is allocated for it. The bridge runs with a 256 MiB
/// address-space limit, under which allocating it would abort the process.
#[test]
fn a_database_too_large_to_sort_is_refused_under_a_memory_limit() {
    let db = sparse("limits-too-large", u64::from(u32::MAX));
    let home = db.dir().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
    std::fs::write(home.join("token"), TOKEN).unwrap();
    let path = db.dir().join("db.2cbh");
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("ulimit -v 262144 && exec \"$0\" --database \"$1\"")
        .arg(env!("CARGO_BIN_EXE_oschess-bridge"))
        .arg(&path)
        .env("OSCHESS_BRIDGE_HOME", &home)
        .env("OSCHESS_BRIDGE_THREADS", "1")
        .env("MALLOC_ARENA_MAX", "2")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let up = |deadline: Instant| {
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    };
    let result = std::panic::catch_unwind(|| {
        assert!(up(Instant::now() + Duration::from_secs(20)), "the bridge did not start");
        let id = id_of(Path::new(&path));
        let (status, out) = get(port, &format!("/v1/databases/{id}/games?sort=date"));
        assert_eq!(status, 422, "{out}");
        assert!(out.contains(r#""code":"database_too_large""#), "{out}");
        let (status, out) = get(port, &format!("/v1/databases/{id}/games?limit=1"));
        assert_eq!(status, 200, "number order needs no sort: {out}");
        assert_eq!(get(port, "/v1/status").0, 200, "the bridge is still running");
    });
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
