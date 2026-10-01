//! The position indexes of the databases in use, kept up to date in the
//! background (#149): the keeper that queues their builds, the queues that
//! run requested builds before background ones, background builds that give
//! way to searches, and what `/v1/status` and the explorer say meanwhile.
//! Every bridge here has its keeper look every 50 ms, with the quiet period
//! each test sets. A test that takes nothing having happened as never
//! happening waits for the keeper's looks, which its [`Computer`] counts.
//!
//! A search a test holds is foreground work for the whole process
//! (`bridge::foreground`): the background builds of the other tests would
//! give way to it too, for their patience at every batch. The tests that
//! hold one each run in a process of their own ([`in_child`]).

use std::collections::HashSet;
use std::fs::Metadata;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::config::Watched;
use bridge::explorer::format::{HEADER_LEN, Header};
use bridge::explorer::runs::{Limits, Progress};
use bridge::fetch::Cloud;
use bridge::machine::Machine;
use bridge::search::Indexes;
use bridge::sources::Sources;
use cbformat::fixture::{Builder, DbItems, TempDb, pgn_file};
use cbformat::movetable::{END_OF_LINE, MOVES};
use cbformat::v2::Database;
use chesscore::Board;

mod common;
use common::{Sent, TestBridge, WAIT_LIMIT, get, in_child, index_dir, policy, until};

const TICK: Duration = Duration::from_millis(50);
/// What tells a child process that runs a test's body that it is one.
const CHILD: &str = "BRIDGE_BACKGROUND_CHILD";
/// The patience of a background build where a test shows that it goes on at
/// once when a search it gives way to ends, or that a request makes it give
/// way no longer: twice as long as the test waits for that ([`WAIT_LIMIT`]).
const LONG_PATIENCE: Duration = Duration::from_secs(600);
/// The numbers ChessBase writes beside a database in its window list.
const WINDOW_NUMBERS: [i64; 6] = [0, 28, 1, 1, 1037620, 1037559];
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

/// Sets the modification time of every file in `dir` ten minutes back.
fn backdate(dir: &Path) {
    let a_while_ago = SystemTime::now() - Duration::from_secs(600);
    for entry in std::fs::read_dir(dir).unwrap() {
        let file = std::fs::File::options().write(true).open(entry.unwrap().path()).unwrap();
        file.set_modified(a_while_ago).unwrap();
    }
}

/// The generation of the database at `path` now.
fn generation(path: &Path) -> u64 {
    Catalog::new([path.to_path_buf()]).entries()[0].generation().unwrap()
}

/// The phase `/v1/status` gives the build of `id`, if it lists one.
fn phase(port: u16, id: &str) -> Option<String> {
    let (_, body) = get(port, "/v1/status");
    let before = format!(r#"{{"id":"{id}","phase":""#);
    let at = body.find(&before)? + before.len();
    Some(body[at..at + body[at..].find('"')?].to_string())
}

/// What a test does as the keeper begins a look.
type OnLook = Box<dyn Fn() + Send + Sync>;

/// A computer whose power and free disk space a test sets, which counts the
/// keeper's looks, and does what the test sets as each begins: the keeper
/// asks about the power as it begins a look, on its thread, `bridge-keeper`.
#[derive(Default)]
struct Computer {
    battery: AtomicBool,
    free: Mutex<Option<u64>>,
    looks: AtomicUsize,
    on_look: Mutex<Option<OnLook>>,
}

impl Machine for Computer {
    fn on_battery(&self) -> bool {
        if std::thread::current().name() == Some("bridge-keeper") {
            self.looks.fetch_add(1, Ordering::SeqCst);
            if let Some(look) = &*self.on_look.lock().unwrap() {
                look();
            }
        }
        self.battery.load(Ordering::SeqCst)
    }

    fn free_bytes(&self, _: &Path) -> Option<u64> {
        *self.free.lock().unwrap()
    }
}

impl Computer {
    /// The looks the keeper has begun.
    fn looks(&self) -> usize {
        self.looks.load(Ordering::SeqCst)
    }

    /// Waits until the keeper has begun three more looks, so that two whole
    /// looks came after the call: what a look would do has been done.
    fn looked(&self) {
        let from = self.looks();
        until("the keeper did not look", WAIT_LIMIT, || self.looks() >= from + 3);
    }
}

/// A cloud provider keeping `files` in the cloud, and every file while
/// `everything` is set, which counts downloads.
#[derive(Default)]
struct Provider {
    files: HashSet<PathBuf>,
    everything: AtomicBool,
    fetches: AtomicUsize,
}

impl Cloud for Provider {
    fn is_cloud_only(&self, path: &Path, _: &Metadata) -> bool {
        self.everything.load(Ordering::SeqCst) || self.files.contains(path)
    }

    fn fetch(&self, _: &Path, _: &mut dyn FnMut(u64)) -> std::io::Result<()> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("not for a test"))
    }
}

