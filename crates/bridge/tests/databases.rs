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

    /// [`Root::window`] from a handle that must not remove the folder.
    fn window_keep(self, entries: &[(&Path, &str)]) {
        self.window(entries);
        std::mem::forget(self);
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

    /// The provider moves `files` back to the cloud, keeping their sizes and
    /// modification times.
    fn evict(&self, files: &[PathBuf]) {
        self.cloud.lock().unwrap().extend(files.iter().cloned());
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
    let root = Root::new("cloud");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    // Only the moves file is in the cloud: the others count as present.
    let cloud = Arc::new(FakeCloud::with_files([files[1].clone()], false));
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
    assert!(!entry.marks_kept());
    assert!(entry.open_to_read().is_ok());
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

/// A download whose thread cannot start leaves the database cloud-only, not
/// downloading for ever: once threads start again, the next request for its
/// games downloads it.
#[test]
fn a_download_that_cannot_start_is_tried_again() {
    let root = Root::new("no-thread");
    let db = database_at(&root.path("bases"), "Remote");
    let cloud = Arc::new(FakeCloud::with_files(files_of(&db), false));
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let entry = catalog.get(&id_of(&db)).unwrap();
    catalog.downloads().refuse_starts(true);
    for _ in 0..3 {
        assert!(matches!(entry.open_to_read(), Err(State::CloudOnly)));
        assert_eq!(entry.state(), State::CloudOnly);
        assert!(entry.progress().is_none());
    }
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 0);
    catalog.downloads().refuse_starts(false);
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    wait_for(&entry, State::Ready);
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 3);
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

/// A downloaded database that the provider moves back to the cloud is
/// cloud-only again, although its sizes and times are unchanged: listing does
/// not show it ready, and its games download it again through the queue.
/// Both orders: evicted after its games were read, and before.
#[test]
fn a_database_moved_back_to_the_cloud_is_cloud_only_again() {
    for read_first in [true, false] {
        let root = Root::new(if read_first { "evict-after-read" } else { "evict-before-read" });
        let db = database_at(&root.path("bases"), "Remote");
        let files = files_of(&db);
        let cloud = Arc::new(FakeCloud::with_files(files.clone(), false));
        let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
        let entry = catalog.get(&id_of(&db)).unwrap();
        assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
        wait_for(&entry, State::Ready);
        if read_first {
            assert!(entry.open_to_read().is_ok());
        }
        let fetched = cloud.fetches.load(Ordering::SeqCst);
        assert_eq!(fetched, 3);

        cloud.evict(&files);
        assert_eq!(states(&catalog), ["cloudOnly"]);
        assert_eq!(entry.generation(), catalog.get(&id_of(&db)).unwrap().generation());
        assert_eq!(cloud.fetches.load(Ordering::SeqCst), fetched, "listing fetched a file");
        assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
        wait_for(&entry, State::Ready);
        assert_eq!(cloud.fetches.load(Ordering::SeqCst), fetched + 3);
    }
}

/// Waits until the database's download, running or queued, has ended.
fn wait_for_download(entry: &Entry) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while entry.progress().is_some() {
        assert!(Instant::now() < deadline, "the download does not end");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A companion file moved to the cloud while the download of another ran
/// keeps the database cloud-only when it ends, and the next request for its
/// games downloads that file.
#[test]
fn a_file_moved_to_the_cloud_during_a_download_is_downloaded_next_time() {
    let root = Root::new("evict-during");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    let cloud = Arc::new(FakeCloud::with_files([files[0].clone()], false));
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let entry = catalog.get(&id_of(&db)).unwrap();
    cloud.hold(true);
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    cloud.evict(&files[1..2]);
    cloud.hold(false);
    wait_for_download(&entry);
    assert_eq!(entry.state(), State::CloudOnly);
    assert!(!entry.marks_kept(), "the downloaded file lost its mark");
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 1);
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    wait_for(&entry, State::Ready);
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 2);
}

/// A provider that keeps a file marked after every byte of it was read
/// leaves the database cloud-only, reported once; nothing downloads again
/// until the next request for its games.
#[test]
fn a_mark_kept_after_a_download_keeps_the_database_cloud_only() {
    let root = Root::new("kept-mark");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    let cloud = Arc::new(FakeCloud::with_files([files[1].clone()], true));
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let entry = catalog.get(&id_of(&db)).unwrap();
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    wait_for_download(&entry);
    assert_eq!(entry.state(), State::CloudOnly);
    assert!(entry.marks_kept());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(states(&catalog), ["cloudOnly"]);
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 1, "a kept mark started a download by itself");
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    wait_for_download(&entry);
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 2);
}

