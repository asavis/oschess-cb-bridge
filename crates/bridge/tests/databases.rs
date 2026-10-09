//! The list of databases (`docs/api.md`, "GET /v1/databases"): ChessBase's
//! window list, `bridge.toml` and the command line, read again when they
//! change, and cloud-only databases downloaded when they are opened.

use std::collections::HashSet;
use std::fs::Metadata;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bridge::access::Policy;
use bridge::api::App;
use bridge::catalog::{Catalog, Entry, State, id_of};
use bridge::config::Watched;
use bridge::fetch::Cloud;
use bridge::server;
use bridge::sources::Sources;
use cbformat::fixture::{Builder, DbItems, TempDb, quiet};
use cbformat::fixture_cbh::{self, Tok, encode, move_record};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use chesscore::Board;

mod common;
use common::{
    TOKEN, TestBridge, WAIT_LIMIT, get, has_members, has_object, members, objects, policy, poll, settle, until,
};

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
        let config = Arc::new(Watched::new(self.path("bridge.toml")));
        Sources { chessbase: Some(self.chessbase()), config: Some(config), fixed: Vec::new() }
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
    catalog.use_data_dir(&root.path("data"));

    // 2CBH entries first in the file, as ChessBase writes them; an empty
    // title shows the file name. `Old.cbh` is a classic header file without
    // the other files of its database. The PGN file opens once its index is
    // built.
    assert_eq!(names(&catalog), ["Beta (2cbh)", "Alpha", "Gone", "Games", "Delta", "Gamma", "Old", "Cli"]);
    assert_eq!(states(&catalog), ["ready", "ready", "missing", "opening", "ready", "ready", "unreadable", "ready"]);
    let entries = catalog.entries();
    assert_eq!(entries[0].id, id_of(&b));
    assert!(catalog.get(&id_of(&d)).is_some());
    assert_eq!(catalog.get(&id_of(&c)).unwrap().format.name(), "2cbh");
    assert_eq!(catalog.get(&id_of(&old)).unwrap().format.name(), "cbh");
    // The PGN file's index is built before the folder goes.
    settle(&catalog);
}

