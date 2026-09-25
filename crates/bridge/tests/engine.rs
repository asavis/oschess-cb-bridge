//! `/v1/engine/analyze` over real loopback connections, with the scripted
//! engine of `tests/fake-uci` (#52).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::Catalog;
use bridge::engine::{self, Engine, EngineConfig};
use bridge::server;
use bridge::sources::Sources;

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const ORIGIN: &str = "https://oschess.org";

fn fake() -> EngineConfig {
    EngineConfig::new(env!("CARGO_BIN_EXE_fake-uci").into(), Some(1), Some(16))
}

/// A server with `engine`, and the port it listens on.
fn start(engine: Engine) -> (u16, Arc<App>) {
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let app = Arc::new(App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new(Vec::new()),
        between_reads: None,
        engine,
    });
    let served = app.clone();
    std::thread::spawn(move || server::serve(listeners, served));
    (port, app)
}

/// An analysis being read: the response head, then its lines one by one.
struct Analysis {
    status: u16,
    head: String,
    reader: BufReader<TcpStream>,
}

impl Analysis {
    fn open(port: u16, query: &str) -> Analysis {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let raw = format!(
            "GET /v1/engine/analyze?{query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {ORIGIN}\r\nConnection: close\r\n\r\n"
        );
        (&s).write_all(raw.as_bytes()).unwrap();
        let mut reader = BufReader::new(s);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line);
        }
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        Analysis { status, head, reader }
    }

    /// The next line of the chunked body; `None` at its end.
    fn line(&mut self) -> Option<String> {
        let mut size = String::new();
        self.reader.read_line(&mut size).ok()?;
        let size = usize::from_str_radix(size.trim(), 16).ok()?;
        if size == 0 {
            return None;
        }
        let mut chunk = vec![0; size + 2];
        self.reader.read_exact(&mut chunk).ok()?;
        assert!(chunk.ends_with(b"\n\r\n"), "{chunk:?}");
        Some(String::from_utf8(chunk[..size - 1].to_vec()).unwrap())
    }

    /// Every line to the end of the body.
    fn rest(&mut self) -> Vec<String> {
        std::iter::from_fn(|| self.line()).collect()
    }

    /// The lines to the end of the body, or `None` if it goes on past `limit`.
    fn rest_within(&mut self, limit: Duration) -> Option<Vec<String>> {
        let deadline = Instant::now() + limit;
        let mut lines = Vec::new();
        while Instant::now() < deadline {
            match self.line() {
                Some(line) => lines.push(line),
                None => return Some(lines),
            }
        }
        None
    }

    /// The whole body of a response that is not streamed.
    fn body(mut self) -> String {
        let mut body = String::new();
        let _ = self.reader.read_to_string(&mut body);
        body
    }
}

const BEST: &str = r#"{"bestmove":"e2e4"}"#;

