//! The bridge's state at one moment, for a user interface to poll: whether it
//! still serves, why it stopped, and the state of each database.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::api::App;
use crate::catalog::{Entry, State};
use crate::server;
use crate::start::Bridge;
use crate::sync::lock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub version: &'static str,
    pub port: u16,
    /// `None` while the bridge serves; why it stopped otherwise.
    pub stopped: Option<String>,
    pub databases: Vec<Database>,
    /// The work a restart would lose now (#61), each kind once.
    pub work: Vec<Work>,
    /// A paired browser has been served since the bridge started.
    pub served: bool,
}

/// Work a restart of the bridge would lose, which an update waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Work {
    /// A database brought from the cloud.
    Downloading,
    /// A database opened for the first time: a PGN file's header index.
    Opening,
    /// A position index built.
    Indexing,
    /// An analysis streamed to an analysis board, for [`ANALYSIS_HOLDS_UPDATES`].
    Analysing,
}

/// The most threads that look at the databases of a list at once (#348).
pub const LOOKERS: usize = 8;

/// How long a running analysis holds an update back: one left running longer,
/// as in a tab left open, does not keep the bridge from updating for good.
pub const ANALYSIS_HOLDS_UPDATES: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Database {
    pub id: String,
    /// The name ChessBase's window shows: its title for the database, else
    /// the file name without its extension.
    pub name: String,
    /// `2cbh`, `cbh`, `pgn` or `other`, as the API names it.
    pub format: &'static str,
    /// Checked without reading a file kept in the cloud: such a database is
    /// `CloudOnly`, or `Downloading` once its games were asked for.
    pub state: State,
    /// Games, guiding texts and analyses, when the database is ready.
    pub records: Option<u32>,
    /// The generation it was opened at, when it is ready.
    pub generation: Option<u64>,
    /// The bytes of its files, while they are kept in the cloud or downloaded:
    /// while it downloads, the bytes the download brings in all.
    pub size: Option<u64>,
    /// The bytes on this computer and in all, while it downloads; the bytes
    /// of a PGN file read and in all, while it opens.
    pub progress: Option<(u64, u64)>,
    /// Whether the database is on the list: one that left it is `missing`.
    pub listed: bool,
    /// Whether it takes writes now: a ready PGN file without the read-only
    /// attribute (`docs/api.md`, "Writing games").
    pub writable: bool,
    /// The folder holding it, as path segments
    /// ([`crate::documents::folder_segments`]).
    pub folder: Vec<String>,
    /// When its main file was created, from its metadata: `None` when it is
    /// missing, or when the file system keeps no such time.
    pub created: Option<SystemTime>,
    /// When the file it is read through that changed last changed, from
    /// their metadata: `None` when it is missing.
    pub modified: Option<SystemTime>,
}

impl Database {
    /// The database as `GET /v1/databases`, `/v1/status` and the tray app all
    /// show it (#67), from one look at its files: a ready one is opened once.
    pub fn of(entry: &Entry) -> Database {
        let (open, look) = entry.open_looked();
        let (state, records, generation) = match open {
            Ok(open) => (State::Ready, Some(open.db.record_count()), Some(open.generation)),
            Err(state) => (state, None, None),
        };
        let progress = match state {
            State::Downloading => entry.progress(),
            State::Opening => entry.opening(),
            _ => None,
        }
        .map(|p| (p.present(), p.total));
        let size = match (state, progress) {
            (State::Downloading, Some((_, total))) => Some(total),
            (State::CloudOnly | State::Downloading, _) => look.map(|l| l.size),
            _ => None,
        };
        Database {
            id: entry.id.clone(),
            name: entry.name.clone(),
            format: entry.format.name(),
            state,
            records,
            generation,
            size,
            progress,
            listed: entry.listed(),
            writable: state == State::Ready && entry.writable_file(),
            folder: entry.folder(),
            created: look.and_then(|l| l.created),
            modified: look.and_then(|l| l.modified),
        }
    }

