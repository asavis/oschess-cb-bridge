//! The list of databases (`docs/api.md`, "GET /v1/databases"): ChessBase's
//! window list, `bridge.toml` and the command line, read again when they
//! change, and cloud-only databases downloaded when they are opened.

use std::collections::HashSet;
use std::fs::Metadata;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, Entry, State, id_of};
use bridge::fetch::Cloud;
use bridge::server;
use bridge::sources::Sources;
use cbformat::fixture::{Builder, DbItems, TempDb, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const NUMBERS: [i64; 6] = [0, 28, 1, 1, 1037620, 1037559];

/// A temporary folder standing for Documents, removed on drop.
struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Root {
        let dir = std::env::temp_dir().join(format!("bridge-databases-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ChessBase")).unwrap();
        Root(dir)
    }

    fn chessbase(&self) -> PathBuf {
        self.0.join("ChessBase")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Writes the window list with `(path, title)` entries, 2CBH ones in their
    /// section and the others in `Databases`, as ChessBase does.
    fn window(&self, entries: &[(&Path, &str)]) {
        let mut f = DbItems::new();
        f.section("2cbg");
        for (p, title) in entries.iter().filter(|(p, _)| p.extension().is_some_and(|e| e == "2cbh")) {
            f.database(&p.to_string_lossy(), title, NUMBERS);
        }
        f.section("Databases");
        for (p, title) in entries.iter().filter(|(p, _)| !p.extension().is_some_and(|e| e == "2cbh")) {
            f.database(&p.to_string_lossy(), title, NUMBERS);
        }
        std::fs::write(self.chessbase().join("DBItems.cbini"), f.bytes()).unwrap();
    }

    fn sources(&self) -> Sources {
        Sources { chessbase: Some(self.chessbase()), config: Some(self.path("bridge.toml")), fixed: Vec::new() }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A one-game database written under `dir` as `stem.2cbh` and its companions.
fn database_at(dir: &Path, stem: &str) -> PathBuf {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    // Named after the whole folder, so that tests running at once never share it.
    let unique = dir.to_string_lossy().replace(['/', '\\', ':'], "-");
    let db: TempDb = b.write(&format!("databases{unique}-{stem}"));
    std::fs::create_dir_all(dir).unwrap();
    for ext in ["2cbh", "2cbg", "2lid"] {
        std::fs::copy(db.dir().join(format!("db.{ext}")), dir.join(format!("{stem}.{ext}"))).unwrap();
    }
    dir.join(format!("{stem}.2cbh"))
}

fn names(catalog: &Catalog) -> Vec<String> {
    catalog.entries().iter().map(|e| e.name.clone()).collect()
}

fn states(catalog: &Catalog) -> Vec<&'static str> {
    catalog.entries().iter().map(|e| e.state().name()).collect()
}

/// The window's databases in its order and with its titles, then the
/// configured ones (a folder's by file name), then the command line's; each
/// database once.
#[test]
fn the_window_comes_first_then_bridge_toml_then_the_command_line() {
    let root = Root::new("order");
    let a = database_at(&root.path("bases"), "Alpha");
    let b = database_at(&root.path("bases"), "Beta");
    let c = database_at(&root.path("folder"), "Gamma");
    let d = database_at(&root.path("folder"), "Delta");
    let old = root.path("folder/Old.cbh");
    std::fs::write(&old, [0u8; 46]).unwrap();
    std::fs::write(root.path("folder/notes.txt"), "not a database").unwrap();
    let pgn = root.path("bases/Games.pgn");
    std::fs::write(&pgn, "[Event \"?\"]\n\n*\n").unwrap();
    let fixed = database_at(&root.path("cli"), "Cli");
    root.window(&[(&b, "Beta (2cbh)"), (&pgn, ""), (&a, ""), (&root.path("bases/Gone.2cbh"), "Gone")]);
    std::fs::write(
        root.path("bridge.toml"),
        format!("databases = ['{}', '{}']\n", root.path("folder").display(), a.display()),
    )
    .unwrap();
    let mut sources = root.sources();
    sources.fixed = vec![fixed, b.clone()];
    let catalog = Catalog::with_sources(sources, Arc::new(bridge::fetch::System));

    // 2CBH entries first in the file, as ChessBase writes them; an empty
    // title shows the file name.
    assert_eq!(names(&catalog), ["Beta (2cbh)", "Alpha", "Gone", "Games", "Delta", "Gamma", "Old", "Cli"]);
    assert_eq!(
        states(&catalog),
        ["ready", "ready", "missing", "unsupported", "ready", "ready", "unsupported", "ready"]
    );
    let entries = catalog.entries();
    assert_eq!(entries[0].id, id_of(&b));
    assert!(catalog.get(&id_of(&d)).is_some());
    assert_eq!(catalog.get(&id_of(&c)).unwrap().format.name(), "2cbh");
    assert_eq!(catalog.get(&id_of(&old)).unwrap().format.name(), "cbh");
}

/// Changes to the window list and to `bridge.toml` show on the next listing.
/// A database that leaves the list stays, reported missing, under its id.
#[test]
fn the_list_is_read_again_when_its_sources_change() {
    let root = Root::new("reload");
    let a = database_at(&root.path("bases"), "Alpha");
    let b = database_at(&root.path("bases"), "Beta");
    let c = database_at(&root.path("more"), "Gamma");
    root.window(&[(&a, ""), (&b, "")]);
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(names(&catalog), ["Alpha", "Beta"]);
    let alpha = catalog.get(&id_of(&a)).unwrap();
    assert!(alpha.open().is_ok());

    // Beta leaves the window, a renamed Alpha stays the same database.
    root.window(&[(&a, "Alpha renamed")]);
    assert_eq!(names(&catalog), ["Alpha renamed", "Beta"]);
    assert_eq!(states(&catalog), ["ready", "missing"]);
    assert_eq!(catalog.get(&id_of(&b)).unwrap().state(), State::Missing);

    // bridge.toml gains a database without a restart; Beta comes back.
    std::fs::write(root.path("bridge.toml"), format!("databases = ['{}']\n", c.display())).unwrap();
    root.window(&[(&b, ""), (&a, "Alpha renamed")]);
    assert_eq!(names(&catalog), ["Beta", "Alpha renamed", "Gamma"]);
    assert_eq!(states(&catalog), ["ready", "ready", "ready"]);

    // A broken bridge.toml keeps the databases it had.
    std::fs::write(root.path("bridge.toml"), "databases = [unquoted]\n").unwrap();
    assert_eq!(names(&catalog), ["Beta", "Alpha renamed", "Gamma"]);

    // A database deleted from disk is missing where it stands.
    for ext in ["2cbh", "2cbg", "2lid"] {
        std::fs::remove_file(root.path(&format!("bases/Beta.{ext}"))).unwrap();
    }
    assert_eq!(states(&catalog), ["missing", "ready", "ready"]);
}

/// A new database in a configured folder shows on the next listing.
#[test]
fn a_configured_folder_is_read_again_when_it_changes() {
    let root = Root::new("folder");
    database_at(&root.path("folder"), "One");
    std::fs::write(root.path("bridge.toml"), format!("databases = ['{}']\n", root.path("folder").display())).unwrap();
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(names(&catalog), ["One"]);
    database_at(&root.path("folder"), "Two");
    assert_eq!(names(&catalog), ["One", "Two"]);
}

/// Damaged, empty or absent lists give no databases and no panic; a list that
/// becomes damaged later keeps the databases it had.
#[test]
fn damaged_empty_and_absent_lists() {
    let root = Root::new("damaged");
    let a = database_at(&root.path("bases"), "Alpha");
    let list = root.chessbase().join("DBItems.cbini");
    for bytes in [&b""[..], b"\x0c\x0b\x0a\x0e\x00\x00\x00\x05\x19\x01", b"garbage that is no list"] {
        std::fs::write(&list, bytes).unwrap();
        let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
        assert!(catalog.entries().is_empty());
    }
    std::fs::remove_file(&list).unwrap();
    assert!(Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System)).entries().is_empty());
    let absent = Sources { chessbase: Some(root.path("nowhere")), ..Sources::default() };
    assert!(Catalog::with_sources(absent, Arc::new(bridge::fetch::System)).entries().is_empty());

    root.window(&[(&a, "")]);
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(names(&catalog), ["Alpha"]);
    std::fs::write(&list, b"now damaged, and longer than before").unwrap();
    assert_eq!(names(&catalog), ["Alpha"]);
    assert_eq!(states(&catalog), ["ready"]);
}

