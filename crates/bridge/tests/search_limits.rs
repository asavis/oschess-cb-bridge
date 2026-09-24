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
use bridge::search::Indexes;
use bridge::server;
use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

fn get(port: u16, path: &str) -> (u16, String) {
    try_get(port, path).expect("the bridge answers")
}

/// [`get`], or `None` when the connection is refused or cut: during a start,
/// the port may belong to another test's bridge that has just ended.
fn try_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(raw.as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    let status = out.split(' ').nth(1)?.parse().ok()?;
    Some((status, out))
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
        engine: bridge::engine::Engine::none(),
    });
    let served = app.clone();
    std::thread::spawn(move || server::serve(listeners, served));
    Served { port, id: id_of(&path), app, _db: db }
}

impl Served {
    /// The indexes searches on this database use, where a test holds them.
    fn indexes(&self) -> Arc<Indexes> {
        self.app.catalog.get(&self.id).unwrap().open().ok().unwrap().indexes
    }

    /// Sends `query` on its own connection.
    fn send(&self, query: &str) -> std::thread::JoinHandle<(u16, String)> {
        let (port, path) = (self.port, format!("/v1/databases/{}/games?{query}", self.id));
        std::thread::spawn(move || get(port, &path))
    }

    fn get(&self, query: &str) -> (u16, String) {
        get(self.port, &format!("/v1/databases/{}/games?{query}", self.id))
    }
}

/// How long a test waits for a search to be held.
const ARRIVAL: Duration = Duration::from_secs(30);