    /// Every database of `entries`, in their order, each as [`Database::of`]
    /// shows it, from a look at each on up to [`LOOKERS`] threads, the calling
    /// one among them, each taking the next entry none has taken (#348): a
    /// list of hundreds of databases, and the first open of each one on it,
    /// take a part of the time one thread takes. A thread that cannot start
    /// leaves the entries to the others. The threads run at the caller's
    /// priority.
    pub fn all(entries: &[Arc<Entry>]) -> Vec<Database> {
        let next = AtomicUsize::new(0);
        let priority = crate::machine::current();
        let look = || {
            crate::machine::follow(priority);
            let mut seen = Vec::new();
            loop {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(entry) = entries.get(at) else { return seen };
                seen.push((at, Database::of(entry)));
            }
        };
        let mut seen: Vec<(usize, Database)> = std::thread::scope(|s| {
            let helpers: Vec<_> = (1..LOOKERS.min(entries.len()))
                .filter_map(|_| {
                    let thread = std::thread::Builder::new().name("bridge-look".into());
                    thread.stack_size(crate::THREAD_STACK).spawn_scoped(s, look).ok()
                })
                .collect();
            let mut seen = look();
            for helper in helpers {
                seen.extend(helper.join().unwrap_or_else(|p| std::panic::resume_unwind(p)));
            }
            seen
        });
        seen.sort_unstable_by_key(|(at, _)| *at);
        seen.into_iter().map(|(_, database)| database).collect()
    }
}

/// The work a restart would lose: the databases downloading or opening, a
/// header index built (`headers`), a position index built (`indexing`) and
/// an analysis running (`analysing`), each kind once and in order.
fn work_of(databases: &[Database], headers: bool, indexing: bool, analysing: bool) -> Vec<Work> {
    let mut work: Vec<Work> = databases
        .iter()
        .filter_map(|d| match d.state {
            State::Downloading => Some(Work::Downloading),
            State::Opening => Some(Work::Opening),
            _ => None,
        })
        .collect();
    work.extend(headers.then_some(Work::Opening));
    work.extend(indexing.then_some(Work::Indexing));
    work.extend(analysing.then_some(Work::Analysing));
    work.sort();
    work.dedup();
    work
}

/// A bridge serving on a thread of its own.
pub struct Background {
    app: Arc<App>,
    port: u16,
    stopped: Arc<Mutex<Option<String>>>,
}

impl Background {
    /// Serves `bridge` on a new thread, and keeps the position indexes of its
    /// databases in use ([`crate::explorer::keeper`]). Take what else is
    /// needed from `bridge`, such as the pairing link, before handing it over.
    pub fn serve(bridge: Bridge) -> std::io::Result<Background> {
        let Bridge { listeners, app, port, .. } = bridge;
        // A serving bridge keeps the indexes of the databases in use (#149).
        crate::explorer::keeper::start(&app);
        let stopped = Arc::new(Mutex::new(None));
        let (served, record) = (app.clone(), stopped.clone());
        std::thread::Builder::new().name("bridge-server".into()).stack_size(crate::THREAD_STACK).spawn(move || {
            let reason = match server::serve(listeners, served) {
                Ok(()) => "the server stopped".to_string(),
                Err(e) => e.to_string(),
            };
            *lock(&record) = Some(reason);
        })?;
        Ok(Background { app, port, stopped })
    }