/// A database names its folder, relative to ChessBase's documents folder when
/// it is inside it and from the root otherwise, and the times of its files:
/// when its main file was created, and when any file it is read through last
/// changed, a game added to `.2cbg` alone included. A database that is gone
/// keeps its folder and has no times (#298).
#[test]
fn a_database_names_its_folder_and_the_times_of_its_files() {
    use bridge::snapshot::Database;
    let root = Root::new("folders");
    let inside = database_at(&root.chessbase().join("Bases").join("Mega2026"), "Mega");
    let top = database_at(&root.chessbase(), "Top");
    let outside = database_at(&root.path("elsewhere"), "Outside");
    let gone = root.chessbase().join("Old").join("Gone.2cbh");
    root.window(&[(&inside, ""), (&top, ""), (&outside, ""), (&gone, "")]);
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    catalog.use_data_dir(&root.path("data"));
    let of = |path: &Path| Database::of(&catalog.get(&id_of(path)).unwrap());

    assert_eq!(of(&inside).folder, ["Bases", "Mega2026"]);
    assert!(of(&top).folder.is_empty());
    let profile = bridge::documents::profile();
    let elsewhere = bridge::documents::folder_segments(&outside, Some(&root.chessbase()), profile.as_deref());
    assert_eq!(of(&outside).folder, elsewhere);
    assert_eq!(elsewhere.last().map(String::as_str), Some("elsewhere"));
    let missing = of(&gone);
    assert_eq!(
        (missing.state, missing.folder.as_slice(), missing.created, missing.modified),
        (State::Missing, &["Old".to_owned()][..], None, None)
    );

    let mega = of(&inside);
    assert_eq!(mega.state, State::Ready);
    let main = std::fs::metadata(&inside).unwrap();
    assert_eq!(mega.created, main.created().ok());
    let newest = files_of(&inside).iter().map(|f| std::fs::metadata(f).unwrap().modified().unwrap()).max();
    assert_eq!(mega.modified, newest);
    // A game added in ChessBase writes the games file, not the header file.
    touch(&inside.with_extension("2cbg"), 2_000_000_000);
    let later = of(&inside);
    let added = Some(std::time::UNIX_EPOCH + Duration::from_secs(2_000_000_000));
    assert_eq!(later.modified, added);
    assert_eq!(later.created, mega.created);
    // The name files beside it are never read: a later one changes nothing.
    for ext in ["2lgd", "2lcd"] {
        std::fs::write(inside.with_extension(ext), b"names").unwrap();
        touch(&inside.with_extension(ext), 2_000_000_300);
        assert_eq!(of(&inside).modified, added, ".{ext}");
    }
    settle(&catalog);
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

/// A new database in a configured folder, or in any folder below it (#320),
/// shows on the next listing.
#[test]
fn a_configured_folder_is_read_again_when_it_changes() {
    let root = Root::new("folder");
    database_at(&root.path("folder"), "One");
    std::fs::write(root.path("bridge.toml"), format!("databases = ['{}']\n", root.path("folder").display())).unwrap();
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    assert_eq!(names(&catalog), ["One"]);
    database_at(&root.path("folder"), "Two");
    assert_eq!(names(&catalog), ["One", "Two"]);
    // In path order: `Bases/2026/Three.2cbh` comes before `One.2cbh`.
    let three = database_at(&root.path("folder/Bases/2026"), "Three");
    assert_eq!(names(&catalog), ["Three", "One", "Two"]);
    assert_eq!(catalog.get(&id_of(&three)).unwrap().state(), State::Ready);
}

/// A database file in a subfolder that cannot be examined, here a link into a
/// folder closed to this user, is listed, missing while it cannot be reached,
/// and the other databases of its folder and below stay ready (#320). Unix
/// only.
#[cfg(unix)]
#[test]
fn a_database_that_cannot_be_examined_keeps_its_folder_listed() {
    use std::os::unix::fs::PermissionsExt;

    let root = Root::new("unexamined");
    let good = database_at(&root.path("folder/sub"), "Good");
    let deeper = database_at(&root.path("folder/sub/deeper"), "Deeper");
    let closed = root.path("closed");
    database_at(&closed, "Hidden");
    std::os::unix::fs::symlink(closed.join("Hidden.2cbh"), root.path("folder/sub/Blocked.2cbh")).unwrap();
    std::fs::write(root.path("bridge.toml"), format!("databases = ['{}']\n", root.path("folder").display())).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    let listed: Vec<(String, &str)> = catalog.entries().iter().map(|e| (e.name.clone(), e.state().name())).collect();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(listed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["Blocked", "Good", "Deeper"]);
    assert_eq!(catalog.get(&id_of(&good)).unwrap().state(), State::Ready);
    assert_eq!(catalog.get(&id_of(&deeper)).unwrap().state(), State::Ready);
    assert_eq!(listed[0].1, "missing", "while it cannot be reached");
}

/// A database added to a configured folder shows even when the folder's own
/// size and time did not change, as happens within one tick of the kernel's
/// coarse clock: the listing, not only the folder's time, decides, for a PGN
/// file as for a ChessBase database. Unix only: resetting a folder's time
/// needs a directory handle with timestamp-write access on Windows, which
/// `File::open` does not give.
#[cfg(unix)]
#[test]
fn a_database_added_within_one_clock_tick_shows() {
    let root = Root::new("folder-tick");
    database_at(&root.path("folder"), "One");
    std::fs::write(root.path("bridge.toml"), format!("databases = ['{}']\n", root.path("folder").display())).unwrap();
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    catalog.use_data_dir(&root.path("data"));
    assert_eq!(names(&catalog), ["One"]);
    let folder = std::fs::File::open(root.path("folder")).unwrap();
    let before = folder.metadata().unwrap().modified().unwrap();
    database_at(&root.path("folder"), "Two");
    folder.set_modified(before).unwrap();
    assert_eq!(folder.metadata().unwrap().modified().unwrap(), before);
    assert_eq!(names(&catalog), ["One", "Two"]);

    std::fs::write(root.path("folder/Three.pgn"), "[Event \"?\"]\n\n*\n").unwrap();
    folder.set_modified(before).unwrap();
    assert_eq!(folder.metadata().unwrap().modified().unwrap(), before);
    assert_eq!(names(&catalog), ["One", "Three", "Two"]);
    // The PGN file's index is built before the folder goes.
    settle(&catalog);
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
    /// The files whose marks were looked at, one each time.
    looks: AtomicUsize,
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
        self.looks.fetch_add(1, Ordering::SeqCst);
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

/// The files of a classic database that the bridge reads and that the
/// builder writes: all but `.cbj`.
const CLASSIC: [&str; 7] = ["cbh", "cbg", "cba", "cbp", "cbt", "cbc", "cbs"];

/// A one-game classic database written under `dir` as `stem.cbh` and its
/// companions.
fn classic_at(dir: &Path, stem: &str) -> PathBuf {
    let mut b = fixture_cbh::Builder::new();
    b.game(&move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false)));
    let unique = dir.to_string_lossy().replace(['/', '\\', ':'], "-");
    let db = b.write(&format!("databases{unique}-{stem}"));
    std::fs::create_dir_all(dir).unwrap();
    for ext in CLASSIC {
        std::fs::copy(db.dir().join(format!("db.{ext}")), dir.join(format!("{stem}.{ext}"))).unwrap();
    }
    dir.join(format!("{stem}.cbh"))
}