/// A search with `q` in a stream makes the one still running in the same
/// stream answer `409 superseded`, and the newer one is served.
#[test]
fn a_superseded_search_answers_409() {
    let s = serve_sparse("limits-superseded", 100_000);
    let indexes = s.indexes();
    let held = indexes.gate().hold(1);
    let first = s.send("q=needle&stream=tab-1");
    assert!(held.arrived(1, ARRIVAL));
    let (status, out) = s.get("q=other&stream=tab-1");
    assert_eq!(status, 200, "{out}");
    drop(held);
    let (status, out) = first.join().unwrap();
    assert_eq!(status, 409, "{out}");
    assert!(out.contains(r#""code":"superseded""#), "{out}");
}

/// Clearing the search box is a new query too: an empty `q=` supersedes.
#[test]
fn an_empty_q_supersedes_the_running_search() {
    let s = serve_sparse("limits-empty-q", 100_000);
    let indexes = s.indexes();
    let held = indexes.gate().hold(1);
    let first = s.send("q=needle&stream=tab-1");
    assert!(held.arrived(1, ARRIVAL));
    let (status, out) = s.get("q=&stream=tab-1");
    assert_eq!(status, 200, "{out}");
    drop(held);
    assert_eq!(first.join().unwrap().0, 409);
}

/// Another stream, another origin or no stream at all never stops a search.
#[test]
fn other_streams_do_not_supersede() {
    let s = serve_sparse("limits-streams", 100_000);
    let indexes = s.indexes();
    let held = indexes.gate().hold(2);
    let first = s.send("q=needle&stream=oschess-tab");
    assert!(held.arrived(1, ARRIVAL));
    let second = s.send("q=other&stream=staging-tab");
    assert!(held.arrived(2, ARRIVAL));
    let (status, out) = s.get("q=third");
    assert_eq!(status, 200, "{out}");
    drop(held);
    assert_eq!(first.join().unwrap().0, 200);
    assert_eq!(second.join().unwrap().0, 200);
    for bad in ["", "has%20space", "x%2Fy"] {
        let (status, out) = s.get(&format!("q=x&stream={bad}"));
        assert!(status == 400 && out.contains(r#""parameter":"stream""#), "{bad}: {out}");
    }
    let (status, out) = s.get(&format!("q=x&stream={}", "a".repeat(65)));
    assert_eq!(status, 400, "{out}");
}

/// The bridge as a separate process under a 256 MiB address-space limit,
/// serving `path`, with `env` set; killed when dropped.
struct Limited {
    child: std::process::Child,
    port: u16,
    id: String,
}

impl Limited {
    /// Starts the bridge on a port found free. Another test's bridge may take
    /// that port first, and this one then fails to bind and ends: the bridge
    /// counts as started only while it runs and lists this database, and
    /// otherwise starts again on another port.
    fn start(path: &Path, home: &Path, env: &[(&str, &str)]) -> Limited {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("token"), TOKEN).unwrap();
        let id = id_of(path);
        for _ in 0..10 {
            let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
            std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg("ulimit -v 262144 && exec \"$0\" --database \"$1\"")
                .arg(env!("CARGO_BIN_EXE_oschess-bridge"))
                .arg(path)
                .env("OSCHESS_BRIDGE_HOME", home)
                .env("MALLOC_ARENA_MAX", "2")
                .stdout(std::process::Stdio::null());
            for (k, v) in env {
                command.env(k, v);
            }
            let mut limited = Limited { child: command.spawn().unwrap(), port, id: id.clone() };
            if limited.serves() {
                return limited;
            }
        }
        panic!("the bridge did not start");
    }

    /// Whether this bridge runs and serves its database, waiting for it to start.
    fn serves(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return false;
            }
            if let Some((_, body)) = try_get(self.port, "/v1/databases") {
                let listed = body.contains(&format!(r#""id":"{}""#, self.id));
                return listed && self.child.try_wait().unwrap().is_none();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Limited {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A header file claiming 4,294,967,295 records, of 824 GB but a few bytes on
/// disk, is answered `422 database_too_large` when sorted: the order would
/// need 51 GB, and nothing is allocated for it. Under the process's 256 MiB
/// address-space limit, allocating it would abort the bridge.
#[test]
fn a_database_too_large_to_sort_is_refused_under_a_memory_limit() {
    let db = sparse("limits-too-large", u64::from(u32::MAX));
    let path = db.dir().join("db.2cbh");
    let b = Limited::start(&path, &db.dir().join("home"), &[("OSCHESS_BRIDGE_THREADS", "1")]);
    let (status, out) = get(b.port, &format!("/v1/databases/{}/games?sort=date", b.id));
    assert_eq!(status, 422, "{out}");
    assert!(out.contains(r#""code":"database_too_large""#), "{out}");
    let (status, out) = get(b.port, &format!("/v1/databases/{}/games?limit=1", b.id));
    assert_eq!(status, 200, "number order needs no sort: {out}");
    assert_eq!(get(b.port, "/v1/status").0, 200, "the bridge is still running");
}

/// Sixteen workers with a 16 MiB search budget: their batch buffers together
/// would take more than the whole budget, so a pass runs on as many as fit
/// instead of calling a small database too large.
#[test]
fn more_workers_than_the_budget_holds_buffers_for_still_search() {
    let db = sparse("limits-many-workers", 300_000);
    let path = db.dir().join("db.2cbh");
    let env = [("OSCHESS_BRIDGE_THREADS", "16"), ("OSCHESS_BRIDGE_SEARCH_MIB", "16")];
    let b = Limited::start(&path, &db.dir().join("home"), &env);
    for query in ["q=moves:12345", "sort=date"] {
        let (status, out) = get(b.port, &format!("/v1/databases/{}/games?{query}", b.id));
        assert_eq!(status, 200, "{query}: {out}");
    }
}

/// The same sixteen workers and 16 MiB, on a query that matches every one of
/// 100,000 games: each worker's list of matches needs room of its own beside
/// the batch buffers, so the buffers leave half the budget free for them.
#[test]
fn many_workers_leave_room_for_their_matches() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for _ in 0..100_000 {
        b.game(e4)[0x8a..0x8c].copy_from_slice(&21i16.to_le_bytes());
    }
    let db = b.write("limits-many-matches");
    let path = db.dir().join("db.2cbh");
    let env = [("OSCHESS_BRIDGE_THREADS", "16"), ("OSCHESS_BRIDGE_SEARCH_MIB", "16")];
    let b = Limited::start(&path, &db.dir().join("home"), &env);
    for query in ["q=moves:21&limit=1", "q=moves:21&sort=date&limit=1"] {
        let (status, out) = get(b.port, &format!("/v1/databases/{}/games?{query}", b.id));
        assert_eq!(status, 200, "{query}: {out}");
        assert!(out.contains(r#""total":100000"#), "{query}: {out}");
    }
}

/// A `.2lid` holding only players, `names[id]` for each id; id 0 is empty.
fn players_lid(names: &[String]) -> Vec<u8> {
    let field = |s: &str| [(s.len() as i32).to_le_bytes().to_vec(), s.as_bytes().to_vec()].concat();
    let size = 64usize;
    let mut d = Vec::new();
    d.extend(184i32.to_be_bytes());
    d.extend(6i32.to_be_bytes());
    for _ in 0..6 {
        d.extend((size as i32).to_be_bytes());
        d.extend((names.len() as i64).to_be_bytes());
        d.extend((-1i64).to_be_bytes());
    }
    d.resize(184, 0);
    for name in names {
        let record = if name.is_empty() { Vec::new() } else { [field(name), field("")].concat() };
        let mut player = vec![0u8; size];
        if !record.is_empty() {
            player[..4].copy_from_slice(&(record.len() as i32).to_le_bytes());
            player[4..4 + record.len()].copy_from_slice(&record);
        }
        d.extend(player);
        d.extend(vec![0u8; size * 5]);
    }
    d
}

/// Name tables load on the shared workers too: 100,000 short player names,
/// sixteen workers and a 16 MiB budget. Each worker grows its share of the
/// table in steps that scale with the budget, so sixteen shares fit beside
/// the table's offsets instead of being refused as too large.
#[test]
fn many_names_load_on_many_workers_within_a_small_budget() {
    let names: Vec<String> = std::iter::once(String::new()).chain((1..=100_000).map(|i| format!("a{i:06}"))).collect();
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for id in 1..names.len() as i64 {
        b.game(e4)[0x18..0x20].copy_from_slice(&id.to_le_bytes());
    }
    b.lid(players_lid(&names));
    let db = b.write("limits-many-names");
    let path = db.dir().join("db.2cbh");
    let env = [("OSCHESS_BRIDGE_THREADS", "16"), ("OSCHESS_BRIDGE_SEARCH_MIB", "16")];
    let b = Limited::start(&path, &db.dir().join("home"), &env);
    for (query, total) in [("q=player:a&limit=1", "100000"), ("q=player:nomatch&limit=1", "0")] {
        let (status, out) = get(b.port, &format!("/v1/databases/{}/games?{query}", b.id));
        assert_eq!(status, 200, "{query}: {out}");
        assert!(out.contains(&format!(r#""total":{total}"#)), "{query}: {out}");
    }
    let (status, out) = get(b.port, &format!("/v1/databases/{}/suggest?field=player&prefix=a0&limit=3", b.id));
    assert_eq!(status, 200, "{out}");
}

/// A player table of `slots` 1 MiB containers, a hole on disk except for a
/// 1 MiB name at the start of each worker's range of 4,096 ids.
fn huge_names_lid(path: &Path, slots: i64) {
    use std::io::{Seek, SeekFrom};
    let container: i32 = 1 << 20;
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&cbformat::fixture::lid_header(container, slots)).unwrap();
    let mut record = vec![0u8; container as usize];
    let last = container as usize - 12;
    record[..4].copy_from_slice(&(last as i32 + 8).to_le_bytes());
    record[4..8].copy_from_slice(&(last as i32).to_le_bytes());
    record[8..8 + last].fill(b'x');
    for id in (1..slots).step_by(4096) {
        f.seek(SeekFrom::Start(184 + id as u64 * u64::from(container as u32))).unwrap();
        f.write_all(&record).unwrap();
    }
    f.set_len(184 + slots as u64 * u64::from(container as u32)).unwrap();
}

/// Name records of 1 MiB at the start of each of sixteen workers' ranges,
/// under the 256 MiB limit and a 16 MiB budget, with requests arriving at
/// once: a name record is read to at most 4 KiB, so a longer one is an empty
/// name, and every request gets an answer while the bridge keeps serving.
#[test]
fn huge_name_records_are_never_read_whole() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4)[0x18..0x20].copy_from_slice(&1i64.to_le_bytes());
    let db = b.write("limits-huge-names");
    huge_names_lid(&db.dir().join("db.2lid"), 16 * 4096);
    let path = db.dir().join("db.2cbh");
    let env = [("OSCHESS_BRIDGE_THREADS", "16"), ("OSCHESS_BRIDGE_SEARCH_MIB", "16")];
    let b = Limited::start(&path, &db.dir().join("home"), &env);
    let answers: Vec<(u16, String)> = std::thread::scope(|s| {
        let running: Vec<_> = (0..8)
            .map(|n| {
                let path = format!("/v1/databases/{}/games?q=player:absent{n}&limit=1", b.id);
                let port = b.port;
                s.spawn(move || get(port, &path))
            })
            .collect();
        running.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (status, out) in &answers {
        assert!(*status == 200 || (*status == 503 && out.contains(r#""code":"busy""#)), "{out}");
    }
    assert_eq!(get(b.port, "/v1/status").0, 200, "the bridge is still running");
    let (status, out) = get(b.port, &format!("/v1/databases/{}/games?q=player:x&limit=1", b.id));
    assert_eq!(status, 200, "{out}");
    assert!(out.contains(r#""total":0"#), "a name over 4 KiB is empty: {out}");
}

/// Two dozen searches at once, under the 256 MiB limit, four workers and a
/// 16 MiB search budget: every connection gets an answer, `200` or `503`, and
/// the bridge keeps serving. Scans share the four workers instead of starting
/// their own, and each worker's batch buffer is reserved first.
#[test]
fn concurrent_searches_under_a_memory_limit_all_get_an_answer() {
    let db = sparse("limits-concurrent", 2_000_000);
    let path = db.dir().join("db.2cbh");
    let env = [("OSCHESS_BRIDGE_THREADS", "4"), ("OSCHESS_BRIDGE_SEARCH_MIB", "16")];
    let b = Limited::start(&path, &db.dir().join("home"), &env);
    let answers: Vec<(u16, String)> = std::thread::scope(|s| {
        let running: Vec<_> = (0..24)
            .map(|n| {
                let path = format!("/v1/databases/{}/games?q=moves:{}", b.id, 1000 + n);
                let port = b.port;
                s.spawn(move || get(port, &path))
            })
            .collect();
        running.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (status, out) in &answers {
        assert!(*status == 200 || (*status == 503 && out.contains(r#""code":"busy""#)), "{out}");
    }
    assert!(answers.iter().any(|(status, _)| *status == 200));
    assert_eq!(get(b.port, "/v1/status").0, 200, "the bridge is still running");
    let (status, out) = get(b.port, &format!("/v1/databases/{}/games?q=moves:5", b.id));
    assert_eq!(status, 200, "{out}");
}
