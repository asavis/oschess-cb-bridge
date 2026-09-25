//! A game's date, ECO code and round are one text wherever the bridge shows
//! them (#68): the game list's row, what a search matches, and the served
//! PGN's tag, which writes `?` where the list shows nothing.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::server;
use cbformat::fixture::{Builder, TempDb, lid_header, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

/// A date (year, month, day, 0 unknown), an ECO field as stored, and a round
/// and sub-round as 2CBH stores them, signed.
type Fields = ((i32, i32, i32), u16, (i16, i16));

const GAMES: [Fields; 9] = [
    ((2020, 2, 15), 128, (5, 2)),
    ((1998, 0, 0), 500 * 128 + 5, (5, 0)),
    ((1858, 12, 0), 0, (0, 0)),
    ((0, 0, 0), 64576 + 518, (0, 3)),
    ((2001, 0, 9), 1, (-1, 0)),
    ((2024, 7, 31), 64128, (5, -1)),
    ((1972, 7, 11), 200 * 128, (12, 7)),
    ((2026, 9, 25), 499 * 128 + 127, (32767, 32767)),
    ((1886, 1, 11), 64575, (-32768, 5)),
];

fn database(name: &str) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for ((year, month, day), eco, (round, sub)) in GAMES {
        let rec = b.game(e4);
        rec[0xbc..0xc0].copy_from_slice(&((year << 9) | (month << 5) | day).to_le_bytes());
        rec[0x80..0x82].copy_from_slice(&eco.to_le_bytes());
        rec[0x5a..0x5c].copy_from_slice(&round.to_le_bytes());
        rec[0x5c..0x5e].copy_from_slice(&sub.to_le_bytes());
    }
    b.lid(lid_header(1024, 1));
    b.write(name)
}

fn start(paths: Vec<PathBuf>) -> u16 {
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new(paths),
        between_reads: None,
        engine: bridge::engine::Engine::none(),
    };
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listeners, app));
    port
}

fn get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {}\r\nConnection: close\r\n\r\n",
        DEFAULT_ORIGINS[0]
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{path}: {out}");
    out.split_once("\r\n\r\n").unwrap().1.to_string()
}

/// A string member `"key":"…"` of a JSON text, or a `[Key \"…\"]` tag of a
/// PGN inside one.
fn member<'a>(text: &'a str, key: &str) -> &'a str {
    let at = text.find(&format!(r#""{key}":""#)).unwrap_or_else(|| panic!("no {key} in {text}")) + key.len() + 4;
    &text[at..at + text[at..].find('"').unwrap()]
}

fn tag<'a>(pgn: &'a str, name: &str) -> Option<&'a str> {
    let at = pgn.find(&format!(r#"[{name} \""#))? + name.len() + 4;
    Some(&pgn[at..at + pgn[at..].find('\\').unwrap()])
}

fn numbers(body: &str) -> Vec<u32> {
    body.split(r#"{"number":"#).skip(1).map(|r| r[..r.find(',').unwrap()].parse().unwrap()).collect()
}

#[test]
fn the_list_the_search_and_the_pgn_agree() {
    let db = database("fields");
    let path = db.dir().join("db.2cbh");
    let port = start(vec![path.clone()]);
    let id = id_of(&path);
    let list = get(port, &format!("/v1/databases/{id}/games?limit=20"));
    let rows: Vec<&str> = list.split(r#"{"number":"#).skip(1).collect();
    assert_eq!(rows.len(), GAMES.len());
    let mut seen = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let number = i as u32 + 1;
        let (date, eco, round) = (member(row, "date"), member(row, "eco"), member(row, "round"));
        let pgn = get(port, &format!("/v1/databases/{id}/games/{number}"));
        assert_eq!(tag(&pgn, "Date"), Some(date), "game {number}");
        assert_eq!(tag(&pgn, "ECO"), Some(eco).filter(|e| !e.is_empty()), "game {number}");
        assert_eq!(tag(&pgn, "Round"), Some(if round.is_empty() { "?" } else { round }), "game {number}");
        // A search for the text the row shows finds the game.
        for (qualifier, value) in [("round", round), ("eco", eco), ("date", date)] {
            if value.is_empty() || (qualifier == "date" && value.contains('?')) {
                continue;
            }
            let found = get(port, &format!("/v1/databases/{id}/games?limit=20&q={qualifier}%3A%22{value}%22"));
            assert!(numbers(&found).contains(&number), "{qualifier}:\"{value}\" finds game {number}: {found}");
        }
        seen.push((date.to_string(), eco.to_string(), round.to_string()));
    }
    let expected = [
        ("2020.02.15", "A00", "5(2)"),
        ("1998.??.??", "E99", "5"),
        ("1858.12.??", "", ""),
        ("????.??.??", "", ""),
        ("2001.??.09", "", ""),
        ("2024.07.31", "", "5"),
        ("1972.07.11", "B99", "12(7)"),
        ("2026.09.25", "E98", "32767(32767)"),
        ("1886.01.11", "", ""),
    ];
    let expected: Vec<(String, String, String)> =
        expected.iter().map(|(d, e, r)| (d.to_string(), e.to_string(), r.to_string())).collect();
    assert_eq!(seen, expected);
}