/// Sets the modification time of `path`, as a program saving it would.
fn touch(path: &Path, seconds: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds)).unwrap();
}

/// A classic database follows the files it reads: one of them kept in the
/// cloud makes it cloud-only, and downloading reads only that one; a change
/// to any of them, or a `.cbj` added, is a new generation. A file it never
/// reads, such as a search booster, counts for neither.
#[test]
fn a_classic_database_follows_the_files_it_reads() {
    let root = Root::new("classic");
    let db = classic_at(&root.path("bases"), "Old");
    std::fs::write(db.with_extension("cbtt"), b"booster").unwrap();
    let cloud = Arc::new(FakeCloud::with_files([db.with_extension("cbc"), db.with_extension("cbtt")], false));
    root.window(&[(&db, "")]);
    let catalog = Catalog::with_sources(root.sources(), cloud.clone());
    let entry = catalog.get(&id_of(&db)).unwrap();
    assert_eq!(entry.format.name(), "cbh");
    assert_eq!(states(&catalog), ["cloudOnly"]);
    let files: Vec<PathBuf> = CLASSIC.iter().map(|ext| db.with_extension(ext)).collect();
    assert_eq!(entry.open_looked().1.map(|l| l.size), Some(size_of(&files)));
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    wait_for(&entry, State::Ready);
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 1, "the annotators' file only");
    assert_eq!(entry.open().unwrap().db.record_count(), 1);

    let generation = entry.generation().unwrap();
    touch(&db.with_extension("cbtt"), 1_000_000_000);
    assert_eq!(entry.generation(), Some(generation), "a booster changed");
    touch(&db.with_extension("cbc"), 1_000_000_000);
    let changed = entry.generation().unwrap();
    assert_ne!(changed, generation, "an entity file changed");
    std::fs::write(db.with_extension("cbj"), [0u8; 32]).unwrap();
    assert_ne!(entry.generation(), Some(changed), "a .cbj added");
    let open = entry.open().unwrap();
    assert_eq!((open.generation, open.db.record_count()), (entry.generation().unwrap(), 1));
}

fn files_of(db: &Path) -> Vec<PathBuf> {
    ["2cbh", "2cbg", "2lid"].iter().map(|ext| db.with_extension(ext)).collect()
}

fn size_of(files: &[PathBuf]) -> u64 {
    files.iter().map(|f| std::fs::metadata(f).unwrap().len()).sum()
}