/// A database whose marks clear, however that happens, is ready at once and
/// opens without a download.
#[test]
fn a_database_whose_marks_clear_is_ready() {
    let root = Root::new("marks-clear");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    let cloud = Arc::new(FakeCloud::with_files(files.clone(), true));
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let entry = catalog.get(&id_of(&db)).unwrap();
    assert_eq!(entry.state(), State::CloudOnly);
    cloud.cloud.lock().unwrap().clear();
    assert_eq!(entry.state(), State::Ready);
    assert!(entry.open_to_read().is_ok());
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 0);
}

/// A source that changes while the list is read is read again on the next
/// request, even when the change came after it was read.
#[test]
fn a_source_changed_while_it_is_read_is_read_again() {
    let root = Root::new("mid-refresh");
    let a = database_at(&root.path("bases"), "Alpha");
    let b = database_at(&root.path("bases"), "Beta");
    let c = database_at(&root.path("bases"), "Gamma2");
    root.window(&[(&a, "")]);
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(names(&catalog), ["Alpha"]);

    let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (root_dir, a2, b2) = (root.0.clone(), a.clone(), b.clone());
    catalog.after_read(move || {
        if !once.swap(true, Ordering::SeqCst) {
            Root(root_dir.clone()).window_keep(&[(&a2, ""), (&b2, "")]);
        }
    });
    root.window(&[(&a, ""), (&c, "")]);
    // This request read the list before the hook changed it again.
    assert_eq!(names(&catalog), ["Alpha", "Gamma2"]);
    // The next one reads the change made during that read; Gamma2 left the list.
    assert_eq!(names(&catalog), ["Alpha", "Beta", "Gamma2"]);
    assert_eq!(states(&catalog), ["ready", "ready", "missing"]);
}

/// Pipes and folders named like database files are never opened: a folder's
/// discovery skips them, and a database with one among its files is
/// unreadable. Nothing blocks, and a pipe as `bridge.toml` is an error.
#[cfg(unix)]
#[test]
fn files_that_are_not_regular_are_never_opened() {
    use std::process::Command;
    use std::sync::mpsc;

    let root = Root::new("pipes");
    let mkfifo = |path: &Path| assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
    let folder = root.path("folder");
    let good = database_at(&folder, "Good");
    mkfifo(&folder.join("Pipe.2cbh"));
    std::fs::create_dir_all(folder.join("Dir.2cbh")).unwrap();
    // A database whose moves file is a pipe, and one whose header file is.
    let companion = database_at(&root.path("bases"), "Companion");
    std::fs::remove_file(companion.with_extension("2cbg")).unwrap();
    mkfifo(&companion.with_extension("2cbg"));
    let header = root.path("bases/Header.2cbh");
    mkfifo(&header);
    std::fs::write(
        root.path("bridge.toml"),
        format!("databases = ['{}', '{}', '{}']\n", folder.display(), companion.display(), header.display()),
    )
    .unwrap();
    let pipe_config = root.path("pipe.toml");
    mkfifo(&pipe_config);

    let (sources, good_id, companion_id) = (root.sources(), id_of(&good), id_of(&companion));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let catalog = Catalog::with_sources(sources, Arc::new(bridge::fetch::System));
        let listed: Vec<(String, &str)> =
            catalog.entries().iter().map(|e| (e.name.clone(), e.state().name())).collect();
        let games = catalog.get(&companion_id).unwrap().open_to_read().err();
        let ready = catalog.get(&good_id).unwrap().open_to_read().is_ok();
        let config = Sources { config: Some(pipe_config.clone()), ..Sources::default() }.configured().is_err();
        let startup = bridge::config::load_or_create(&pipe_config).is_err();
        tx.send((listed, games, ready, config, startup)).unwrap();
    });
    let (listed, games, ready, config, startup) =
        rx.recv_timeout(Duration::from_secs(20)).expect("a pipe blocked the bridge");
    let listed: Vec<(&str, &str)> = listed.iter().map(|(n, s)| (n.as_str(), *s)).collect();
    assert_eq!(listed, [("Good", "ready"), ("Companion", "unreadable"), ("Header", "unreadable")]);
    assert_eq!(games, Some(State::Unreadable));
    assert!(ready && config && startup);
}

