//! A read that fails with a bug answers `500 internal` and is logged by its
//! database's id alone, without the database's folder or name (#117). The log
//! is one per process, so the test that opens it has a binary of its own.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::{log, server};
use cbformat::fixture_cbh::Builder;

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

/// Serves the database at `path`, with the indexes in `index_dir`.
fn start(path: &Path, index_dir: &Path) -> u16 {
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new(vec![path.to_path_buf()]),
        between_reads: None,
        engine: bridge::engine::Engine::none(),
    };
    app.catalog.explorer.set_dir(index_dir.to_path_buf());
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listeners, app));
    port
}

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {}\r\nConnection: close\r\n\r\n",
        DEFAULT_ORIGINS[0]
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split(' ').nth(1).unwrap().parse().unwrap();
    (status, out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

/// A classic database whose guiding text points into the `.cbg` file's own
/// header: a list window and a sort both read its title, and fail with a
/// format error, which no change of the database explains.
#[test]
fn a_damaged_database_answers_internal_and_is_logged_by_its_id() {
    let top = std::env::temp_dir().join(format!("bridge-internal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&top);
    let data = top.join("data");
    log::open(&data);

    let mut b = Builder::new();
    b.text(&[(0, b"Notes")])[1..5].fill(0);
    let built = b.write("internal");
    // The database under a folder and a name of the user's own.
    let folder = top.join("Jane Doe");
    std::fs::create_dir_all(&folder).unwrap();
    for file in std::fs::read_dir(built.dir()).unwrap() {
        let file = file.unwrap().path();
        let ext = file.extension().unwrap().to_str().unwrap();
        std::fs::copy(&file, folder.join(format!("Private Games.{ext}"))).unwrap();
    }
    drop(built);
    let path = folder.join("Private Games.cbh");
    let port = start(&path, &top.join("index"));
    let id = id_of(&path);

    for query in ["", "?sort=tournament"] {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games{query}"));
        assert_eq!(status, 500, "{query}: {body}");
        assert!(body.contains(r#""code":"internal""#), "{query}: {body}");
    }

    let log = std::fs::read_to_string(data.join(log::FILE_NAME)).unwrap();
    let logged = format!(" internal error on database {id}: format error: ");
    assert_eq!(log.lines().filter(|l| l.contains(&logged)).count(), 2, "{log}");
    for private in [folder.to_str().unwrap(), "Jane Doe", "Private Games.cbh", "Private Games"] {
        assert!(!log.contains(private), "{private} in {log}");
    }
    let _ = std::fs::remove_dir_all(&top);
}