/// `DBItems-<computer>.cbini` is a sync-conflict copy, which ChessBase does not
/// read: it is ignored.
#[test]
fn sync_conflict_copies_are_ignored() {
    let root = Root::new("conflict");
    let a = database_at(&root.path("bases"), "Alpha");
    let b = database_at(&root.path("bases"), "Beta");
    root.window(&[(&b, "")]);
    std::fs::rename(root.chessbase().join("DBItems.cbini"), root.chessbase().join("DBItems-OTHERPC.cbini")).unwrap();
    root.window(&[(&a, "")]);
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(names(&catalog), ["Alpha"]);
}

/// Cloud-only files as a provider shows them. A fetch reads the file, can be
/// held until released, can fail, and makes the file local unless `sticky`.
#[derive(Default)]
struct FakeCloud {
    cloud: Mutex<HashSet<PathBuf>>,
    failing: Mutex<HashSet<PathBuf>>,
    /// The attribute stays after a fetch, as with a provider that keeps it.
    sticky: bool,
    held: Mutex<bool>,
    freed: Condvar,
    fetches: AtomicUsize,
    running: AtomicUsize,
    most_at_once: AtomicUsize,
}

impl FakeCloud {
    fn with_files(files: impl IntoIterator<Item = PathBuf>, sticky: bool) -> FakeCloud {
        FakeCloud { cloud: Mutex::new(files.into_iter().collect()), sticky, ..FakeCloud::default() }
    }