/// An analysis counts as work a restart would lose while it streams, for a
/// bounded time (#61): an update waits for it, and not for one left running.
#[test]
fn an_analysis_is_work_while_it_streams() {
    let (port, app) = start(Engine::new(fake()));
    assert!(!app.engine.analyzing(Duration::from_secs(60)), "none yet");
    let mut a = Analysis::open(port, "stream=tab1");
    assert_eq!(a.status, 200);
    assert!(a.line().is_some(), "it streams");
    assert!(app.engine.analyzing(Duration::from_secs(60)));
    assert!(!app.engine.analyzing(Duration::ZERO), "one running past the bound counts as none");
    drop(a);
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.engine.analyzing(Duration::from_secs(60)) {
        assert!(Instant::now() < deadline, "the analysis ends when its client leaves");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn streams_the_lines_of_a_search_to_its_best_move() {
    let (port, app) = start(Engine::new(fake()));
    assert_eq!(app.engine.name().as_deref(), Some("fake-uci"));
    let mut a = Analysis::open(port, "moves=e2e4+e7e5&multipv=2&depth=6&stream=tab1");
    assert_eq!(a.status, 200);
    assert!(a.head.contains("Content-Type: application/x-ndjson"), "{}", a.head);
    assert!(a.head.contains("Transfer-Encoding: chunked"));
    assert!(a.head.contains(&format!("Access-Control-Allow-Origin: {ORIGIN}")), "{}", a.head);
    let lines = a.rest();
    assert_eq!(lines.last().unwrap(), BEST, "{lines:?}");
    // The deepest lines of both numbers are written before the best move.
    for k in [1, 2] {
        let deepest = format!(r#"{{"info":{{"depth":6,"seldepth":8,"multipv":{k},"score":{{"cp":{}}},"#, 10 * k);
        assert!(lines.iter().any(|l| l.starts_with(&deepest)), "{k}: {lines:?}");
    }
    assert!(lines.iter().all(|l| !l.contains("searching")));
    // The engine said its own name.
    assert_eq!(app.engine.name().as_deref(), Some("Fake UCI 1.0"));
}

/// The `nps` of the deepest line: what the fake engine was set to.
fn settings_of(lines: &[String]) -> (u64, u64, u64) {
    let line = lines.iter().rev().find(|l| l.contains(r#""nps":"#)).expect("an info line");
    let nps: u64 = line.split(r#""nps":"#).nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
    (nps / 1_000_000_000, nps / 1_000 % 1_000_000, nps % 1_000)
}

#[test]
fn threads_and_hash_come_with_the_analysis_and_are_set_only_when_they_change() {
    let (port, _app) = start(Engine::new(fake()));
    let search = |extra: &str| {
        let mut a = Analysis::open(port, &format!("depth=2&stream=tab1{extra}"));
        assert_eq!(a.status, 200);
        settings_of(&a.rest())
    };
    // The handshake sets the configured 1 thread and 16 MB.
    assert_eq!(search(""), (1, 16, 2));
    assert_eq!(search("&threads=1&hash=64"), (1, 64, 3));
    assert_eq!(search("&threads=1&hash=64"), (1, 64, 3), "the same values set nothing");
    // Left out, a value goes back to the configured default.
    assert_eq!(search(""), (1, 16, 4));
}

#[test]
fn configured_values_above_the_limits_start_and_stay_within_them() {
    let limits = bridge::engine::limits();
    let over = EngineConfig::new(env!("CARGO_BIN_EXE_fake-uci").into(), Some(u32::MAX), Some(u32::MAX));
    let (port, _app) = start(Engine::new(over));
    let status = get(port, "/v1/status", true);
    let (threads, hash) = (u64::from(limits.max_threads), u64::from(limits.max_hash_mb));
    assert!(status.contains(&format!(r#""threads":{{"default":{threads},"max":{threads}}}"#)), "{status}");
    assert!(status.contains(&format!(r#""hash":{{"default":{hash},"max":{hash}}}"#)), "{status}");
    let search = |extra: &str| {
        let mut a = Analysis::open(port, &format!("depth=2&stream=tab1{extra}"));
        assert_eq!(a.status, 200);
        settings_of(&a.rest())
    };
    // The handshake already sets the limits, not the configured values.
    assert_eq!(search(""), (threads, hash, 2));
    // A computer whose limit is 1 thread or 16 MB has nothing to change there.
    let changed = u64::from(threads != 1) + u64::from(hash != 16);
    assert_eq!(search("&threads=1&hash=16"), (1, 16, 2 + changed));
    assert_eq!(search(""), (threads, hash, 2 + 2 * changed), "back to the defaults within the limits");
}

#[test]
fn stops_the_engine_when_the_client_leaves() {
    let (port, app) = start(Engine::new(fake()));
    let mut a = Analysis::open(port, "stream=tab1");
    assert!(a.line().unwrap().starts_with(r#"{"info":"#));
    drop(a);
    // The engine was stopped, not lost: the next analysis finds it free.
    let mut b = Analysis::open(port, "depth=2&stream=tab1");
    assert_eq!(b.rest().last().unwrap(), BEST);
    assert!(app.engine.is_running());
}

#[test]
fn a_newer_analysis_takes_the_engine() {
    let (port, _app) = start(Engine::new(fake()));
    // From another view: the first hears why it ended.
    let mut first = Analysis::open(port, "stream=tab1");
    assert!(first.line().is_some());
    let mut second = Analysis::open(port, "depth=3&stream=tab2");
    let ended = first.rest();
    assert_eq!(ended.last().unwrap(), r#"{"superseded":true}"#, "{ended:?}");
    assert_eq!(second.rest().last().unwrap(), BEST);
    // From the same view: it just ends.
    let mut first = Analysis::open(port, "stream=tab1");
    assert!(first.line().is_some());
    let mut second = Analysis::open(port, "moves=d2d4&depth=3&stream=tab1");
    let ended = first.rest();
    assert!(ended.iter().all(|l| l.starts_with(r#"{"info":"#)), "{ended:?}");
    assert_eq!(second.rest().last().unwrap(), BEST);
}

#[test]
fn a_crashed_engine_ends_the_stream_and_starts_again() {
    let (port, _app) = start(Engine::new(fake()));
    let mut a = Analysis::open(port, "moves=h2h3&stream=tab1");
    let lines = a.rest();
    assert!(lines.last().unwrap().starts_with(r#"{"error":{"code":"engine_exited""#), "{lines:?}");
    let mut b = Analysis::open(port, "depth=2&stream=tab1");
    assert_eq!(b.rest().last().unwrap(), BEST);
}

#[test]
fn an_engine_that_ignores_stop_is_replaced() {
    let (port, _app) = start(Engine::new(fake()));
    let mut stubborn = Analysis::open(port, "moves=a2a3&stream=tab1");
    assert!(stubborn.line().is_some());
    let started = Instant::now();
    let mut next = Analysis::open(port, "depth=2&stream=tab2");
    assert_eq!(next.rest().last().unwrap(), BEST);
    assert!(started.elapsed() >= Duration::from_secs(2), "the stop grace was not waited for");
    assert_eq!(stubborn.rest().last().unwrap(), r#"{"superseded":true}"#);
}

#[test]
fn a_quiet_search_repeats_its_last_lines() {
    let (port, _app) = start(Engine::new(fake()));
    let mut a = Analysis::open(port, "moves=b2b3&stream=tab1");
    let first = a.line().unwrap();
    let started = Instant::now();
    assert_eq!(a.line().unwrap(), first);
    assert!(started.elapsed() >= Duration::from_millis(1500), "{:?}", started.elapsed());
}

#[test]
fn a_handshake_past_its_deadline_fails_however_much_the_engine_writes() {
    let dir = std::env::temp_dir().join(format!("bridge-chatty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let chatty = dir.join(format!("fake-uci-chatty{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_fake-uci"), &chatty).unwrap();
    let (port, _app) = start(Engine::new(EngineConfig::new(chatty, Some(1), Some(16))));
    let started = Instant::now();
    let lines = Analysis::open(port, "depth=1").rest();
    assert!(lines.last().unwrap().starts_with(r#"{"error":{"code":"engine_failed""#), "{lines:?}");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(7), "the handshake took {took:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_idle_engine_ends_its_process() {
    let (port, app) = start(Engine::with_idle(fake(), Duration::from_millis(300)));
    let mut a = Analysis::open(port, "depth=1");
    assert_eq!(a.rest().last().unwrap(), BEST);
    assert!(app.engine.is_running());
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.engine.is_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!app.engine.is_running(), "the idle engine still runs");
}

#[test]
fn refuses_bad_input_and_a_missing_engine() {
    let (port, _app) = start(Engine::new(fake()));
    for (query, parameter) in [
        ("moves=e2e5", "moves"),
        ("moves=e2e4%0Aquit", "moves"),
        ("fen=8%2F8%2F8%2F8%2F8%2F8%2F8%2F8+w+-+-+0+1", "fen"),
        ("multipv=6", "multipv"),
        ("multipv=x", "multipv"),
        ("depth=0", "depth"),
        ("depth=5&movetime=100", "movetime"),
        ("stream=a%20b", "stream"),
        ("threads=0", "threads"),
        ("threads=x", "threads"),
        ("hash=15", "hash"),
        ("hash=99999999", "hash"),
    ] {
        let a = Analysis::open(port, query);
        assert_eq!(a.status, 400, "{query}");
        let body = a.body();
        assert!(body.contains(&format!(r#""parameter":"{parameter}""#)), "{query}: {body}");
    }
    let (port, _app) = start(Engine::none());
    let a = Analysis::open(port, "depth=1");
    assert_eq!(a.status, 409);
    assert!(a.body().contains(r#""code":"no_engine""#));
}

fn get(port: u16, path: &str, token: bool) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = if token { format!("Authorization: Bearer {TOKEN}\r\n") } else { String::new() };
    let raw =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Origin: {ORIGIN}\r\nConnection: close\r\n\r\n");
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn the_status_names_the_engine() {
    let (port, _app) = start(Engine::new(fake()));
    let status = get(port, "/v1/status", true);
    let limits = bridge::engine::limits();
    let engine = format!(
        r#""engine":{{"name":"fake-uci","threads":{{"default":1,"max":{}}},"hash":{{"default":16,"max":{}}}}}"#,
        limits.max_threads, limits.max_hash_mb
    );
    assert!(status.contains(&engine), "{status}");
    let (port, _app) = start(Engine::none());
    assert!(get(port, "/v1/status", true).contains(r#""engine":null"#));
}

#[test]
fn the_token_is_required() {
    let (port, _app) = start(Engine::new(fake()));
    let out = get(port, "/v1/engine/analyze?depth=1", false);
    assert!(out.starts_with("HTTP/1.1 401"), "{out}");
}

/// With a real engine, which the tests do not carry:
/// `BRIDGE_REAL_ENGINE=/path/to/stockfish cargo test -p bridge --test engine -- --ignored`.
#[test]
#[ignore = "needs BRIDGE_REAL_ENGINE"]
fn a_real_engine_analyses() {
    let path = std::env::var("BRIDGE_REAL_ENGINE").expect("BRIDGE_REAL_ENGINE names an engine");
    let (port, app) = start(Engine::new(EngineConfig::new(path.into(), Some(2), Some(64))));
    let mut a = Analysis::open(port, "moves=e2e4+e7e5+g1f3&multipv=3&depth=14&stream=real");
    let lines = a.rest();
    assert!(lines.last().unwrap().starts_with(r#"{"bestmove":""#), "{lines:?}");
    for k in 1..=3 {
        let number = format!(r#""multipv":{k},"#);
        assert!(lines.iter().any(|l| l.contains(r#""depth":14,"#) && l.contains(&number)), "{k}: {lines:?}");
    }
    assert!(app.engine.name().unwrap().starts_with("Stockfish"), "{:?}", app.engine.name());
    // A search without a limit stops when its client leaves; the engine serves the next one.
    let mut b = Analysis::open(
        port,
        "fen=r1bqkbnr%2Fpppp1ppp%2F2n5%2F4p3%2F4P3%2F5N2%2FPPPP1PPP%2FRNBQKB1R+w+KQkq+-+2+3&stream=real",
    );
    assert!(b.line().unwrap().starts_with(r#"{"info":"#));
    drop(b);
    let mut c = Analysis::open(port, "depth=8&stream=real");
    assert!(c.rest().last().unwrap().starts_with(r#"{"bestmove":""#));
    // A decided position keeps its score, with no moves to give.
    for (fen, score) in [
        ("7k%2F6Q1%2F5K2%2F8%2F8%2F8%2F8%2F8+b+-+-+0+1", r#""score":{"mate":0}"#),
        ("7k%2F5Q2%2F6K1%2F8%2F8%2F8%2F8%2F8+b+-+-+0+1", r#""score":{"cp":0}"#),
    ] {
        let lines = Analysis::open(port, &format!("fen={fen}&depth=1&stream=real")).rest();
        assert!(lines.iter().any(|l| l.contains(score) && l.contains(r#""pv":[]"#)), "{fen}: {lines:?}");
        assert_eq!(lines.last().unwrap(), r#"{"bestmove":"(none)"}"#);
    }
}

/// A copy of the fake engine under another file name, so that two engines differ.
fn fake_copy(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_fake-uci"), &path).unwrap();
    path
}

fn engine_line(path: &std::path::Path) -> String {
    format!("port = 39581\nengine = '{}'\nengine_threads = 1\nengine_hash = 16\n", path.display())
}

#[test]
fn the_engine_follows_bridge_toml() {
    let dir = std::env::temp_dir().join(format!("bridge-follow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let toml = dir.join("bridge.toml");
    let write = |text: &str| {
        // Replaced whole, as the settings window saves it: the engine's watcher
        // must never read a half-written file, which an empty one would be.
        let part = dir.join("bridge.toml.part");
        std::fs::write(&part, text).unwrap();
        std::fs::rename(&part, &toml).unwrap();
        // The signature includes the modification time; let it move on coarse clocks.
        std::thread::sleep(Duration::from_millis(20));
    };
    write("port = 39581\n");
    let (port, app) = start(Engine::following(toml.clone(), Duration::from_millis(50)));
    assert!(!app.engine.is_configured());
    let first = fake_copy(&dir, "engine-a");
    write(&engine_line(&first));
    assert_eq!(app.engine.name().as_deref(), Some("engine-a"));
    let mut a = Analysis::open(port, "depth=2");
    assert_eq!(a.rest().last().unwrap(), BEST);

    // Choosing another engine stops the running search by itself: nothing
    // calls the engine between the change and the search's end.
    let mut running = Analysis::open(port, "stream=tab1");
    assert!(running.line().is_some());
    let second = fake_copy(&dir, "engine-b");
    write(&engine_line(&second));
    let ended = running.rest_within(Duration::from_secs(3)).expect("the old search ran on");
    assert!(ended.iter().all(|l| l.starts_with(r#"{"info":"#) || l == r#"{"superseded":true}"#), "{ended:?}");
    assert_eq!(app.engine.name().as_deref(), Some("engine-b"));

    // No engine at all, then a file that no longer parses keeps the engine it named.
    write("port = 39581\n");
    assert!(!app.engine.is_configured());
    write(&engine_line(&first));
    assert!(app.engine.is_configured());
    write("engine = \n");
    assert_eq!(app.engine.name().as_deref(), Some("engine-a"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The database list and the engine read `bridge.toml` alike (#70): a file
/// that cannot be parsed keeps the databases and the engine read before, and
/// the next good file sets both.
#[test]
fn a_broken_file_keeps_the_databases_and_the_engine() {
    let dir = std::env::temp_dir().join(format!("bridge-broken-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let toml = dir.join("bridge.toml");
    let (first, second) = (fake_copy(&dir, "engine-c"), fake_copy(&dir, "engine-d"));
    let file = |engine: &std::path::Path, database: &str| {
        format!("{}databases = ['{}']\n", engine_line(engine), dir.join(database).display())
    };
    // Each text replaces the file whole: the engine's first read runs in the
    // background, and must not find a file cut short while it is written.
    let replace = |text: &str| {
        let part = dir.join("bridge.toml.part");
        std::fs::write(&part, text).unwrap();
        std::fs::rename(&part, &toml).unwrap();
    };
    replace(&file(&first, "Old.2cbh"));
    let catalog = Catalog::with_sources(
        Sources { config: Some(toml.clone()), ..Sources::default() },
        Arc::new(bridge::fetch::System),
    );
    let engine = Engine::following(toml.clone(), Duration::from_secs(3600));
    let names = || catalog.entries().iter().filter(|e| e.listed()).map(|e| e.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(), ["Old"]);
    assert_eq!(engine.name().as_deref(), Some("engine-c"));

    // Of another length than before, so its signature changes whatever the clock.
    replace(&format!("{}databases = [unquoted, and more]\n", engine_line(&second)));
    assert_eq!(names(), ["Old"], "a broken file keeps the databases");
    assert_eq!(engine.name().as_deref(), Some("engine-c"), "and the engine");

    replace(&file(&second, "New database.2cbh"));
    assert_eq!(names(), ["New database"]);
    assert_eq!(engine.name().as_deref(), Some("engine-d"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A read that fails is tried again even when the file did not change, and a
/// pipe in the file's place is never read, so nothing waits on it.
#[cfg(unix)]
#[test]
fn a_failed_read_is_tried_again_and_a_pipe_is_never_read() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("bridge-reread-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let toml = dir.join("bridge.toml");
    let first = fake_copy(&dir, "engine-a");
    let second = fake_copy(&dir, "engine-b");
    std::fs::write(&toml, engine_line(&first)).unwrap();
    let engine = Engine::following(toml.clone(), Duration::from_secs(3600));
    assert_eq!(engine.name().as_deref(), Some("engine-a"));
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&toml, engine_line(&second)).unwrap();
    std::fs::set_permissions(&toml, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Skipped when the tests run as root, who reads the file anyway.
    if std::fs::read(&toml).is_err() {
        assert_eq!(engine.name().as_deref(), Some("engine-a"));
    }
    std::fs::set_permissions(&toml, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(engine.name().as_deref(), Some("engine-b"));

    std::fs::remove_file(&toml).unwrap();
    assert!(std::process::Command::new("mkfifo").arg(&toml).status().unwrap().success());
    let asked = Instant::now();
    assert_eq!(engine.name().as_deref(), Some("engine-b"));
    assert!(asked.elapsed() < Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_probe_accepts_only_a_uci_engine() {
    assert_eq!(engine::probe(env!("CARGO_BIN_EXE_fake-uci").as_ref()).as_deref(), Ok("Fake UCI 1.0"));
    let dir = std::env::temp_dir().join(format!("bridge-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let text = dir.join("notes.txt");
    std::fs::write(&text, "not an engine").unwrap();
    assert!(engine::probe(&text).is_err());
    assert!(engine::probe(&dir.join("missing.exe")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