    /// The state now. Checking a database opens it when it is ready, so call
    /// this from a thread that may wait on the disk.
    pub fn snapshot(&self) -> Snapshot {
        let databases = Database::all(&self.app.catalog.entries());
        let catalog = &self.app.catalog;
        let work = work_of(
            &databases,
            catalog.pgn().building(),
            // A build that waits for its turn loses nothing.
            catalog.explorer.building().iter().any(|b| b.1 != "waiting"),
            self.app.engine.analyzing(ANALYSIS_HOLDS_UPDATES),
        );
        Snapshot {
            version: self.app.version,
            port: self.port,
            stopped: lock(&self.stopped).clone(),
            databases,
            work,
            served: self.app.served.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;

    use super::*;
    use crate::access::{DEFAULT_ORIGINS, Policy};
    use crate::catalog::Catalog;
    use cbformat::fixture::{Builder, TempDb, lid_header, quiet};
    use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

    fn bridge(databases: Vec<PathBuf>) -> Bridge {
        let listeners = server::bind(0).unwrap();
        let port = listeners[0].local_addr().unwrap().port();
        let policy = Policy { port, origins: DEFAULT_ORIGINS.map(String::from).to_vec(), token: TOKEN.into() };
        let app = App::new("test", policy, Catalog::new(databases));
        Bridge { listeners, app: Arc::new(app), port, token: TOKEN.into(), link: String::new(), first_run: false }
    }

    /// A database of `games` games of 1.e4.
    fn ready(name: &str, games: u32) -> TempDb {
        let mut b = Builder::new();
        let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
        for _ in 0..games {
            b.game(e4);
        }
        let player = [&6i32.to_le_bytes()[..], b"Morphy"].concat();
        let mut lid = lid_header(1024, 1);
        lid.extend((player.len() as i32).to_le_bytes());
        lid.extend(&player);
        b.lid(lid);
        b.write(name)
    }

    #[test]
    fn a_serving_bridge_and_its_databases() {
        let missing = PathBuf::from("/no/such/folder/Games.2cbh");
        let db = ready("snapshot-ready", 3);
        let bridge = bridge(vec![missing.clone(), PathBuf::from("/no/such/folder/Old.pgn"), db.dir().join("db.2cbh")]);
        let port = bridge.port;
        let background = Background::serve(bridge).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request = format!(
            "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

        let snapshot = background.snapshot();
        assert_eq!((snapshot.version, snapshot.port, snapshot.stopped.as_deref()), ("test", port, None));
        let states: Vec<(&str, &str, State, Option<u32>)> =
            snapshot.databases.iter().map(|d| (d.name.as_str(), d.format, d.state, d.records)).collect();
        assert_eq!(
            states,
            [
                ("Games", "2cbh", State::Missing, None),
                ("Old", "pgn", State::Missing, None),
                ("db", "2cbh", State::Ready, Some(3))
            ]
        );
        assert_eq!(snapshot.databases[0].id, crate::catalog::id_of(&missing));
        assert!(snapshot.databases.iter().all(|d| d.listed));
    }

    /// The databases of a list, looked at on several threads and opened
    /// there the first time, are those one thread sees, in the list's order
    /// (#348): ready, missing and of every format, more of them than threads.
    #[test]
    fn a_list_looked_at_on_threads_is_the_one_thread_sees() {
        let dbs: Vec<TempDb> = (0..12).map(|i| ready(&format!("snapshot-all-{i}"), i % 4 + 1)).collect();
        let mut paths: Vec<PathBuf> = dbs.iter().map(|db| db.dir().join("db.2cbh")).collect();
        for (i, missing) in
            ["/no/such/folder/A.2cbh", "/no/such/folder/B.pgn", "/no/such/folder/C.cbh"].iter().enumerate()
        {
            paths.insert(i * 5, PathBuf::from(missing));
        }
        let catalog = Catalog::new(paths.clone());
        let entries = catalog.entries();
        assert_eq!(entries.len(), 15);
        assert!(entries.len() > LOOKERS);
        let threads = Database::all(&entries);
        let one: Vec<Database> = entries.iter().map(|e| Database::of(e)).collect();
        assert_eq!(threads, one);
        let ids: Vec<String> = paths.iter().map(|p| crate::catalog::id_of(p)).collect();
        assert_eq!(threads.iter().map(|d| d.id.clone()).collect::<Vec<_>>(), ids, "in the list's order");
        let ready = threads.iter().filter(|d| d.state == State::Ready).count();
        assert_eq!((ready, threads.len() - ready), (12, 3));
        assert_eq!(Database::all(&[]), []);
    }

    /// The status line of a `GET /v1/status` sent with `token`.
    fn status_with(port: u16, token: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request = format!(
            "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer.lines().next().unwrap_or_default().to_string()
    }

    #[test]
    fn a_browser_counts_as_served_only_once_it_passes_the_policy() {
        let bridge = bridge(Vec::new());
        let port = bridge.port;
        let background = Background::serve(bridge).unwrap();
        assert!(!background.snapshot().served);
        assert!(status_with(port, "not-the-token").starts_with("HTTP/1.1 401"));
        assert!(!background.snapshot().served, "a refused request is no paired browser");
        assert!(status_with(port, TOKEN).starts_with("HTTP/1.1 200"));
        assert!(background.snapshot().served);
    }

    #[test]
    fn a_stopped_server_says_why() {
        let bridge = bridge(Vec::new());
        let background =
            Background { app: bridge.app, port: bridge.port, stopped: Arc::new(Mutex::new(Some("port lost".into()))) };
        let snapshot = background.snapshot();
        assert_eq!(snapshot.stopped.as_deref(), Some("port lost"));
        assert!(snapshot.databases.is_empty());
    }

    /// The work an update waits for (#61): each kind once, whatever named it.
    #[test]
    fn work_names_each_kind_once() {
        let db = |state| Database {
            id: "0123456789abcdef".into(),
            name: "Base".into(),
            format: "2cbh",
            state,
            records: None,
            generation: None,
            size: None,
            progress: None,
            listed: true,
            writable: false,
            folder: Vec::new(),
            created: None,
            modified: None,
        };
        assert_eq!(work_of(&[db(State::Ready), db(State::Missing)], false, false, false), []);
        assert_eq!(
            work_of(&[db(State::Opening), db(State::Downloading), db(State::Downloading)], true, false, false),
            [Work::Downloading, Work::Opening]
        );
        assert_eq!(work_of(&[], false, true, true), [Work::Indexing, Work::Analysing]);
    }
}