    fn hold(&self, held: bool) {
        *self.held.lock().unwrap() = held;
        self.freed.notify_all();
    }
}

impl Cloud for FakeCloud {
    fn is_cloud_only(&self, path: &Path, _: &Metadata) -> bool {
        self.cloud.lock().unwrap().contains(path)
    }

    fn fetch(&self, path: &Path, read: &mut dyn FnMut(u64)) -> std::io::Result<()> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_at_once.fetch_max(now, Ordering::SeqCst);
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.freed.wait(held).unwrap();
        }
        drop(held);
        let result = if self.failing.lock().unwrap().contains(path) {
            Err(std::io::Error::other("the provider is offline"))
        } else {
            read(std::fs::metadata(path)?.len());
            if !self.sticky {
                self.cloud.lock().unwrap().remove(path);
            }
            Ok(())
        };
        self.running.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

fn files_of(db: &Path) -> Vec<PathBuf> {
    ["2cbh", "2cbg", "2lid"].iter().map(|ext| db.with_extension(ext)).collect()
}

fn size_of(files: &[PathBuf]) -> u64 {
    files.iter().map(|f| std::fs::metadata(f).unwrap().len()).sum()
}

fn wait_for(entry: &Entry, state: State) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while entry.state() != state {
        assert!(Instant::now() < deadline, "still {:?}, waiting for {state:?}", entry.state());
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Listing never reads a cloud-only database; opening it for games downloads
/// it in the background, with its progress, and it is then ready.
#[test]
fn a_cloud_only_database_downloads_when_opened() {
    for sticky in [false, true] {
        let root = Root::new(if sticky { "cloud-sticky" } else { "cloud" });
        let db = database_at(&root.path("bases"), "Remote");
        let files = files_of(&db);
        // Only the moves file is in the cloud: the others count as present.
        let cloud = Arc::new(FakeCloud::with_files([files[1].clone()], sticky));
        root.window(&[(&db, "")]);
        let catalog = Catalog::with_sources(root.sources(), cloud.clone());
        let entry = catalog.get(&id_of(&db)).unwrap();
        assert_eq!(states(&catalog), ["cloudOnly"]);
        assert_eq!(entry.size(), size_of(&files));
        assert_eq!(cloud.fetches.load(Ordering::SeqCst), 0, "listing fetched a file");

        cloud.hold(true);
        assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
        assert_eq!(entry.state(), State::Downloading);
        let p = entry.progress().unwrap();
        assert_eq!((p.present(), p.total), (size_of(&files) - size_of(&files[1..2]), size_of(&files)));
        // A second request joins the download instead of starting another.
        assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
        cloud.hold(false);
        wait_for(&entry, State::Ready);
        assert_eq!(cloud.fetches.load(Ordering::SeqCst), 1);
        assert!(entry.progress().is_none());
        assert!(entry.open_to_read().is_ok());
    }
}

/// A failed download leaves the database cloud-only, and the next request
/// tries again.
#[test]
fn a_failed_download_can_be_tried_again() {
    let root = Root::new("cloud-fail");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    let cloud = Arc::new(FakeCloud::with_files(files.clone(), false));
    cloud.failing.lock().unwrap().insert(files[1].clone());
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let entry = catalog.get(&id_of(&db)).unwrap();
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    wait_for(&entry, State::CloudOnly);
    // The first file came down before the second failed.
    assert!(!cloud.cloud.lock().unwrap().contains(&files[0]));
    cloud.failing.lock().unwrap().clear();
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    wait_for(&entry, State::Ready);
}

/// Databases download one at a time; a queued one reports downloading.
#[test]
fn downloads_run_one_at_a_time() {
    let root = Root::new("cloud-queue");
    let dbs: Vec<PathBuf> = ["One", "Two", "Three"].iter().map(|s| database_at(&root.path("bases"), s)).collect();
    let cloud = Arc::new(FakeCloud::with_files(dbs.iter().flat_map(|d| files_of(d)), false));
    let catalog = Catalog::with_sources(Sources { fixed: dbs.clone(), ..Sources::default() }, cloud.clone());
    cloud.hold(true);
    let entries: Vec<_> = dbs.iter().map(|d| catalog.get(&id_of(d)).unwrap()).collect();
    for e in &entries {
        assert!(matches!(e.open_to_read(), Err(State::Downloading)));
    }
    assert_eq!(states(&catalog), ["downloading"; 3]);
    cloud.hold(false);
    for e in &entries {
        wait_for(e, State::Ready);
    }
    assert_eq!(cloud.most_at_once.load(Ordering::SeqCst), 1);
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 9);
}

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split(' ').nth(1).unwrap().parse().unwrap();
    let body = out.split_once("\r\n\r\n").unwrap().1.to_string();
    (status, body)
}

