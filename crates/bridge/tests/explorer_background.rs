//! The position indexes of the databases in use, kept up to date in the
//! background (#149): the keeper that queues their builds, the queues that
//! run requested builds before background ones, and what `/v1/status` and the
//! explorer say meanwhile. Every bridge here has its keeper look every 50 ms,
//! with the quiet period each test sets.

use std::collections::HashSet;
use std::fs::Metadata;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::explorer::format::{HEADER_LEN, Header};
use bridge::explorer::runs::Limits;
use bridge::fetch::Cloud;
use bridge::machine::Machine;
use bridge::sources::Sources;
use cbformat::fixture::{Builder, TempDb, pgn_file};
use cbformat::movetable::{END_OF_LINE, MOVES};
use chesscore::Board;

mod common;
use common::{get, policy, serve_shared};

const TICK: Duration = Duration::from_millis(50);
const START: &str = "rnbqkbnr%2Fpppppppp%2F8%2F8%2F8%2F8%2FPPPPPPPP%2FRNBQKBNR%20w%20KQkq%20-%200%201";

/// `games` games, each one of eight lines of 160 plies: a database written
/// in a moment whose index takes a while to build.
fn copies(name: &str, games: usize) -> TempDb {
    let mut b = Builder::new();
    let mut lines = Vec::new();
    let mut x = 7u64;
    for _ in 0..8 {
        let mut board = Board::startpos();
        let mut words = vec![MOVES];
        for _ in 0..160 {
            let moves = board.legal_moves();
            if moves.is_empty() {
                break;
            }
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let mv = moves[(x % moves.len() as u64) as usize];
            words.push(cbformat::replay::word_of(&board, mv).unwrap());
            board.play_checked(mv).unwrap();
        }
        words.push(END_OF_LINE);
        lines.push(b.moves(1, &words));
    }
    for g in 0..games {
        b.game(lines[g % lines.len()]);
    }
    b.write(name)
}

/// A data folder of its own, empty.
fn data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-background-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The generation the index of `id` in the data folder `dir` was built for,
/// by its header; `None` without one.
fn built_for(dir: &Path, id: &str) -> Option<u64> {
    let mut head = [0u8; HEADER_LEN];
    std::fs::File::open(dir.join("index").join(format!("{id}.idx"))).ok()?.read_exact(&mut head).ok()?;
    Header::decode(&head).map(|h| h.generation)
}

/// The generation of the database at `path` now.
fn generation(path: &Path) -> u64 {
    Catalog::new([path.to_path_buf()]).entries()[0].generation().unwrap()
}

/// Waits up to `secs` seconds for `done`.
fn wait(what: &str, secs: u64, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The phase `/v1/status` gives the build of `id`, if it lists one.
fn phase(port: u16, id: &str) -> Option<String> {
    let (_, body) = get(port, "/v1/status");
    let before = format!(r#"{{"id":"{id}","phase":""#);
    let at = body.find(&before)? + before.len();
    Some(body[at..at + body[at..].find('"')?].to_string())
}

/// A computer whose power and free disk space a test sets.
#[derive(Default)]
struct Computer {
    battery: AtomicBool,
    free: Mutex<Option<u64>>,
}

impl Machine for Computer {
    fn on_battery(&self) -> bool {
        self.battery.load(Ordering::SeqCst)
    }

    fn free_bytes(&self, _: &Path) -> Option<u64> {
        *self.free.lock().unwrap()
    }
}

/// A cloud provider keeping `files` in the cloud, which counts downloads.
#[derive(Default)]
struct Provider {
    files: HashSet<PathBuf>,
    fetches: AtomicUsize,
}

impl Cloud for Provider {
    fn is_cloud_only(&self, path: &Path, _: &Metadata) -> bool {
        self.files.contains(path)
    }

    fn fetch(&self, _: &Path, _: &mut dyn FnMut(u64)) -> std::io::Result<()> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("not for a test"))
    }
}

