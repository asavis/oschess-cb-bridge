//! The bridge's state at one moment, for a user interface to poll: whether it
//! still serves, why it stopped, and the state of each database.

use std::sync::{Arc, Mutex};

use crate::api::App;
use crate::catalog::State;
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
    /// The name ChessBase shows: the file name without its extension.
    pub name: String,
    pub state: State,
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
        std::thread::Builder::new().name("bridge-server".into()).spawn(move || {
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
        let databases = self.app.catalog.entries().iter();
        Snapshot {
            version: self.app.version,
            port: self.port,
            stopped: self.stopped.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            databases: databases
                .map(|e| Database { id: e.id.clone(), name: e.name.clone(), state: e.state() })
                .collect(),
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

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

    fn bridge(databases: Vec<PathBuf>) -> Bridge {
        let listeners = server::bind(0).unwrap();
        let port = listeners[0].local_addr().unwrap().port();
        let app = App {
            version: "test",
            policy: Policy { port, origins: DEFAULT_ORIGINS.map(String::from).to_vec(), token: TOKEN.into() },
            catalog: Catalog::new(databases),
            between_reads: None,
        };
        Bridge { listeners, app: Arc::new(app), port, token: TOKEN.into(), link: String::new(), first_run: false }
    }

    #[test]
    fn a_serving_bridge_and_its_databases() {
        let missing = PathBuf::from("/no/such/folder/Games.2cbh");
        let bridge = bridge(vec![missing.clone(), PathBuf::from("/no/such/folder/Old.pgn")]);
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
        let states: Vec<(&str, State)> = snapshot.databases.iter().map(|d| (d.name.as_str(), d.state)).collect();
        assert_eq!(states, [("Games", State::Missing), ("Old", State::Missing)]);
        assert_eq!(snapshot.databases[0].id, crate::catalog::id_of(&missing));
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