/// The contract's rows and answers for a cloud-only database.
#[test]
fn cloud_states_over_http() {
    let root = Root::new("cloud-http");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    let size = size_of(&files);
    let cloud = Arc::new(FakeCloud::with_files(files.clone(), false));
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone()),
        between_reads: None,
    };
    std::thread::spawn(move || server::serve(listeners, Arc::new(app)));
    let id = id_of(&db);

    let (status, body) = get(port, "/v1/databases");
    assert_eq!(status, 200);
    assert!(body.contains(&format!("\"state\":\"cloudOnly\",\"size\":{size}")), "{body}");
    let (_, body) = get(port, "/v1/status");
    assert!(body.contains("\"cloudOnly\":1,\"downloading\":0"), "{body}");
    assert!(!body.contains("\"download\""), "{body}");

    cloud.hold(true);
    let (status, body) = get(port, &format!("/v1/databases/{id}/games"));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("\"code\":\"database_unavailable\"") && body.contains("\"state\":\"downloading\""), "{body}");
    let (status, body) = get(port, &format!("/v1/databases/{id}/games/1"));
    assert_eq!(status, 409, "{body}");
    let (_, body) = get(port, "/v1/databases");
    assert!(
        body.contains(&format!(
            "\"state\":\"downloading\",\"size\":{size},\"progress\":{{\"present\":0,\"total\":{size}}}"
        )),
        "{body}"
    );
    let (_, body) = get(port, "/v1/status");
    assert!(body.contains("\"cloudOnly\":0,\"downloading\":1"), "{body}");
    assert!(body.contains(&format!("\"download\":{{\"present\":0,\"total\":{size}}}")), "{body}");

    cloud.hold(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games"));
        if status == 200 {
            assert!(body.contains("\"total\":1"), "{body}");
            break;
        }
        assert!(Instant::now() < deadline, "{status} {body}");
        std::thread::sleep(Duration::from_millis(5));
    }
    let (_, body) = get(port, "/v1/databases");
    assert!(body.contains("\"state\":\"ready\",\"records\":1"), "{body}");
}

/// The binary finds the window list through `OSCHESS_BRIDGE_DOCUMENTS`.
#[test]
fn the_bridge_reads_the_window_of_the_documents_folder() {
    let root = Root::new("binary");
    let a = database_at(&root.path("bases"), "Alpha");
    root.window(&[(&a, "Windowed")]);
    let home = root.path("home");
    std::fs::create_dir_all(&home).unwrap();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oschess-bridge"))
        .env("OSCHESS_BRIDGE_HOME", &home)
        .env("OSCHESS_BRIDGE_DOCUMENTS", &root.0)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut text = String::new();
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(10);
    while !text.contains("Windowed") && Instant::now() < deadline {
        match out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(text.contains(&format!("{} [ready] Windowed", id_of(&a))), "{text}");
}
