//! A game's date, ECO code and round are one text wherever the bridge shows
//! them (#68): the game list's row, what a search matches, and the served
//! PGN's tag, which writes `?` where the list shows nothing.

use bridge::catalog::id_of;
use cbformat::fixture::{Builder, TempDb, lid_header, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

mod common;
use common::{TestBridge, app_of, get, member, objects, string_member};

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

/// The body of a `200` answer to `GET path`.
fn get_ok(port: u16, path: &str) -> String {
    let (status, body) = get(port, path);
    assert_eq!(status, 200, "{path}: {body}");
    body
}

/// The `[Key \"…\"]` tag of a PGN inside a JSON string.
fn tag<'a>(pgn: &'a str, name: &str) -> Option<&'a str> {
    let at = pgn.find(&format!(r#"[{name} \""#))? + name.len() + 4;
    Some(&pgn[at..at + pgn[at..].find('\\').unwrap()])
}

fn numbers(body: &str) -> Vec<u32> {
    objects(body, "rows").into_iter().map(|r| member(r, "number").parse().unwrap()).collect()
}

#[test]
fn the_list_the_search_and_the_pgn_agree() {
    let db = database("fields");
    let path = db.dir().join("db.2cbh");
    let bridge = TestBridge::new(app_of([path.clone()]));
    let port = bridge.port;
    let id = id_of(&path);
    let list = get_ok(port, &format!("/v1/databases/{id}/games?limit=20"));
    let rows = objects(&list, "rows");
    assert_eq!(rows.len(), GAMES.len());
    let mut seen = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let number = i as u32 + 1;
        let (date, eco, round) = (string_member(row, "date"), string_member(row, "eco"), string_member(row, "round"));
        let pgn = get_ok(port, &format!("/v1/databases/{id}/games/{number}"));
        assert_eq!(tag(&pgn, "Date"), Some(date), "game {number}");
        assert_eq!(tag(&pgn, "ECO"), Some(eco).filter(|e| !e.is_empty()), "game {number}");
        assert_eq!(tag(&pgn, "Round"), Some(if round.is_empty() { "?" } else { round }), "game {number}");
        // A search for the text the row shows finds the game.
        for (qualifier, value) in [("round", round), ("eco", eco), ("date", date)] {
            if value.is_empty() || (qualifier == "date" && value.contains('?')) {
                continue;
            }
            let found = get_ok(port, &format!("/v1/databases/{id}/games?limit=20&q={qualifier}%3A%22{value}%22"));
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