/// A bridge serving `catalog`, with its data folder `dir`; its keeper starts
/// with [`Bridge::keep`]. Dropped, it stops its keeper, waits for its builds
/// and gives up the indexes it holds.
struct Bridge {
    port: u16,
    app: Arc<App>,
}

impl Bridge {
    fn new(catalog: Catalog, dir: &Path) -> Bridge {
        catalog.use_data_dir(dir);
        let (port, app) = serve_shared(App::new("test", policy(), catalog));
        Bridge { port, app }
    }

    /// Starts the keeper, looking every [`TICK`], with `quiet` as the quiet
    /// period.
    fn keep(&self, quiet: Duration) {
        self.app.catalog.explorer.set_keeping(TICK, quiet);
        bridge::explorer::keeper::start(&self.app);
    }

    fn explorer(&self, id: &str) -> (u16, String) {
        get(self.port, &format!("/v1/databases/{id}/explorer?fen={START}"))
    }

    fn building(&self) -> Vec<(String, &'static str, u64, u64)> {
        self.app.catalog.explorer.building()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let explorer = &self.app.catalog.explorer;
        explorer.stop_keeping();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !explorer.building().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        explorer.release();
    }
}

/// A database in use that changes is rebuilt once it is quiet, without a
/// request; the explorer then answers at once from the new index.
#[test]
fn a_changed_database_in_use_is_rebuilt_without_a_request() {
    let db = copies("background-changed", 20);
    let path = db.dir().join("db.2cbh");
    let (id, dir) = (id_of(&path), data_dir("changed"));
    let bridge = Bridge::new(Catalog::new([path.clone()]), &dir);
    bridge.keep(Duration::from_millis(200));
    // The only ready database is in use from the start.
    let first = generation(&path);
    wait("the index was not built", 60, || built_for(&dir, &id) == Some(first));
    let (status, body) = bridge.explorer(&id);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""games":20,"#), "{body}");
    // More games.
    std::thread::sleep(Duration::from_millis(20));
    let _grown = copies("background-changed", 30);
    let second = generation(&path);
    assert_ne!(first, second);
    wait("the index was not rebuilt", 60, || built_for(&dir, &id) == Some(second));
    wait("the build did not end", 60, || bridge.building().is_empty());
    let (status, body) = bridge.explorer(&id);
    assert_eq!(status, 200, "the first request after the change answers at once: {body}");
    assert!(body.contains(r#""games":30,"#), "{body}");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A database that changes more often than the quiet period is not built
/// until it stops changing; then it is, at its last generation.
#[test]
fn a_database_that_keeps_changing_waits_until_it_is_quiet() {
    let db = copies("background-changing", 10);
    let path = db.dir().join("db.2cbh");
    let (id, dir) = (id_of(&path), data_dir("changing"));
    let bridge = Bridge::new(Catalog::new([path.clone()]), &dir);
    bridge.keep(Duration::from_secs(2));
    // Written again every 50 ms for 3 s, with 11 or 12 games: many ticks.
    let mut written = Vec::new();
    let until = Instant::now() + Duration::from_secs(3);
    let mut games = 11;
    while Instant::now() < until {
        written.push(copies("background-changing", games));
        games = 23 - games;
        assert!(built_for(&dir, &id).is_none(), "built while it changed");
        assert!(bridge.building().is_empty(), "queued while it changed: {:?}", bridge.building());
        std::thread::sleep(Duration::from_millis(50));
    }
    let last = generation(&path);
    wait("the index was not built once quiet", 60, || built_for(&dir, &id) == Some(last));
    drop(bridge);
    drop(written);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A requested build stops the background build of another database at its
/// next batch and goes first; the stopped build waits and then completes,
/// without a request.
#[test]
fn a_requested_build_goes_before_a_background_one() {
    let (large, small) = (copies("background-first-large", 20_000), copies("background-first-small", 20));
    let paths = [large.dir().join("db.2cbh"), small.dir().join("db.2cbh")];
    let (a, b) = (id_of(&paths[0]), id_of(&paths[1]));
    let dir = data_dir("first");
    let catalog = Catalog::new(paths.clone());
    // Builds of many passes, so that the large one takes a while.
    catalog.explorer.set_limits(Limits { pass_bytes: Some(256 << 10), ..Limits::default() });
    let bridge = Bridge::new(catalog, &dir);
    bridge.keep(Duration::ZERO);
    // The largest database is in use from the start: its build runs.
    wait("the background build did not start", 60, || {
        phase(bridge.port, &a).is_some_and(|p| !["waiting", "checking"].contains(&p.as_str()))
    });
    let (status, body) = bridge.explorer(&b);
    assert_eq!(status, 409, "{body}");
    wait("the requested build did not answer", 60, || bridge.explorer(&b).0 == 200);
    assert!(built_for(&dir, &a).is_none(), "the background build ended first");
    assert!(phase(bridge.port, &a).is_some(), "the stopped build waits again");
    // It completes afterwards, asked for by nobody.
    wait("the stopped build did not complete", 120, || built_for(&dir, &a) == Some(generation(&paths[0])));
    wait("the build did not end", 60, || bridge.building().is_empty());
    let (status, body) = bridge.explorer(&a);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""games":20000,"#), "{body}");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// No database is read in the background while it is kept in the cloud,
/// once it has left the list, or while a PGN file's header index is built,
/// even in use; a ready one beside them is. The PGN file is built once
/// ready.
#[test]
fn nothing_is_built_in_the_background_for_a_cloud_only_unlisted_or_opening_database() {
    let root = data_dir("never");
    std::fs::create_dir_all(&root).unwrap();
    let (ready, cloud, unlisted) = (
        copies("background-never-ready", 10),
        copies("background-never-cloud", 40),
        copies("background-never-gone", 30),
    );
    let pgn = pgn_file("background-never-opening", b"[Event \"x\"]\n\n1. e4 e5 2. Nf3 *\n");
    let paths = [
        ready.dir().join("db.2cbh"),
        cloud.dir().join("db.2cbh"),
        unlisted.dir().join("db.2cbh"),
        pgn.dir().join("db.pgn"),
    ];
    let ids: Vec<String> = paths.iter().map(|p| id_of(p)).collect();
    let config = root.join("bridge.toml");
    let list = |paths: &[&PathBuf]| {
        let quoted: Vec<String> = paths.iter().map(|p| format!("'{}'", p.display())).collect();
        std::fs::write(&config, format!("databases = [{}]\n", quoted.join(", "))).unwrap();
    };
    list(&paths.iter().collect::<Vec<_>>());
    let files = std::fs::read_dir(cloud.dir()).unwrap().map(|e| e.unwrap().path()).collect();
    let provider = Arc::new(Provider { files, ..Provider::default() });
    let catalog =
        Catalog::with_sources(Sources { config: Some(config.clone()), ..Sources::default() }, provider.clone());
    assert_eq!(catalog.entries().len(), 4);
    // The PGN file's header index waits behind another build.
    let (release, held) = mpsc::channel::<()>();
    assert!(catalog.pgn().queue().submit(Box::new(move || {
        let _ = held.recv();
    })));
    for id in &ids {
        catalog.explorer.mark_in_use(id);
    }
    // The third leaves the list.
    std::thread::sleep(Duration::from_millis(20));
    list(&[&paths[0], &paths[1], &paths[3]]);
    let entries = catalog.entries();
    let listed: Vec<bool> = ids.iter().map(|id| entries.iter().any(|e| &e.id == id && e.listed())).collect();
    assert_eq!(listed, [true, true, false, true]);
    let bridge = Bridge::new(catalog, &root);
    bridge.keep(Duration::ZERO);
    wait("the ready database was not built", 60, || built_for(&root, &ids[0]).is_some());
    std::thread::sleep(TICK * 10);
    for (id, what) in ids[1..].iter().zip(["cloud-only", "unlisted", "opening"]) {
        assert!(built_for(&root, id).is_none(), "{what} was built");
    }
    assert!(bridge.building().is_empty(), "{:?}", bridge.building());
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 0, "a database was downloaded");
    // Once opened, the PGN file is built.
    release.send(()).unwrap();
    wait("the PGN file was not built once ready", 60, || built_for(&root, &ids[3]).is_some());
    drop(bridge);
    std::fs::remove_dir_all(&root).unwrap();
}

/// At the start, the ready database with the most records is in use: its
/// index is built unasked, and no other's.
#[test]
fn the_startup_pick_is_the_largest_ready_database() {
    let (small, large, cloud) = (
        copies("background-pick-small", 10),
        copies("background-pick-large", 40),
        copies("background-pick-cloud", 100),
    );
    let paths = [small.dir().join("db.2cbh"), large.dir().join("db.2cbh"), cloud.dir().join("db.2cbh")];
    let ids: Vec<String> = paths.iter().map(|p| id_of(p)).collect();
    let files = std::fs::read_dir(cloud.dir()).unwrap().map(|e| e.unwrap().path()).collect();
    let provider = Arc::new(Provider { files, ..Provider::default() });
    let dir = data_dir("pick");
    let catalog = Catalog::with_sources(Sources { fixed: paths.to_vec(), ..Sources::default() }, provider.clone());
    let bridge = Bridge::new(catalog, &dir);
    bridge.keep(Duration::ZERO);
    wait("the largest ready database was not built", 60, || built_for(&dir, &ids[1]).is_some());
    std::thread::sleep(TICK * 10);
    assert!(built_for(&dir, &ids[0]).is_none(), "a smaller database was built");
    assert!(built_for(&dir, &ids[2]).is_none(), "a cloud-only database was built");
    assert!(bridge.building().is_empty(), "{:?}", bridge.building());
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 0);
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// With less free space than a build needs on the index folder's disk, a
/// requested build is answered `503 index_unavailable` saying so, and a
/// background build is not queued, and not tried again, until the database
/// changes.
#[test]
fn too_little_free_space_answers_503_and_skips_background_builds() {
    let (large, small) = (copies("background-disk-large", 40), copies("background-disk-small", 10));
    let paths = [large.dir().join("db.2cbh"), small.dir().join("db.2cbh")];
    let (a, b) = (id_of(&paths[0]), id_of(&paths[1]));
    let dir = data_dir("disk");
    let computer = Arc::new(Computer::default());
    *computer.free.lock().unwrap() = Some(100 << 20);
    let catalog = Catalog::new(paths.clone());
    catalog.explorer.set_machine(computer.clone());
    let bridge = Bridge::new(catalog, &dir);
    bridge.keep(Duration::ZERO);
    std::thread::sleep(TICK * 10);
    assert!(bridge.building().is_empty(), "{:?}", bridge.building());
    assert!(built_for(&dir, &a).is_none());
    for id in [&b, &a] {
        let (status, body) = bridge.explorer(id);
        assert_eq!(status, 503, "{body}");
        assert!(body.contains(r#""code":"index_unavailable""#), "{body}");
        assert!(body.contains("the disk of the index folder has 105 MB free"), "{body}");
        assert!(body.contains("needs 269 MB"), "{body}");
    }
    // With room again, the background build waits for the next change.
    *computer.free.lock().unwrap() = Some(1 << 40);
    std::thread::sleep(TICK * 10);
    assert!(bridge.building().is_empty() && built_for(&dir, &a).is_none(), "{:?}", bridge.building());
    std::thread::sleep(Duration::from_millis(20));
    let _changed = copies("background-disk-large", 41);
    wait("the changed database was not built", 60, || built_for(&dir, &a) == Some(generation(&paths[0])));
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A build queued behind another, requested or in the background, is listed
/// with the phase `waiting`, and a request for its database is answered so;
/// the requested one runs before the background one.
#[test]
fn a_queued_build_shows_waiting() {
    let (large, small, other) = (
        copies("background-wait-large", 20_000),
        copies("background-wait-small", 10),
        copies("background-wait-other", 12),
    );
    let paths = [large.dir().join("db.2cbh"), small.dir().join("db.2cbh"), other.dir().join("db.2cbh")];
    let ids: Vec<String> = paths.iter().map(|p| id_of(p)).collect();
    let dir = data_dir("wait");
    let catalog = Catalog::new(paths.clone());
    catalog.explorer.set_limits(Limits { pass_bytes: Some(256 << 10), ..Limits::default() });
    catalog.explorer.mark_in_use(&ids[2]);
    let bridge = Bridge::new(catalog, &dir);
    assert_eq!(bridge.explorer(&ids[0]).0, 409);
    wait("the build did not start", 60, || phase(bridge.port, &ids[0]).is_some_and(|p| p != "waiting"));
    let (status, body) = bridge.explorer(&ids[1]);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""progress":{"phase":"waiting","done":0,"total":0}"#), "{body}");
    assert_eq!(phase(bridge.port, &ids[1]).as_deref(), Some("waiting"));
    // The keeper queues the third, in use, behind them.
    bridge.keep(Duration::ZERO);
    wait("the background build was not queued", 60, || phase(bridge.port, &ids[2]).is_some());
    assert_eq!(phase(bridge.port, &ids[2]).as_deref(), Some("waiting"));
    wait("the builds did not end", 120, || ids.iter().all(|id| built_for(&dir, id).is_some()));
    let written =
        |id: &str| std::fs::metadata(dir.join("index").join(format!("{id}.idx"))).unwrap().modified().unwrap();
    assert!(written(&ids[0]) <= written(&ids[1]) && written(&ids[1]) <= written(&ids[2]), "built out of turn");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A background build waits while the computer runs on battery, and a
/// requested one does not; back on mains power, it is built.
#[test]
fn a_background_build_waits_on_battery() {
    let (large, small) = (copies("background-battery-large", 40), copies("background-battery-small", 10));
    let paths = [large.dir().join("db.2cbh"), small.dir().join("db.2cbh")];
    let (a, b) = (id_of(&paths[0]), id_of(&paths[1]));
    let dir = data_dir("battery");
    let computer = Arc::new(Computer::default());
    computer.battery.store(true, Ordering::SeqCst);
    let catalog = Catalog::new(paths.clone());
    catalog.explorer.set_machine(computer.clone());
    let bridge = Bridge::new(catalog, &dir);
    bridge.keep(Duration::ZERO);
    wait("the background build was not queued", 60, || phase(bridge.port, &a).is_some());
    std::thread::sleep(TICK * 10);
    assert_eq!(phase(bridge.port, &a).as_deref(), Some("waiting"));
    assert!(built_for(&dir, &a).is_none());
    wait("the requested build did not answer", 60, || bridge.explorer(&b).0 == 200);
    assert!(built_for(&dir, &a).is_none(), "built on battery");
    computer.battery.store(false, Ordering::SeqCst);
    wait("the background build did not run on mains power", 60, || built_for(&dir, &a).is_some());
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The files of a database written a while ago, and so quiet from the start:
/// the keeper builds it at its first look, as a bridge that starts builds
/// the Mega Database, which the quiet period would otherwise hold back.
#[test]
fn a_database_unchanged_for_the_quiet_period_is_built_at_the_first_look() {
    let db = copies("background-old", 10);
    let path = db.dir().join("db.2cbh");
    let a_while_ago = SystemTime::now() - Duration::from_secs(600);
    for entry in std::fs::read_dir(db.dir()).unwrap() {
        let file = std::fs::File::options().write(true).open(entry.unwrap().path()).unwrap();
        file.set_modified(a_while_ago).unwrap();
    }
    let (id, dir) = (id_of(&path), data_dir("old"));
    let bridge = Bridge::new(Catalog::new([path.clone()]), &dir);
    // A tick and a quiet period of an hour: only the first look can build it.
    bridge.app.catalog.explorer.set_keeping(Duration::from_secs(3600), Duration::from_secs(60));
    bridge::explorer::keeper::start(&bridge.app);
    wait("the index was not built at the first look", 60, || built_for(&dir, &id) == Some(generation(&path)));
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}
