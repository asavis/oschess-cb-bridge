//! The games window's `line` parameter (#81): the start of each game's main
//! line in SAN, as `GET /games/{number}` writes it, in both formats.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::server;
use cbformat::fixture::{Builder, TempDb, lid_header, sq};
use cbformat::fixture_cbh::{self, Tok, encode, move_record, start_position};
use cbformat::movetable::{
    self, ALTERNATIVE, Captured, CastleSide, Color, END_OF_LINE, MOVES, MoveWord, NULL_MOVE, Piece,
};
use chesscore::{Board, Color as CColor, Move, Piece as CPiece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

/// Castling, a capture, a knight named by its file, a promotion with check,
/// a mate: the games both formats hold, with the line each must answer.
const GAMES: [(&str, &str); 3] = [
    ("e2e4 e7e5 g1f3 b8c6 f1c4 g8f6 e1g1 f6e4 d2d3 e4f6 b1d2", "e4 e5 Nf3 Nc6 Bc4 Nf6 O-O Nxe4 d3 Nf6 Nbd2"),
    ("e2e4 d7d5 e4d5 c7c6 d5c6 d8d7 c6b7 g8f6 b7c8q d7d8", "e4 d5 exd5 c6 dxc6 Qd7 cxb7 Nf6 bxc8=Q+ Qd8"),
    ("f2f3 e7e5 g2g4 d8h4", "f3 e5 g4 Qh4#"),
];

fn color(c: CColor) -> Color {
    if c == CColor::White { Color::White } else { Color::Black }
}

fn piece(p: CPiece) -> Piece {
    match p {
        CPiece::King => Piece::King,
        CPiece::Queen => Piece::Queen,
        CPiece::Rook => Piece::Rook,
        CPiece::Bishop => Piece::Bishop,
        CPiece::Knight => Piece::Knight,
        CPiece::Pawn => Piece::Pawn,
    }
}

/// The 2CBH words of `ucis` played from `board`.
fn words(board: &mut Board, ucis: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for uci in ucis.split_whitespace() {
        let mut mv: Move = uci.parse().unwrap();
        let (p, c) = board.piece_at(mv.from).unwrap();
        let word = if p == CPiece::King && mv.from.file().abs_diff(mv.to.file()) == 2 {
            let short = mv.to.file() == 6;
            mv.to = chesscore::Square::new(if short { 7 } else { 0 }, mv.from.rank());
            MoveWord::Castle { color: color(c), side: if short { CastleSide::Short } else { CastleSide::Long } }
        } else {
            let captured = match board.piece_at(mv.to) {
                Some((CPiece::Queen, _)) => Captured::Queen,
                Some((CPiece::Rook, _)) => Captured::Rook,
                Some((CPiece::Bishop, _)) => Captured::Bishop,
                Some((CPiece::Knight, _)) => Captured::Knight,
                Some(_) => Captured::Pawn,
                None if p == CPiece::Pawn && mv.from.file() != mv.to.file() => Captured::EnPassant,
                None => Captured::Nothing,
            };
            MoveWord::Normal {
                color: color(c),
                piece: piece(p),
                from: mv.from.index() as u8,
                to: mv.to.index() as u8,
                captured,
                promotion: mv.promotion.map(piece),
            }
        };
        board.play_checked(mv).unwrap();
        out.push(movetable::encode(word).unwrap());
    }
    out
}

/// A 2CBH game of the words `stream` holds after `MOVES`.
fn game(b: &mut Builder, stream: &[u16]) {
    let mut all = vec![MOVES];
    all.extend(stream);
    all.push(END_OF_LINE);
    let at = b.moves(1, &all);
    b.game(at)[0x58] = 2;
}

/// Records 1-3 [`GAMES`]; 4 has the variation 1...c6 2.d4 to its 1...c5 of
/// 1.e4 c5 2.Nf3; 5 plays a null move after 1.e4; 6 starts from a set-up
/// position; 7 is Chess960; 8 turns illegal after 1.e4 e5; 9 points at no move
/// record; 10 is a guiding text.
fn two_cbh(name: &str) -> TempDb {
    let mut b = Builder::new();
    for (ucis, _) in GAMES {
        game(&mut b, &words(&mut Board::startpos(), ucis));
    }
    let mut board = Board::startpos();
    let mut stream = words(&mut board, "e2e4 c7c5");
    stream.push(ALTERNATIVE);
    stream.extend(words(&mut board.clone(), "g1f3"));
    stream.push(END_OF_LINE);
    let mut before = Board::startpos();
    words(&mut before, "e2e4");
    stream.extend(words(&mut before, "c7c6 d2d4"));
    game(&mut b, &stream);
    let mut stream = words(&mut Board::startpos(), "e2e4");
    stream.push(NULL_MOVE);
    game(&mut b, &stream);
    let p = |c, pc, at| movetable::encode_piece_word(c, pc, sq(at)).unwrap();
    let setup = [
        movetable::START_POSITION,
        1,
        0,
        0,
        p(Color::White, Piece::King, "a1"),
        p(Color::White, Piece::Pawn, "a2"),
        p(Color::Black, Piece::King, "h8"),
        MOVES,
        movetable::encode(MoveWord::Normal {
            color: Color::White,
            piece: Piece::Pawn,
            from: sq("a2"),
            to: sq("a3"),
            captured: Captured::Nothing,
            promotion: None,
        })
        .unwrap(),
        END_OF_LINE,
    ];
    let at = b.moves(1, &setup);
    b.game(at);
    let mut stream = vec![movetable::START_POSITION, 518, MOVES];
    stream.extend(words(&mut Board::startpos(), "e2e4"));
    stream.push(END_OF_LINE);
    let at = b.moves(2, &stream);
    b.game(at);
    let mut stream = words(&mut Board::startpos(), "e2e4 e7e5");
    stream.extend(words(&mut Board::startpos(), "e2e4"));
    game(&mut b, &stream);
    b.game(5);
    let at = b.moves(1, &[MOVES, END_OF_LINE]);
    b.game(at)[0] |= 2;
    b.lid(lid_header(1024, 1));
    b.write(name)
}

/// Tokens of `ucis` for the classic encoder: castling by name.
fn toks(board: &mut Board, ucis: &str) -> Vec<String> {
    let mut out = Vec::new();
    for uci in ucis.split_whitespace() {
        let mut mv: Move = uci.parse().unwrap();
        let castles =
            board.piece_at(mv.from).map(|p| p.0) == Some(CPiece::King) && mv.from.file().abs_diff(mv.to.file()) == 2;
        out.push(match castles {
            true if mv.to.file() == 6 => "O-O".to_string(),
            true => "O-O-O".to_string(),
            false => uci.to_string(),
        });
        if castles {
            mv.to = chesscore::Square::new(if mv.to.file() == 6 { 7 } else { 0 }, mv.from.rank());
        }
        board.play_checked(mv).unwrap();
    }
    out
}

fn classic_game(b: &mut fixture_cbh::Builder, items: &[&str]) {
    let mut stream: Vec<Tok<'_>> = items
        .iter()
        .map(|t| {
            if *t == "|" {
                Tok::Var
            } else if *t == ";" {
                Tok::End
            } else {
                Tok::Mv(t)
            }
        })
        .collect();
    stream.push(Tok::End);
    b.game(&move_record(0, None, None, &encode(&Board::startpos(), &stream, 0, false)))[0x1b] = 2;
}

/// Records 1-3 [`GAMES`]; 4 the variation of [`two_cbh`]'s; 5 a null move
/// after 1.e4; 6 a set-up position; 7 Chess960; 8 a guiding text.
fn classic(name: &str) -> TempDb {
    let mut b = fixture_cbh::Builder::new();
    for (ucis, _) in GAMES {
        let t = toks(&mut Board::startpos(), ucis);
        classic_game(&mut b, &t.iter().map(String::as_str).collect::<Vec<_>>());
    }
    // `|` marks a move with alternatives still to come; `;` ends a line.
    classic_game(&mut b, &["e2e4", "|", "c7c5", "g1f3", ";", "c7c6", "d2d4"]);
    classic_game(&mut b, &["e2e4", "--"]);
    let pieces =
        [("a1", CPiece::King, CColor::White), ("a2", CPiece::Pawn, CColor::White), ("h8", CPiece::King, CColor::Black)];
    let position = start_position(&pieces, false, 0, 0);
    let start = {
        let mut b = chesscore::BoardBuilder::empty();
        for (at, p, c) in pieces {
            b.set(at.parse().unwrap(), Some((p, c)));
        }
        b.build().unwrap()
    };
    let stream = encode(&start, &[Tok::Mv("a2a3"), Tok::End], 0, false);
    b.game(&move_record(0x40, Some(&position), None, &stream));
    let start = Board::chess960(518).unwrap();
    let names: Vec<String> = (0..64u8).map(|i| format!("{}{}", (b'a' + i % 8) as char, i / 8 + 1)).collect();
    let pieces: Vec<(&str, CPiece, CColor)> =
        names.iter().filter_map(|n| start.piece_at(n.parse().unwrap()).map(|(p, c)| (n.as_str(), p, c))).collect();
    let mut extra = [0u8; 8];
    extra[6..8].copy_from_slice(&518u16.to_be_bytes());
    let stream = encode(&start, &[Tok::Mv("e2e4"), Tok::End], 10, false);
    b.game(&move_record(0x4a, Some(&start_position(&pieces, false, 0x0f, 0)), Some(&extra), &stream));
    b.text(&[(0, b"A text".as_slice())]);
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

/// Each row's `line` member, verbatim, by the row's number: `"…"`, `null`,
/// or absent.
fn lines(body: &str) -> Vec<(u32, Option<String>)> {
    body.split(r#"{"number":"#)
        .skip(1)
        .map(|row| {
            let number = row[..row.find(',').unwrap()].parse().unwrap();
            let row = &row[..row.find(r#"{"number":"#).unwrap_or(row.len())];
            let line = row.find(r#""line":"#).map(|at| {
                let value = &row[at + 7..];
                match value.strip_prefix('"') {
                    Some(text) => text[..text.find('"').unwrap()].to_string(),
                    None => value[..4].to_string(),
                }
            });
            (number, line)
        })
        .collect()
}

/// The main line of a served PGN in SAN, one space between moves: its
/// comments, variations, move numbers and result left out.
fn main_line(pgn: &str) -> String {
    let movetext = pgn.split("\\n\\n").nth(1).unwrap();
    let (mut depth, mut comment, mut out) = (0, false, String::new());
    for c in movetext.chars() {
        match c {
            '{' => comment = true,
            '}' => comment = false,
            '(' if !comment => depth += 1,
            ')' if !comment => depth -= 1,
            _ if comment || depth > 0 => {}
            c => out.push(c),
        }
    }
    out.replace("\\n", " ")
        .split_whitespace()
        .filter(|t| !t.ends_with('.') && !["1-0", "0-1", "1/2-1/2", "*"].contains(t))
        .collect::<Vec<_>>()
        .join(" ")
}

fn served(port: u16, id: &str, number: u32) -> String {
    let (status, body) = get(port, &format!("/v1/databases/{id}/games/{number}"));
    assert_eq!(status, 200, "{body}");
    let pgn = &body[body.find(r#""pgn":""#).unwrap() + 7..];
    main_line(&pgn[..pgn.find(r#"","annotations""#).unwrap()])
}

#[test]
fn a_window_carries_each_games_main_line() {
    let (a, c) = (two_cbh("lines-2cbh"), classic("lines-cbh"));
    let (pa, pc) = (a.dir().join("db.2cbh"), c.dir().join("db.cbh"));
    let port = start(vec![pa.clone(), pc.clone()]);
    for (path, count) in [(&pa, 10), (&pc, 8)] {
        let id = id_of(path);
        let (status, body) = get(port, &format!("/v1/databases/{id}/games?limit=20&line=60"));
        assert_eq!(status, 200, "{body}");
        let got = lines(&body);
        assert_eq!(got.len(), count, "{body}");
        for (i, (_, want)) in GAMES.iter().enumerate() {
            assert_eq!(got[i].1.as_deref(), Some(*want), "{path:?} game {}", i + 1);
            assert_eq!(served(port, &id, i as u32 + 1), *want, "the line is the served PGN's main line");
        }
        assert_eq!(got[3].1.as_deref(), Some("e4 c5 Nf3"), "{path:?}: a variation is not the main line");
        assert_eq!(served(port, &id, 4), "e4 c5 Nf3");
        let (_, pgn) = get(port, &format!("/v1/databases/{id}/games/4"));
        assert!(pgn.contains("c6 2. d4)"), "{path:?}: the game holds the variation: {pgn}");
        assert_eq!(got[4].1.as_deref(), Some("e4"), "{path:?}: a null move ends the line");
        assert_eq!(got[5].1.as_deref(), Some("null"), "{path:?}: a set-up position has no line");
        assert_eq!(got[6].1.as_deref(), Some("null"), "{path:?}: Chess960 has no line");
        assert_eq!(got[count - 1].1, None, "{path:?}: a guiding text has no line member");
    }
    let id = id_of(&pa);
    let (_, body) = get(port, &format!("/v1/databases/{id}/games?limit=20&line=60"));
    let got = lines(&body);
    assert_eq!(got[7].1.as_deref(), Some("e4 e5"), "damage ends the line before it");
    assert_eq!(got[8].1.as_deref(), Some("null"), "a game without a readable move record");
}

#[test]
fn line_counts_plies_and_follows_search_and_sort() {
    let db = two_cbh("lines-plies");
    let path = db.dir().join("db.2cbh");
    let port = start(vec![path.clone()]);
    let id = id_of(&path);
    let (status, body) = get(port, &format!("/v1/databases/{id}/games?limit=3&line=3"));
    assert_eq!(status, 200, "{body}");
    let got: Vec<_> = lines(&body).into_iter().map(|(_, l)| l.unwrap()).collect();
    assert_eq!(got, ["e4 e5 Nf3", "e4 d5 exd5", "f3 e5 g4"]);
    let (_, body) = get(port, &format!("/v1/databases/{id}/games?offset=1&limit=2&sort=number-desc&line=1"));
    assert_eq!(lines(&body), [(9, Some("null".to_string())), (8, Some("e4".to_string()))]);
    // A sort by a key reads the rows by number: each keeps its own line.
    let (_, plain) = get(port, &format!("/v1/databases/{id}/games?limit=20&line=60"));
    let (_, sorted) = get(port, &format!("/v1/databases/{id}/games?limit=20&sort=result&line=60"));
    let (mut plain, mut sorted) = (lines(&plain), lines(&sorted));
    assert_ne!(plain, sorted, "the sort reorders the rows");
    sorted.sort();
    plain.sort();
    assert_eq!(sorted, plain);
}

#[test]
fn a_window_without_line_is_unchanged_and_line_is_bounded() {
    let db = two_cbh("lines-bounds");
    let path = db.dir().join("db.2cbh");
    let port = start(vec![path.clone()]);
    let id = id_of(&path);
    let (status, body) = get(port, &format!("/v1/databases/{id}/games?limit=20"));
    assert_eq!(status, 200);
    assert!(!body.contains(r#""line""#), "{body}");
    for bad in ["0", "61", "x", "-1", "256"] {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games?line={bad}"));
        assert_eq!(status, 400, "line={bad}: {body}");
        assert!(body.contains(r#""parameter":"line""#) && body.contains("between 1 and 60"), "{body}");
    }
}