fn wait_for(entry: &Entry, state: State) {
    let reached = poll(WAIT_LIMIT, || (entry.state() == state).then_some(()));
    assert!(reached.is_some(), "still {:?} after {WAIT_LIMIT:?}, waiting for {state:?}", entry.state());
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
    assert_eq!(entry.open_looked().1.map(|l| l.size), Some(size_of(&files)));
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
    until("the download does not end", WAIT_LIMIT, || entry.progress().is_none());
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
/// until the next request for its games. A download starts running or queued,
/// shown by its progress, so none starting is seen at once.
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
    assert_eq!(states(&catalog), ["cloudOnly"]);
    assert!(entry.progress().is_none(), "a kept mark started a download by itself");
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

/// A request for one database answers while the list's sources are read,
/// however long that takes, from the list as last rebuilt: a configured folder
/// on a network drive that stops answering holds up the listings, never the
/// databases' own requests. The rebuilt list replaces it once the read ends.
#[test]
fn a_database_answers_while_the_sources_are_read() {
    use std::sync::mpsc;

    let root = Root::new("slow-source");
    let folder = root.path("folder");
    let one = database_at(&folder, "One");
    std::fs::write(root.path("bridge.toml"), format!("databases = ['{}']\n", folder.display())).unwrap();
    let catalog = Arc::new(Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System)));
    assert_eq!(names(&catalog), ["One"]);

    // The next read of the sources is held until `release` is dropped.
    let (reading, read) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    catalog.after_read(move || {
        let _ = reading.send(());
        let _ = released.lock().unwrap().recv();
    });
    let two = database_at(&folder, "Two");
    let listing = {
        let catalog = Arc::clone(&catalog);
        std::thread::spawn(move || names(&catalog))
    };
    read.recv_timeout(WAIT_LIMIT).expect("the sources were not read");

    let (tx, rx) = mpsc::channel();
    let (reader, ids) = (Arc::clone(&catalog), [id_of(&one), id_of(&two)]);
    std::thread::spawn(move || {
        let _ = tx.send(ids.map(|id| reader.get(&id).map(|e| e.state())));
    });
    let answers = rx.recv_timeout(WAIT_LIMIT).expect("a request waited for the sources");
    assert_eq!(answers, [Some(State::Ready), None]);
    drop(release);
    assert_eq!(listing.join().unwrap(), ["One", "Two"]);
    assert_eq!(catalog.get(&id_of(&two)).map(|e| e.state()), Some(State::Ready));
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
        let pipe = Sources { config: Some(Arc::new(Watched::new(pipe_config.clone()))), ..Sources::default() };
        let config = pipe.configured().is_err();
        let startup = bridge::config::load_or_create(&pipe_config).is_err();
        tx.send((listed, games, ready, config, startup)).unwrap();
    });
    let (listed, games, ready, config, startup) = rx.recv_timeout(WAIT_LIMIT).expect("a pipe blocked the bridge");
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

/// The contract's rows and answers for a cloud-only database.
#[test]
fn cloud_states_over_http() {
    let root = Root::new("cloud-http");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    let size = size_of(&files);
    let cloud = Arc::new(FakeCloud::with_files(files.clone(), false));
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let bridge = TestBridge::new(App::new("test", policy(), catalog));
    let (port, id) = (bridge.port, id_of(&db));

    let (status, body) = get(port, "/v1/databases");
    assert_eq!(status, 200);
    assert!(has_object(&body, &format!("\"state\":\"cloudOnly\",\"size\":{size}")), "{body}");
    let (_, body) = get(port, "/v1/status");
    assert!(has_object(&body, "\"cloudOnly\":1,\"downloading\":0"), "{body}");
    assert!(!body.contains("\"download\""), "{body}");

    cloud.hold(true);
    let (status, body) = get(port, &format!("/v1/databases/{id}/games"));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("\"code\":\"database_unavailable\"") && body.contains("\"state\":\"downloading\""), "{body}");
    let (status, body) = get(port, &format!("/v1/databases/{id}/games/1"));
    assert_eq!(status, 409, "{body}");
    let (_, body) = get(port, "/v1/databases");
    assert!(
        has_object(
            &body,
            &format!("\"state\":\"downloading\",\"size\":{size},\"progress\":{{\"present\":0,\"total\":{size}}}")
        ),
        "{body}"
    );
    let (_, body) = get(port, "/v1/status");
    assert!(has_object(&body, "\"cloudOnly\":0,\"downloading\":1"), "{body}");
    assert!(has_object(&body, &format!("\"download\":{{\"present\":0,\"total\":{size}}}")), "{body}");

    cloud.hold(false);
    let mut last = (0, String::new());
    let body = poll(WAIT_LIMIT, || {
        last = get(port, &format!("/v1/databases/{id}/games"));
        (last.0 == 200).then(|| last.1.clone())
    });
    let body = body.unwrap_or_else(|| panic!("still {} {} after {WAIT_LIMIT:?}", last.0, last.1));
    assert!(body.contains("\"total\":1"), "{body}");
    let (_, body) = get(port, "/v1/databases");
    assert!(has_object(&body, "\"state\":\"ready\",\"records\":1"), "{body}");
}