/// A window list or `bridge.toml` that cannot be read for a while keeps the
/// databases read before, and is read as soon as it can be again, although
/// its size and time did not change meanwhile.
#[cfg(unix)]
#[test]
fn a_source_unreadable_for_a_while_is_read_again() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();

    let root = Root::new("unreadable-source");
    let a = database_at(&root.path("bases"), "Alpha");
    let b = database_at(&root.path("bases"), "Beta");
    let c = database_at(&root.path("more"), "Gamma");
    let (list, toml) = (root.chessbase().join("DBItems.cbini"), root.path("bridge.toml"));
    root.window(&[(&a, "")]);
    std::fs::write(&toml, "databases = []\n").unwrap();
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(names(&catalog), ["Alpha"]);

    root.window(&[(&a, ""), (&b, "")]);
    mode(&list, 0o000);
    if std::fs::read(&list).is_ok() {
        mode(&list, 0o644);
        eprintln!("skipped: permissions do not apply to this user");
        return;
    }
    assert_eq!(names(&catalog), ["Alpha"]);
    assert_eq!(names(&catalog), ["Alpha"]);
    mode(&list, 0o644);
    assert_eq!(names(&catalog), ["Alpha", "Beta"]);

    std::fs::write(&toml, format!("databases = ['{}']\n", c.display())).unwrap();
    mode(&toml, 0o000);
    assert_eq!(names(&catalog), ["Alpha", "Beta"]);
    mode(&toml, 0o644);
    assert_eq!(names(&catalog), ["Alpha", "Beta", "Gamma"]);
    assert_eq!(states(&catalog), ["ready"; 3]);
}

/// A configured folder that cannot be listed for a while keeps its
/// databases ready, and is listed again as soon as it can be: a database
/// added meanwhile then appears.
#[cfg(unix)]
#[test]
fn a_folder_unreadable_for_a_while_keeps_its_databases() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();

    let root = Root::new("unreadable-folder");
    let folder = root.path("folder");
    database_at(&folder, "One");
    let toml = root.path("bridge.toml");
    std::fs::write(&toml, format!("databases = ['{}']\n", folder.display())).unwrap();
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(states(&catalog), ["ready"]);

    // Not listable, still traversable and writable.
    mode(&folder, 0o311);
    if std::fs::read_dir(&folder).is_ok() {
        mode(&folder, 0o755);
        eprintln!("skipped: permissions do not apply to this user");
        return;
    }
    database_at(&folder, "Two");
    std::fs::write(&toml, format!("# changed\ndatabases = ['{}']\n", folder.display())).unwrap();
    assert_eq!(names(&catalog), ["One"]);
    assert_eq!(states(&catalog), ["ready"]);
    mode(&folder, 0o755);
    assert_eq!(names(&catalog), ["One", "Two"]);
    assert_eq!(states(&catalog), ["ready", "ready"]);
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

/// The snapshot a user interface polls shows a cloud database as the list
/// does, without reading it: cloud-only, downloading once its games were
/// asked for, then ready.
#[test]
fn the_snapshot_shows_cloud_states() {
    use bridge::snapshot::Background;
    use bridge::start::Bridge;

    let root = Root::new("snapshot");
    let db = database_at(&root.path("bases"), "Remote");
    let cloud = Arc::new(FakeCloud::with_files(files_of(&db), false));
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let app = Arc::new(App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone()),
        between_reads: None,
    });
    let bridge =
        Bridge { listeners, app: app.clone(), port, token: TOKEN.into(), link: String::new(), first_run: false };
    let background = Background::serve(bridge).unwrap();
    let snapshot_states = || background.snapshot().databases.iter().map(|d| d.state).collect::<Vec<_>>();
    assert_eq!(snapshot_states(), [State::CloudOnly]);
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 0);

    cloud.hold(true);
    let entry = app.catalog.get(&id_of(&db)).unwrap();
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    assert_eq!(snapshot_states(), [State::Downloading]);
    cloud.hold(false);
    wait_for(&entry, State::Ready);
    assert_eq!(snapshot_states(), [State::Ready]);
    assert_eq!(background.snapshot().databases[0].name, "Remote");
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