/// A bridge serving `catalog`, with its data folder `dir`, which the test
/// removes once the bridge is dropped; its keeper starts with
/// [`TestBridge::keep`].
fn served(catalog: Catalog, dir: &Path) -> TestBridge {
    TestBridge::in_dir(App::new("test", policy(), catalog), dir)
}

/// What these tests ask a bridge.
trait Background {
    fn explorer(&self, id: &str) -> (u16, String);

    fn building(&self) -> Vec<(String, &'static str, u64, u64)>;

    /// The indexes searches on database `id` use, where a test holds them.
    fn indexes(&self, id: &str) -> Arc<Indexes>;

    /// Sends a search on database `id`, whose answer the test reads once it
    /// has let the search go: the answer's patience runs from then (#244).
    fn search(&self, id: &str) -> Sent;
}

impl Background for TestBridge {
    fn explorer(&self, id: &str) -> (u16, String) {
        get(self.port, &format!("/v1/databases/{id}/explorer?fen={START}"))
    }

    fn building(&self) -> Vec<(String, &'static str, u64, u64)> {
        self.app.catalog.explorer.building()
    }

    fn indexes(&self, id: &str) -> Arc<Indexes> {
        self.app.catalog.get(id).unwrap().open().ok().unwrap().indexes
    }

    fn search(&self, id: &str) -> Sent {
        Sent::get(self.port, &format!("/v1/databases/{id}/games?q=x"))
    }
}

/// A database in use that changes is rebuilt once it is quiet, without a
/// request; the explorer then answers at once from the new index.
#[test]
fn a_changed_database_in_use_is_rebuilt_without_a_request() {
    let db = copies("background-changed", 20);
    let path = db.dir().join("db.2cbh");
    let (id, dir) = (id_of(&path), data_dir("changed"));
    let bridge = served(Catalog::new([path.clone()]), &dir);
    bridge.keep(TICK, Duration::from_millis(200));
    // The only ready database is in use from the start.
    let first = generation(&path);
    until("the index was not built", WAIT_LIMIT, || built_for(&dir, &id) == Some(first));
    // The index file is in place before the build is over: the explorer
    // answers from it once the build has ended.
    until("the build did not end", WAIT_LIMIT, || bridge.building().is_empty());
    let (status, body) = bridge.explorer(&id);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""games":20,"#), "{body}");
    // More games.
    std::thread::sleep(Duration::from_millis(20));
    let _grown = copies("background-changed", 30);
    let second = generation(&path);
    assert_ne!(first, second);
    until("the index was not rebuilt", WAIT_LIMIT, || built_for(&dir, &id) == Some(second));
    until("the build did not end", WAIT_LIMIT, || bridge.building().is_empty());
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
    // Written again, with 11 or 12 games, as the keeper begins each look,
    // every 50 ms, forty times as often as the quiet period: each look finds
    // a generation the one before did not, however far apart a loaded
    // machine has the looks come.
    let (changing, written) = (Arc::new(AtomicBool::new(true)), Arc::new(Mutex::new(Vec::new())));
    let computer = Arc::new(Computer::default());
    *computer.on_look.lock().unwrap() = Some(Box::new({
        let (changing, written) = (Arc::clone(&changing), Arc::clone(&written));
        move || {
            if changing.load(Ordering::SeqCst) {
                let mut written = written.lock().unwrap();
                let games = 11 + written.len() % 2;
                written.push(copies("background-changing", games));
            }
        }
    }));
    let catalog = Catalog::new([path.clone()]);
    catalog.explorer.set_machine(computer.clone());
    let bridge = served(catalog, &dir);
    bridge.keep(TICK, Duration::from_secs(2));
    let unbuilt = || {
        assert!(built_for(&dir, &id).is_none(), "built while it changed");
        assert!(bridge.building().is_empty(), "queued while it changed: {:?}", bridge.building());
    };
    // For 3 s, longer than the quiet period, then while the keeper begins
    // three more looks.
    let three = Instant::now() + Duration::from_secs(3);
    while Instant::now() < three {
        unbuilt();
        std::thread::sleep(TICK);
    }
    let (looks, deadline) = (computer.looks() + 3, Instant::now() + WAIT_LIMIT);
    while computer.looks() < looks {
        assert!(Instant::now() < deadline, "the keeper did not look");
        unbuilt();
        std::thread::sleep(TICK);
    }
    // No more writes once the look that may be writing is over.
    changing.store(false, Ordering::SeqCst);
    computer.looked();
    let last = generation(&path);
    until("the index was not built once quiet", WAIT_LIMIT, || built_for(&dir, &id) == Some(last));
    drop(bridge);
    written.lock().unwrap().clear();
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
    let bridge = served(catalog, &dir);
    bridge.keep(TICK, Duration::ZERO);
    // The largest database is in use from the start: its build runs.
    until("the background build did not start", WAIT_LIMIT, || {
        phase(bridge.port, &a).is_some_and(|p| !["waiting", "checking"].contains(&p.as_str()))
    });
    let (status, body) = bridge.explorer(&b);
    assert_eq!(status, 409, "{body}");
    until("the requested build did not answer", WAIT_LIMIT, || bridge.explorer(&b).0 == 200);
    assert!(built_for(&dir, &a).is_none(), "the background build ended first");
    assert!(phase(bridge.port, &a).is_some(), "the stopped build waits again");
    // It completes afterwards, asked for by nobody.
    until("the stopped build did not complete", WAIT_LIMIT, || built_for(&dir, &a) == Some(generation(&paths[0])));
    until("the build did not end", WAIT_LIMIT, || bridge.building().is_empty());
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
    let sources = Sources { config: Some(Arc::new(Watched::new(config.clone()))), ..Sources::default() };
    let catalog = Catalog::with_sources(sources, provider.clone());
    let computer = Arc::new(Computer::default());
    catalog.explorer.set_machine(computer.clone());
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
    let bridge = served(catalog, &root);
    bridge.keep(TICK, Duration::ZERO);
    // A build renames its index into place before its state leaves
    // `building()`, so the ready database's build is waited for to end too.
    until("the ready database was not built", WAIT_LIMIT, || {
        built_for(&root, &ids[0]).is_some() && bridge.building().iter().all(|(id, ..)| id != &ids[0])
    });
    computer.looked();
    for (id, what) in ids[1..].iter().zip(["cloud-only", "unlisted", "opening"]) {
        assert!(built_for(&root, id).is_none(), "{what} was built");
    }
    assert!(bridge.building().is_empty(), "{:?}", bridge.building());
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 0, "a database was downloaded");
    // Once opened, the PGN file is built.
    release.send(()).unwrap();
    until("the PGN file was not built once ready", WAIT_LIMIT, || built_for(&root, &ids[3]).is_some());
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
    let computer = Arc::new(Computer::default());
    catalog.explorer.set_machine(computer.clone());
    let bridge = served(catalog, &dir);
    bridge.keep(TICK, Duration::ZERO);
    // Its build leaves `building()` after it renames the index into place.
    until("the largest ready database was not built", WAIT_LIMIT, || {
        built_for(&dir, &ids[1]).is_some() && bridge.building().iter().all(|(id, ..)| id != &ids[1])
    });
    computer.looked();
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
    let bridge = served(catalog, &dir);
    bridge.keep(TICK, Duration::ZERO);
    computer.looked();
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
    computer.looked();
    assert!(bridge.building().is_empty() && built_for(&dir, &a).is_none(), "{:?}", bridge.building());
    std::thread::sleep(Duration::from_millis(20));
    let _changed = copies("background-disk-large", 41);
    until("the changed database was not built", WAIT_LIMIT, || built_for(&dir, &a) == Some(generation(&paths[0])));
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
    let bridge = served(catalog, &dir);
    assert_eq!(bridge.explorer(&ids[0]).0, 409);
    until("the build did not start", WAIT_LIMIT, || phase(bridge.port, &ids[0]).is_some_and(|p| p != "waiting"));
    let (status, body) = bridge.explorer(&ids[1]);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""progress":{"phase":"waiting","done":0,"total":0}"#), "{body}");
    assert_eq!(phase(bridge.port, &ids[1]).as_deref(), Some("waiting"));
    // The keeper queues the third, in use, behind them.
    bridge.keep(TICK, Duration::ZERO);
    until("the background build was not queued", WAIT_LIMIT, || phase(bridge.port, &ids[2]).is_some());
    assert_eq!(phase(bridge.port, &ids[2]).as_deref(), Some("waiting"));
    until("the builds did not end", WAIT_LIMIT, || ids.iter().all(|id| built_for(&dir, id).is_some()));
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
    let bridge = served(catalog, &dir);
    bridge.keep(TICK, Duration::ZERO);
    until("the background build was not queued", WAIT_LIMIT, || phase(bridge.port, &a).is_some());
    computer.looked();
    assert_eq!(phase(bridge.port, &a).as_deref(), Some("waiting"));
    assert!(built_for(&dir, &a).is_none());
    until("the requested build did not answer", WAIT_LIMIT, || bridge.explorer(&b).0 == 200);
    assert!(built_for(&dir, &a).is_none(), "built on battery");
    computer.battery.store(false, Ordering::SeqCst);
    until("the background build did not run on mains power", WAIT_LIMIT, || built_for(&dir, &a).is_some());
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
    backdate(db.dir());
    let (id, dir) = (id_of(&path), data_dir("old"));
    let bridge = served(Catalog::new([path.clone()]), &dir);
    // A tick and a quiet period of an hour: only the first look can build it.
    bridge.keep(Duration::from_secs(3600), Duration::from_secs(60));
    until("the index was not built at the first look", WAIT_LIMIT, || built_for(&dir, &id) == Some(generation(&path)));
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A database replaced while the bridge watches it waits for the quiet
/// period, even when the replacement's files keep older modification times,
/// as a synced or restored copy's do; then it is built.
#[test]
fn a_replaced_database_with_old_modification_times_waits_until_it_is_quiet() {
    let db = copies("background-replaced", 10);
    let path = db.dir().join("db.2cbh");
    backdate(db.dir());
    let (id, dir) = (id_of(&path), data_dir("replaced"));
    let bridge = served(Catalog::new([path.clone()]), &dir);
    let quiet = Duration::from_secs(3);
    bridge.keep(TICK, quiet);
    let first = generation(&path);
    until("the old database was not built at the first look", WAIT_LIMIT, || built_for(&dir, &id) == Some(first));
    until("the first build did not end", WAIT_LIMIT, || bridge.building().is_empty());
    // A copy of more games, its files dated as long ago, moved over the old.
    let next = copies("background-replaced-next", 11);
    backdate(next.dir());
    let replaced = Instant::now();
    for entry in std::fs::read_dir(next.dir()).unwrap() {
        let from = entry.unwrap().path();
        std::fs::rename(&from, db.dir().join(from.file_name().unwrap())).unwrap();
    }
    let second = generation(&path);
    assert_ne!(second, first);
    while replaced.elapsed() < quiet / 2 {
        assert_eq!(built_for(&dir, &id), Some(first), "rebuilt before the quiet period");
        assert!(bridge.building().is_empty(), "queued before the quiet period: {:?}", bridge.building());
        std::thread::sleep(TICK);
    }
    until("the replaced database was not built once quiet", WAIT_LIMIT, || built_for(&dir, &id) == Some(second));
    assert!(replaced.elapsed() >= quiet, "built after {:?}", replaced.elapsed());
    drop(bridge);
    drop(next);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A database queued for a background build that goes to the cloud while the
/// build waits keeps its generation, yet is not read (#149): the build is
/// dropped at its turn, and nothing is downloaded. Back on this computer, it
/// is built.
#[test]
fn a_queued_background_build_is_dropped_when_its_database_goes_to_the_cloud() {
    let root = data_dir("gone-cloud");
    std::fs::create_dir_all(&root).unwrap();
    let db = copies("background-gone-cloud", 10);
    let path = db.dir().join("db.2cbh");
    let id = id_of(&path);
    let config = root.join("bridge.toml");
    std::fs::write(&config, format!("databases = ['{}']\n", path.display())).unwrap();
    let provider = Arc::new(Provider::default());
    let sources = Sources { config: Some(Arc::new(Watched::new(config.clone()))), ..Sources::default() };
    let catalog = Catalog::with_sources(sources, provider.clone());
    let computer = Arc::new(Computer::default());
    computer.battery.store(true, Ordering::SeqCst);
    catalog.explorer.set_machine(computer.clone());
    let bridge = served(catalog, &root);
    bridge.keep(TICK, Duration::ZERO);
    // The only database is in use from the start; on battery its build waits.
    until("the background build was not queued", WAIT_LIMIT, || phase(bridge.port, &id).as_deref() == Some("waiting"));
    let queued = generation(&path);
    provider.everything.store(true, Ordering::SeqCst);
    let entry = bridge.app.catalog.get(&id).unwrap();
    assert!(entry.open().is_err(), "a cloud-only database opened");
    assert_eq!(entry.generation(), Some(queued), "going to the cloud changed the generation");
    computer.battery.store(false, Ordering::SeqCst);
    until("the build was not dropped", WAIT_LIMIT, || bridge.building().is_empty());
    computer.looked();
    assert!(built_for(&root, &id).is_none(), "a cloud-only database was read");
    assert!(bridge.building().is_empty(), "queued while cloud-only: {:?}", bridge.building());
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 0, "the database was downloaded");
    provider.everything.store(false, Ordering::SeqCst);
    until("the database was not built once back", WAIT_LIMIT, || built_for(&root, &id) == Some(queued));
    drop(bridge);
    std::fs::remove_dir_all(&root).unwrap();
}

/// A database renamed in the ChessBase window while its background build
/// waits, then taken off the window, is not read (#149): the build, queued
/// under the former name, is dropped at its turn.
#[test]
fn a_queued_background_build_is_dropped_when_its_renamed_database_leaves_the_list() {
    let root = data_dir("renamed-gone");
    let chessbase = root.join("ChessBase");
    std::fs::create_dir_all(&chessbase).unwrap();
    let db = copies("background-renamed-gone", 10);
    let path = db.dir().join("db.2cbh");
    let id = id_of(&path);
    let window = |titles: &[&str]| {
        let mut f = DbItems::new();
        f.section("2cbg");
        for title in titles {
            f.database(&path.to_string_lossy(), title, WINDOW_NUMBERS);
        }
        std::fs::write(chessbase.join("DBItems.cbini"), f.bytes()).unwrap();
    };
    window(&["Before"]);
    let sources = Sources { chessbase: Some(chessbase.clone()), ..Sources::default() };
    let catalog = Catalog::with_sources(sources, Arc::new(Provider::default()));
    let computer = Arc::new(Computer::default());
    computer.battery.store(true, Ordering::SeqCst);
    catalog.explorer.set_machine(computer.clone());
    let bridge = served(catalog, &root);
    bridge.keep(TICK, Duration::ZERO);
    // The only database is in use from the start; on battery its build waits.
    until("the background build was not queued", WAIT_LIMIT, || phase(bridge.port, &id).as_deref() == Some("waiting"));
    window(&["Renamed later"]);
    let catalog = &bridge.app.catalog;
    until("the new name was not read", WAIT_LIMIT, || catalog.get(&id).is_some_and(|e| e.name == "Renamed later"));
    window(&[]);
    until("the database did not leave the list", WAIT_LIMIT, || catalog.get(&id).is_some_and(|e| !e.listed()));
    computer.battery.store(false, Ordering::SeqCst);
    until("the build was not dropped", WAIT_LIMIT, || bridge.building().is_empty());
    computer.looked();
    assert!(built_for(&root, &id).is_none(), "a database off the list was read");
    drop(bridge);
    std::fs::remove_dir_all(&root).unwrap();
}

/// A background build gives way to a search (#149): while the search runs,
/// the build waits before its first batch, not a record read; once the
/// search ends, it goes on at once, long before its patience runs out, and
/// the index is built.
#[test]
fn a_background_build_gives_way_to_a_search() {
    if !in_child("a_background_build_gives_way_to_a_search", CHILD, &[]) {
        return;
    }
    let db = copies("background-gives-way", 20);
    let path = db.dir().join("db.2cbh");
    // How long the same games take to build on this computer now, as a build
    // that never gives way.
    let alone = {
        let scratch = index_dir("gives-way-alone");
        let started = Instant::now();
        drop(
            bridge::explorer::prepare(&Database::open(&path).unwrap(), 1, &scratch, "db", &Progress::default())
                .unwrap(),
        );
        let took = started.elapsed();
        std::fs::remove_dir_all(&scratch).unwrap();
        took
    };
    let (id, dir) = (id_of(&path), data_dir("gives-way"));
    let bridge = served(Catalog::new([path.clone()]), &dir);
    bridge.app.catalog.explorer.set_patience(LONG_PATIENCE);
    let indexes = bridge.indexes(&id);
    let held = indexes.gate().hold(1);
    let search = bridge.search(&id);
    assert!(held.arrived(1, WAIT_LIMIT));
    bridge.keep(TICK, Duration::ZERO);
    until("the background build did not start", WAIT_LIMIT, || phase(bridge.port, &id).as_deref() == Some("reading"));
    // Twenty games take milliseconds to build: a build that went on would be
    // over in ten times as long as they took alone, or a second.
    std::thread::sleep((alone * 10).max(Duration::from_secs(1)));
    assert_eq!(bridge.building(), [(id.clone(), "reading", 0, 20)], "it went on while the search ran");
    assert!(built_for(&dir, &id).is_none());
    drop(held);
    let (status, body) = search.answer();
    assert_eq!(status, 200, "{body}");
    until("the build did not go on once the search ended", WAIT_LIMIT, || {
        built_for(&dir, &id) == Some(generation(&path))
    });
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A requested build never gives way (#149): while a search runs and the
/// background build of one database gives way to it, a request for another
/// database's positions stops that build and is built; a request for the
/// first database's positions then makes its build a requested one, which
/// goes on and is built. The search runs all along, and both builds are
/// done long before a background build's patience would run out.
#[test]
fn a_requested_build_does_not_give_way() {
    if !in_child("a_requested_build_does_not_give_way", CHILD, &[]) {
        return;
    }
    let (large, small) = (copies("background-no-way-large", 40), copies("background-no-way-small", 10));
    let paths = [large.dir().join("db.2cbh"), small.dir().join("db.2cbh")];
    let (a, b) = (id_of(&paths[0]), id_of(&paths[1]));
    let dir = data_dir("no-way");
    let bridge = served(Catalog::new(paths.clone()), &dir);
    bridge.app.catalog.explorer.set_patience(LONG_PATIENCE);
    let indexes = bridge.indexes(&b);
    let held = indexes.gate().hold(1);
    let search = bridge.search(&b);
    assert!(held.arrived(1, WAIT_LIMIT));
    // The largest database is in use from the start: its build gives way.
    bridge.keep(TICK, Duration::ZERO);
    until("the background build did not start", WAIT_LIMIT, || phase(bridge.port, &a).as_deref() == Some("reading"));
    until("the requested build did not answer", WAIT_LIMIT, || bridge.explorer(&b).0 == 200);
    assert!(built_for(&dir, &a).is_none(), "the background build went on");
    until("the promoted build did not answer", WAIT_LIMIT, || bridge.explorer(&a).0 == 200);
    assert!(!search.answered(), "the search ended");
    drop(held);
    let (status, body) = search.answer();
    assert_eq!(status, 200, "{body}");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A background build ends under foreground work that never does (#149):
/// each of its threads gives way for its patience at most, then goes on with
/// one batch and gives way again. Its five kinds of pass (the stream pass,
/// the tree's replays and writes, the deep section's replays and writes)
/// each wait their whole patience at least once, one after the other.
#[test]
fn a_background_build_ends_while_a_search_never_does() {
    if !in_child("a_background_build_ends_while_a_search_never_does", CHILD, &[]) {
        return;
    }
    let db = copies("background-starved", 300);
    let path = db.dir().join("db.2cbh");
    let (id, dir) = (id_of(&path), data_dir("starved"));
    let bridge = served(Catalog::new([path.clone()]), &dir);
    let patience = Duration::from_millis(50);
    bridge.app.catalog.explorer.set_patience(patience);
    let indexes = bridge.indexes(&id);
    let held = indexes.gate().hold(1);
    let search = bridge.search(&id);
    assert!(held.arrived(1, WAIT_LIMIT));
    let started = Instant::now();
    bridge.keep(TICK, Duration::ZERO);
    until("the build did not end while the search ran", WAIT_LIMIT, || built_for(&dir, &id) == Some(generation(&path)));
    let took = started.elapsed();
    assert!(took >= patience * 5, "it gave way less than its patience per pass: {took:?}");
    assert!(!search.answered(), "the search ended");
    drop(held);
    let (status, body) = search.answer();
    assert_eq!(status, 200, "{body}");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}