/// A request for the games of a cloud-only database is checked whole before
/// the database is opened: one refused for any parameter, `stream`, `fen`
/// and a `q` with a Library-only qualifier among them, starts no download
/// (#173). A valid one then does.
#[test]
fn a_refused_request_starts_no_download() {
    let root = Root::new("cloud-refused");
    let db = database_at(&root.path("bases"), "Remote");
    let cloud = Arc::new(FakeCloud::with_files(files_of(&db), false));
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let (id, entry) = (id_of(&db), catalog.get(&id_of(&db)).unwrap());
    let bridge = TestBridge::new(App::new("test", policy(), catalog));
    let port = bridge.port;
    // A download started now would wait, and show.
    cloud.hold(true);
    let start = "rnbqkbnr%2Fpppppppp%2F8%2F8%2F8%2F8%2FPPPPPPPP%2FRNBQKBNR+w+KQkq+-+0+1";
    let position_in_a_bad_stream = format!("fen={start}&stream=bad!");
    for (query, parameter) in [
        ("stream=bad!", "stream"),
        ("stream=", "stream"),
        ("fen=nonsense", "fen"),
        (position_in_a_bad_stream.as_str(), "stream"),
        ("limit=0", "limit"),
    ] {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games?{query}"));
        assert_eq!(status, 400, "{query}: {body}");
        assert!(body.contains(&format!("\"parameter\":\"{parameter}\"")), "{query}: {body}");
        assert!(entry.progress().is_none(), "{query} started a download");
        assert_eq!(entry.state(), State::CloudOnly, "{query}");
    }
    // Alone and with a valid position alike.
    for qualifier in ["tag", "created", "updated", "is", "has", "no"] {
        for query in [format!("q={qualifier}%3Ax"), format!("fen={start}&stream=tab-1&q={qualifier}%3Ax")] {
            let (status, body) = get(port, &format!("/v1/databases/{id}/games?{query}"));
            assert_eq!(status, 400, "{query}: {body}");
            let refusal = format!(
                "\"code\":\"unsupported_qualifier\",\"message\":\"ChessBase databases do not have this qualifier\",\"qualifier\":\"{qualifier}\""
            );
            assert!(has_object(&body, &refusal), "{query}: {body}");
            assert!(entry.progress().is_none(), "{query} started a download");
            assert_eq!(entry.state(), State::CloudOnly, "{query}");
        }
    }
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 0);
    let (status, body) = get(port, &format!("/v1/databases/{id}/games?stream=tab-1"));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("\"code\":\"database_unavailable\"") && body.contains("\"state\":\"downloading\""), "{body}");
    cloud.hold(false);
    wait_for(&entry, State::Ready);
}

/// The snapshot a user interface polls shows a cloud database as the list
/// does, without reading it: cloud-only, downloading once its games were
/// asked for, then ready. The list's row and the snapshot are one view (#67),
/// made from one look at the database's files.
#[test]
fn the_snapshot_shows_cloud_states() {
    use bridge::snapshot::{Background, Database};
    use bridge::start::Bridge;

    let root = Root::new("snapshot");
    let db = database_at(&root.path("bases"), "Remote");
    let files = files_of(&db);
    let size = size_of(&files);
    let cloud = Arc::new(FakeCloud::with_files(files.clone(), false));
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let catalog = Catalog::with_sources(Sources { fixed: vec![db.clone()], ..Sources::default() }, cloud.clone());
    let app = Arc::new(App::new("test", Policy { port, ..policy() }, catalog));
    let bridge =
        Bridge { listeners, app: app.clone(), port, token: TOKEN.into(), link: String::new(), first_run: false };
    let background = Background::serve(bridge).unwrap();
    // The members of an object past a database's id, name and format.
    let keys = |object: &str| -> Vec<String> {
        let mut keys: Vec<String> = members(object).into_iter().map(|(k, _)| k.to_string()).collect();
        keys.retain(|k| !["id", "name", "format"].contains(&k.as_str()));
        keys.sort();
        keys
    };
    // The database in one snapshot, which looks at each of its files once, and
    // the list's row for it, which says the same: past its id, name and
    // format, the row has the snapshot's members, and no other.
    let snapshot = || {
        let before = cloud.looks.load(Ordering::SeqCst);
        let database = background.snapshot().databases[0].clone();
        assert_eq!(cloud.looks.load(Ordering::SeqCst) - before, files.len(), "{:?}", database.state);
        let (_, body) = get(port, "/v1/databases");
        let (listed, want) = (objects(&body, "databases")[0], row(&database));
        assert!(has_members(listed, &want) && keys(listed) == keys(&format!("{{{want}}}")), "{want} in {body}");
        database
    };
    let fields = |d: &Database| (d.state, d.records, d.size, d.progress);
    assert_eq!(fields(&snapshot()), (State::CloudOnly, None, Some(size), None));
    assert_eq!(cloud.fetches.load(Ordering::SeqCst), 0);

    cloud.hold(true);
    let entry = app.catalog.get(&id_of(&db)).unwrap();
    assert!(matches!(entry.open_to_read(), Err(State::Downloading)));
    assert_eq!(fields(&snapshot()), (State::Downloading, None, Some(size), Some((0, size))));
    cloud.hold(false);
    wait_for(&entry, State::Ready);
    let ready = snapshot();
    assert_eq!(fields(&ready), (State::Ready, Some(1), None, None));
    assert_eq!((ready.name.as_str(), ready.generation), ("Remote", entry.generation()));
    // Served as the app serves it, with its keeper: stopped, and its work
    // waited for, before the folder goes.
    settle(&app.catalog);
}

