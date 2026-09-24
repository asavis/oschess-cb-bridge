//! The bridge's state at one moment, for a user interface to poll: whether it
//! still serves, why it stopped, and the state of each database.

use std::sync::{Arc, Mutex};

use crate::api::App;
use crate::catalog::{Entry, State};
use crate::server;
use crate::start::Bridge;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub version: &'static str,
    pub port: u16,
    /// `None` while the bridge serves; why it stopped otherwise.
    pub stopped: Option<String>,
    pub databases: Vec<Database>,
}

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
    /// The bytes of its files, while they are kept in the cloud or downloaded.
    pub size: Option<u64>,
    /// The bytes on this computer and in all, while it downloads.
    pub progress: Option<(u64, u64)>,
    /// Whether the database is on the list: one that left it is `missing`.
    pub listed: bool,
}

impl Database {
    fn of(entry: &Entry) -> Database {
        let state = entry.state();
        let records = if state == State::Ready { entry.open().ok().map(|o| o.db.record_count()) } else { None };
        let size = matches!(state, State::CloudOnly | State::Downloading).then(|| entry.size());
        let progress = entry.progress().filter(|_| state == State::Downloading).map(|p| (p.present(), p.total));
        Database {
            id: entry.id.clone(),
            name: entry.name.clone(),
            format: entry.format.name(),
            state,
            records,
            size,
            progress,
            listed: entry.listed(),
        }
    }
}

/// A bridge serving on a thread of its own.
pub struct Background {
    app: Arc<App>,
    port: u16,
    stopped: Arc<Mutex<Option<String>>>,
}

impl Background {
    /// Serves `bridge` on a new thread. Take what else is needed from `bridge`,
    /// such as the pairing link, before handing it over.
    pub fn serve(bridge: Bridge) -> std::io::Result<Background> {
        let Bridge { listeners, app, port, .. } = bridge;
        let stopped = Arc::new(Mutex::new(None));
        let (served, record) = (app.clone(), stopped.clone());
        std::thread::Builder::new().name("bridge-server".into()).stack_size(crate::THREAD_STACK).spawn(move || {
            let reason = match server::serve(listeners, served) {
                Ok(()) => "the server stopped".to_string(),
                Err(e) => e.to_string(),
            };
            *record.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
        })?;
        Ok(Background { app, port, stopped })
    }

    /// The state now. Checking a database opens it when it is ready, so call
    /// this from a thread that may wait on the disk.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            version: self.app.version,
            port: self.port,
            stopped: self.stopped.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            databases: self.app.catalog.entries().iter().map(|e| Database::of(e)).collect(),
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
        let app = App {
            version: "test",
            policy: Policy { port, origins: DEFAULT_ORIGINS.map(String::from).to_vec(), token: TOKEN.into() },
            catalog: Catalog::new(databases),
            between_reads: None,
            engine: crate::engine::Engine::none(),
        };
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

    #[test]
    fn a_stopped_server_says_why() {
        let bridge = bridge(Vec::new());
        let background =
            Background { app: bridge.app, port: bridge.port, stopped: Arc::new(Mutex::new(Some("port lost".into()))) };
        let snapshot = background.snapshot();
        assert_eq!(snapshot.stopped.as_deref(), Some("port lost"));
        assert!(snapshot.databases.is_empty());
    }
}
