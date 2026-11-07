//! Times the database list of a bridge serving many small databases (#348):
//! the first `GET /v1/status` after the start, which opens every database,
//! then repeated `/v1/status` and `/v1/databases`, which look at each one's
//! files. The databases are built by hand, half 2CBH and half classic, in
//! temporary folders removed at the end; only counts and timings are printed.
//! Run the identical example on both commits:
//! `cargo run --release -p bridge --example database_list -- [databases] [repeats]`

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::Catalog;
use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::fixture_cbh::{self, Tok, encode, move_record};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use chesscore::Board;

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

/// A 2CBH database of three games of 1.e4.
fn two_cbh(name: &str) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for _ in 0..3 {
        b.game(e4);
    }
    b.write(name)
}

/// A classic database of one game of 1.e4.
fn classic(name: &str) -> TempDb {
    let mut b = fixture_cbh::Builder::new();
    b.game(&move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false)));
    b.write(name)
}

/// The status and the time of `GET path`.
fn get(port: u16, path: &str) -> (u16, Duration) {
    let started = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(600))).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n",
        DEFAULT_ORIGINS[0]
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, started.elapsed())
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

/// The median and the most of `repeats` requests for `path`.
fn repeated(port: u16, path: &str, repeats: usize) -> String {
    let mut times: Vec<Duration> = (0..repeats)
        .map(|_| {
            let (status, took) = get(port, path);
            assert_eq!(status, 200, "{path}");
            took
        })
        .collect();
    times.sort_unstable();
    format!("median {}, max {}", ms(times[times.len() / 2]), ms(times[times.len() - 1]))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let count: usize = args.next().map_or(500, |n| n.parse().expect("a number of databases"));
    let repeats: usize = args.next().map_or(20, |n| n.parse().expect("a number of repeats"));
    let built = Instant::now();
    let dbs: Vec<(TempDb, &str)> = (0..count)
        .map(|i| match i % 2 {
            0 => (two_cbh(&format!("list-profile-{i}")), "db.2cbh"),
            _ => (classic(&format!("list-profile-{i}")), "db.cbh"),
        })
        .collect();
    let paths: Vec<PathBuf> = dbs.iter().map(|(db, file)| db.dir().join(file)).collect();
    println!("{count} databases built in {:.1} s", built.elapsed().as_secs_f64());

    let data = std::env::temp_dir().join(format!("list-profile-data-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let listeners = bridge::server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let policy = Policy { port, origins: DEFAULT_ORIGINS.map(str::to_string).to_vec(), token: TOKEN.into() };
    let app = App::new("profile", policy, Catalog::new(paths));
    app.catalog.use_data_dir(&data);
    std::thread::spawn(move || bridge::server::serve(listeners, Arc::new(app)).unwrap());

    let (status, first) = get(port, "/v1/status");
    assert_eq!(status, 200);
    println!("first /v1/status        {}", ms(first));
    println!("/v1/status    x{repeats:<4}    {}", repeated(port, "/v1/status", repeats));
    println!("/v1/databases x{repeats:<4}    {}", repeated(port, "/v1/databases", repeats));
    let _ = std::fs::remove_dir_all(&data);
}