/// The members of a database's row in `GET /v1/databases` past its id, name
/// and format, as the contract writes them.
fn row(d: &bridge::snapshot::Database) -> String {
    let mut row = format!("\"state\":\"{}\",\"writable\":{}", d.state.name(), d.writable);
    if let Some(records) = d.records {
        row += &format!(",\"records\":{records}");
    }
    if let Some(generation) = d.generation {
        row += &format!(",\"generation\":\"{generation:016x}\"");
    }
    if let Some(size) = d.size {
        row += &format!(",\"size\":{size}");
    }
    if let Some((present, total)) = d.progress {
        row += &format!(",\"progress\":{{\"present\":{present},\"total\":{total}}}");
    }
    let folder: Vec<String> = d.folder.iter().map(|s| format!("\"{s}\"")).collect();
    row += &format!(",\"folder\":[{}]", folder.join(","));
    for (key, time) in [("created", d.created), ("modified", d.modified)] {
        if let Some(time) = time.and_then(bridge::log::rfc3339) {
            row += &format!(",\"{key}\":\"{time}\"");
        }
    }
    row
}

/// The binary finds the window list through `OSCHESS_BRIDGE_DOCUMENTS`.
#[test]
fn the_bridge_reads_the_window_of_the_documents_folder() {
    let root = Root::new("binary");
    let a = database_at(&root.path("bases"), "Alpha");
    root.window(&[(&a, "Windowed")]);
    let home = root.path("home");
    std::fs::create_dir_all(&home).unwrap();
    // A port found free may be taken by another test's bridge before this one
    // binds it; that bridge then ends at once, and starts again on another
    // port (#63).
    let mut text = String::new();
    for _ in 0..10 {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oschess-bridge"))
            .env("OSCHESS_BRIDGE_HOME", &home)
            .env("OSCHESS_BRIDGE_DOCUMENTS", &root.0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut out = child.stdout.take().unwrap();
        text.clear();
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + WAIT_LIMIT;
        while !text.contains("Windowed") && Instant::now() < deadline {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        if !text.is_empty() {
            break;
        }
    }
    assert!(text.contains(&format!("{} [ready] Windowed", id_of(&a))), "{text}");
}

/// A listed database, once listed and read, holds none of its files open
/// while nothing reads it: ChessBase, which opens a database's files so that
/// no other handle may exist when it saves a game, can then save (#241). The
/// test folder is on NTFS, whose files have a lasting identity.
#[cfg(windows)]
#[test]
fn a_database_holds_no_file_open_while_nothing_reads_it() {
    use common::{WAIT_LIMIT, answered, app_of, classic_fixture, fixture, index_dir, until};
    let dbs = [
        fixture("closed-2cbh", &[]),
        classic_fixture("closed-cbh", &[]),
        cbformat::fixture::pgn_file("closed-pgn", b"[Event \"E\"]\n\n1. e4 e5 *\n"),
    ];
    let paths = [dbs[0].dir().join("db.2cbh"), dbs[1].dir().join("db.cbh"), dbs[2].dir().join("db.pgn")];
    let dir = index_dir("closed");
    let bridge = TestBridge::in_dir(app_of(paths.clone()), &dir);
    let port = bridge.port;
    // The list opens every database, the games read each of them.
    let (_, body) = get(port, "/v1/databases");
    assert!(body.contains(r#""state":"ready""#), "{body}");
    for path in &paths {
        answered(port, &format!("/v1/databases/{}/games", id_of(path)));
    }
    let files: Vec<PathBuf> =
        dbs.iter().flat_map(|db| std::fs::read_dir(db.dir()).unwrap()).map(|entry| entry.unwrap().path()).collect();
    assert!(files.len() >= 10, "{files:?}");
    until("no database file is open", WAIT_LIMIT, || files.iter().all(|file| opens_alone(file)));
    drop(bridge);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A database file replaced by a copy of the same bytes, size and time of
/// change is read again at the next request: its generation changes with the
/// file, so the catalog opens the database again rather than keep one whose
/// file is refused as replaced, and answer `503 database_changing` for good
/// (#241). On Windows the copy is made once the bridge holds none of the
/// files, which is when a file is opened again at its path.
#[test]
fn a_file_replaced_by_a_copy_is_read_again() {
    use common::{answered, app_of, fixture, index_dir, without_generation};
    let db = fixture("replaced-copy", &[]);
    let path = db.dir().join("db.2cbh");
    let dir = index_dir("replaced-copy");
    let bridge = TestBridge::in_dir(app_of([path.clone()]), &dir);
    let port = bridge.port;
    let games = format!("/v1/databases/{}/games", id_of(&path));
    let first = answered(port, &games);
    #[cfg(windows)]
    common::until("the header file is let go", common::WAIT_LIMIT, || opens_alone(&path));
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let moved = path.with_extension("moved");
    std::fs::rename(&path, &moved).unwrap();
    std::fs::write(&path, std::fs::read(&moved).unwrap()).unwrap();
    std::fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
    let again = answered(port, &games);
    assert_ne!(first, again, "the generation is new");
    assert_eq!(without_generation(&first), without_generation(&again));
    drop(bridge);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A PGN file that leaves the list, has its header index swept, and comes
/// back is read again. The index is the bridge's own, and the database opened
/// on it keeps it open: were it opened again at its path, the database the
/// catalog keeps for the unchanged file would find it gone and answer
/// `503 database_changing` for good (#241).
#[test]
fn a_pgn_file_back_on_the_list_after_its_index_was_swept_is_read() {
    use common::{answered, index_dir};
    let root = Root::new("pgn-back");
    std::fs::create_dir_all(root.path("bases")).unwrap();
    let pgn = root.path("bases/Games.pgn");
    std::fs::write(&pgn, "[Event \"E\"]\n[White \"W\"]\n[Black \"B\"]\n\n1. e4 e5 *\n").unwrap();
    let listed = format!("databases = ['{}']\n", pgn.display());
    std::fs::write(root.path("bridge.toml"), &listed).unwrap();
    let catalog = Catalog::with_sources(root.sources(), Arc::new(bridge::fetch::System));
    let dir = index_dir("pgn-back");
    let bridge = TestBridge::in_dir(App::new("test", policy(), catalog), &dir);
    let (port, app) = (bridge.port, &bridge.app);
    let id = id_of(&pgn);
    let games = format!("/v1/databases/{id}/games");
    let first = answered(port, &games);
    let index = bridge::folders::pgn_dir(&dir).join(format!("{id}.head"));
    assert!(index.exists());
    // On Windows, the PGN file is let go once idle, and so would the index
    // be, were it closed between reads, by now or a little later.
    #[cfg(windows)]
    {
        common::until("the PGN file is let go", common::WAIT_LIMIT, || opens_alone(&pgn));
        std::thread::sleep(cbformat::file::IDLE * 3);
    }

    // Off the list, its index swept at once.
    std::fs::write(root.path("bridge.toml"), "databases = []\n").unwrap();
    assert!(app.catalog.entries().iter().all(|e| !e.listed()), "the file left the list");
    app.catalog.set_sweep_grace(Duration::ZERO);
    app.catalog.sweep_indexes();
    assert!(!index.exists(), "the index was swept");

    // Back on the list, unchanged: read as before.
    std::fs::write(root.path("bridge.toml"), &listed).unwrap();
    assert!(app.catalog.entries().iter().any(|e| e.id == id && e.listed()), "the file is back");
    assert_eq!(answered(port, &games), first);
    drop(bridge);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether `file` opens for a writer that shares it with nobody, as ChessBase
/// opens a database to save a game: whether no handle holds it.
#[cfg(windows)]
fn opens_alone(file: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(file).is_ok()
}
