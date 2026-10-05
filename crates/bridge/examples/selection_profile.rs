//! Bounded before/after timings, synthetic by default, or a local database.
//! Prints only counts, timings and byte sizes; never database contents.
//! Run the identical example on both commits with separate empty folders:
//! `cargo run --release -p bridge --example selection_profile -- <empty-dir> [db.2cbh]`
use bridge::catalog::id_of;
use bridge::explorer::{self, runs::Progress};
use cbformat::fixture::{Builder, words};
use cbformat::movetable::{END_OF_LINE, MOVES};
use chesscore::Board;
use std::path::{Path, PathBuf};
use std::time::Instant;

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::Catalog;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
fn app_of(paths: impl IntoIterator<Item = PathBuf>) -> App {
    App::new(
        "profile",
        Policy { port: 0, origins: DEFAULT_ORIGINS.map(str::to_string).to_vec(), token: TOKEN.into() },
        Catalog::new(paths),
    )
}
fn board_after(line: &str) -> Board {
    let mut board = Board::startpos();
    for uci in line.split_whitespace() {
        let mv = board.legal_moves().into_iter().find(|m| explorer::uci(&board, *m) == uci).unwrap();
        board.play_checked(mv).unwrap();
    }
    board
}
fn fen_param(fen: &str) -> String {
    fen.replace(' ', "%20").replace('/', "%2F")
}
fn serve(mut app: App, dir: &Path) -> u16 {
    let listeners = bridge::server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    app.policy.port = port;
    app.catalog.use_data_dir(dir);
    std::thread::spawn(move || bridge::server::serve(listeners, Arc::new(app)).unwrap());
    port
}
fn get(port: u16, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n", DEFAULT_ORIGINS[0]).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    (headers.split_whitespace().nth(1).unwrap().parse().unwrap(), body.to_string())
}

const LINE: &str = "e2e4 e7e5 g1f3 b8c6 f1b5 a7a6 b5a4 g8f6 e1g1 f8e7 f1e1 b7b5 a4b3 d7d6 c2c3 e8g8 h2h3 c8b7 d2d4 f8e8 b1d2 e7f8 d4d5 c6b8 a2a4 c7c6 d5c6 b7c6";

fn synthetic() -> cbformat::fixture::TempDb {
    let mut b = Builder::new();
    let mut line = vec![MOVES];
    line.extend(words(&mut Board::startpos(), LINE));
    line.push(END_OF_LINE);
    let at = b.moves(1, &line);
    for i in 0..50_000u32 {
        let row = b.game(at);
        let white = (2000 + i % 1000) as i16;
        let black = if i % 13 == 0 { 0 } else { white - 100 };
        row[0x60..0x62].copy_from_slice(&white.to_le_bytes());
        row[0x70..0x72].copy_from_slice(&black.to_le_bytes());
        let date = (1980 + i % 45) << 9 | (1 + i % 12) << 5 | (1 + i % 28);
        row[0xbc..0xc0].copy_from_slice(&date.to_le_bytes());
    }
    b.write("selection-profile")
}

fn rss() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(0)
        * 1024
}
fn measure(port: u16, label: &str, paths: &[String], rounds: usize) {
    let mut times = Vec::new();
    for _ in 0..rounds {
        for path in paths {
            let t = Instant::now();
            let (status, body) = get(port, path);
            assert_eq!(status, 200, "benchmark answer failed (body suppressed)");
            assert!(body.contains("\"topGames\":"));
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    times.sort_by(f64::total_cmp);
    println!(
        "{label}: n={} median_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        times.len(),
        times[times.len() / 2],
        times[(times.len() * 95 / 100).min(times.len() - 1)],
        times[times.len() - 1]
    );
}
fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    assert!(!args.is_empty(), "selection_profile <data-dir> [database] [--reuse]");
    let dir = PathBuf::from(&args[0]);
    let reuse = args.get(2).is_some_and(|a| a == "--reuse");
    assert!(
        reuse || !dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none(),
        "use an empty folder or --reuse"
    );
    std::fs::create_dir_all(&dir).unwrap();
    let fixture = (args.len() == 1).then(synthetic);
    let path = args.get(1).map(PathBuf::from).unwrap_or_else(|| fixture.as_ref().unwrap().dir().join("db.2cbh"));
    let id = id_of(&path);
    let app = app_of([path]);
    let entry = app.catalog.get(&id).unwrap();
    let open = entry.open_to_read().expect("open database");
    let t = Instant::now();
    let progress = Progress::default();
    let loaded =
        explorer::prepare(&*open.db, open.generation, &dir.join("index"), &id, &progress).expect("index build");
    println!(
        "records={} games={} build_ms={:.3} index_bytes={} stream_bytes={} build_peak_rss_bytes={}",
        loaded.records(),
        loaded.games(),
        t.elapsed().as_secs_f64() * 1000.0,
        size(&loaded.base.path),
        size(&loaded.stream.path),
        rss()
    );
    drop((loaded, open));
    let port = serve(app, &dir);
    let plies: Vec<_> = LINE.split_whitespace().collect();
    let url = |n| format!("/v1/databases/{id}/explorer?fen={}", fen_param(&board_after(&plies[..n].join(" ")).fen()));
    measure(port, "first", &[url(0)], 1);
    let navigation: Vec<_> = (0..=10).map(url).collect();
    measure(port, "navigation_first", &navigation, 1);
    measure(port, "navigation_repeat", &navigation, 5);
    let filtered: Vec<_> = ["whiteelo:2700..", "date:2020.."].iter().map(|q| format!("{}&q={q}", url(2))).collect();
    measure(port, "filter_first", &filtered, 1);
    measure(port, "filter_repeat", &filtered, 5);
    let deep: Vec<_> = [22, 26, 28].into_iter().map(url).collect();
    measure(port, "deep_first", &deep, 1);
    measure(port, "deep_repeat", &deep, 5);
    println!("final_peak_rss_bytes={}", rss());
}
