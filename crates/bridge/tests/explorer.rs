//! The position index and `GET /v1/databases/{id}/explorer`, on databases
//! built by hand.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::explorer::file::{Bad, IndexFile};
use bridge::explorer::format::{Counts, pack_move};
use bridge::explorer::runs::Progress;
use bridge::explorer::{self, Loaded};
use bridge::server;
use cbformat::cbh;
use cbformat::fixture::{Builder, TempDb, lid_header, sq};
use cbformat::fixture_cbh::{self, Tok, encode, move_record, start_position};
use cbformat::movetable::{self, Captured, CastleSide, Color, END_OF_LINE, MOVES, MoveWord, Piece};
use cbformat::v2::Database;
use chesscore::{Board, Color as CColor, Move, Piece as CPiece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";

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

/// The move words of `ucis` played from `board`, which they advance; castling
/// is written `e1g1`.
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
                Some((q, _)) => match q {
                    CPiece::Queen => Captured::Queen,
                    CPiece::Rook => Captured::Rook,
                    CPiece::Bishop => Captured::Bishop,
                    CPiece::Knight => Captured::Knight,
                    _ => Captured::Pawn,
                },
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

/// A standard game of `ucis` with `result` (0 black, 1 draw, 2 white) and
/// ratings; its record, for further changes.
fn game<'a>(b: &'a mut Builder, ucis: &str, result: u8, elo: (i16, i16)) -> &'a mut [u8; 192] {
    let mut board = Board::startpos();
    let mut stream = vec![MOVES];
    stream.extend(words(&mut board, ucis));
    stream.push(END_OF_LINE);
    let at = b.moves(1, &stream);
    let rec = b.game(at);
    rec[0x58] = result;
    rec[0x60..0x62].copy_from_slice(&elo.0.to_le_bytes());
    rec[0x70..0x72].copy_from_slice(&elo.1.to_le_bytes());
    rec
}

/// Games 1-3 reach the position after 1.e4 e5 2.Nf3 Nc6, game 3 by another
/// order; game 4 castles; game 5 is deleted; game 6 is Chess960 from the
/// standard arrangement; game 7 is 30 plies long and alone past its fourth.
fn database(name: &str) -> TempDb {
    let mut b = Builder::new();
    game(&mut b, "e2e4 e7e5 g1f3 b8c6", 2, (2400, 2300));
    game(&mut b, "e2e4 e7e5 g1f3 b8c6 f1b5", 1, (2600, 2600));
    game(&mut b, "g1f3 b8c6 e2e4 e7e5", 0, (0, 2100));
    game(&mut b, "e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1", 2, (1500, 1500));
    game(&mut b, "e2e4", 2, (2800, 2800))[0] |= 0x80;
    let mut board = Board::startpos();
    let mut stream = vec![movetable::START_POSITION, 518, MOVES];
    stream.extend(words(&mut board, "e2e4"));
    stream.push(END_OF_LINE);
    let at = b.moves(2, &stream);
    b.game(at);
    game(
        &mut b,
        "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7 e1g1 d5c3 c1c3 e6e5 d1c2 e5e4 f3d2 d7f6 f1e1 c8f5",
        1,
        (2200, 2200),
    );
    b.lid(lid_header(1024, 1));
    b.write(name)
}

/// A classic game of `ucis` with `result` and ratings, as [`game`] writes a
/// 2CBH one; its record, for further changes.
fn classic_game<'a>(b: &'a mut fixture_cbh::Builder, ucis: &str, result: u8, elo: (u16, u16)) -> &'a mut [u8; 46] {
    let mut board = Board::startpos();
    let mut toks = Vec::new();
    for uci in ucis.split_whitespace() {
        let mv: Move = uci.parse().unwrap();
        let castles =
            board.piece_at(mv.from).map(|p| p.0) == Some(CPiece::King) && mv.from.file().abs_diff(mv.to.file()) == 2;
        toks.push(match castles {
            true if mv.to.file() == 6 => "O-O".to_string(),
            true => "O-O-O".to_string(),
            false => uci.to_string(),
        });
        let mut played = mv;
        if castles {
            played.to = chesscore::Square::new(if mv.to.file() == 6 { 7 } else { 0 }, mv.from.rank());
        }
        board.play_checked(played).unwrap();
    }
    let mut stream: Vec<Tok<'_>> = toks.iter().map(|t| Tok::Mv(t)).collect();
    stream.push(Tok::End);
    let rec = b.game(&move_record(0, None, None, &encode(&Board::startpos(), &stream, 0, false)));
    rec[0x1b] = result;
    rec[0x1f..0x21].copy_from_slice(&elo.0.to_be_bytes());
    rec[0x21..0x23].copy_from_slice(&elo.1.to_be_bytes());
    rec
}

/// The games of [`database`] in the classic format.
fn classic_database(name: &str) -> TempDb {
    let mut b = fixture_cbh::Builder::new();
    classic_game(&mut b, "e2e4 e7e5 g1f3 b8c6", 2, (2400, 2300));
    classic_game(&mut b, "e2e4 e7e5 g1f3 b8c6 f1b5", 1, (2600, 2600));
    classic_game(&mut b, "g1f3 b8c6 e2e4 e7e5", 0, (0, 2100));
    classic_game(&mut b, "e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1", 2, (1500, 1500));
    classic_game(&mut b, "e2e4", 2, (2800, 2800))[0] |= 0x80;
    // Chess960 from the standard arrangement: start 518.
    let start = Board::chess960(518).unwrap();
    let names: Vec<String> = (0..64u8).map(|i| format!("{}{}", (b'a' + i % 8) as char, i / 8 + 1)).collect();
    let pieces: Vec<(&str, CPiece, CColor)> =
        names.iter().filter_map(|n| start.piece_at(n.parse().unwrap()).map(|(p, c)| (n.as_str(), p, c))).collect();
    let mut extra = [0u8; 8];
    extra[6..8].copy_from_slice(&518u16.to_be_bytes());
    let stream = encode(&start, &[Tok::Mv("e2e4"), Tok::End], 10, false);
    let position = start_position(&pieces, false, 0x0f, 0);
    b.game(&move_record(0x4a, Some(&position), Some(&extra), &stream))[0x1b] = 2;
    classic_game(
        &mut b,
        "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7 e1g1 d5c3 c1c3 e6e5 d1c2 e5e4 f3d2 d7f6 f1e1 c8f5",
        1,
        (2200, 2200),
    );
    b.write(name)
}

fn key_after(ucis: &str) -> u64 {
    let mut b = Board::startpos();
    for u in ucis.split_whitespace() {
        let mut mv: Move = u.parse().unwrap();
        if b.piece_at(mv.from).map(|p| p.0) == Some(CPiece::King) && mv.from.file().abs_diff(mv.to.file()) == 2 {
            mv.to = chesscore::Square::new(if mv.to.file() == 6 { 7 } else { 0 }, mv.from.rank());
        }
        b.play_checked(mv).unwrap();
    }
    b.hash()
}

fn index_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-explorer-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn prepared(db: &TempDb, dir: &Path) -> Loaded {
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    explorer::prepare(&d, 1, dir, "db", &Progress::default()).unwrap()
}

#[test]
fn positions_moves_results_and_transpositions() {
    let db = database("explorer-counts");
    let dir = index_dir("counts");
    let idx = prepared(&db, &dir);
    // Games 1-4 and 7; the deleted game 5 and the Chess960 game 6 are left out.
    assert_eq!(idx.games(), 5);
    let start = idx.lookup(key_after("")).unwrap().unwrap();
    assert_eq!(start.counts, Counts { games: 5, white: 2, draws: 2, black: 1 });
    let e4 = start.moves.iter().find(|m| m.0 == pack_move("e2e4".parse().unwrap())).unwrap();
    assert_eq!(e4.1, Counts { games: 3, white: 2, draws: 1, black: 0 });
    // 1.e4 e5 2.Nf3 Nc6 and 1.Nf3 Nc6 2.e4 e5 are one position.
    let four = idx.lookup(key_after("e2e4 e7e5 g1f3 b8c6")).unwrap().unwrap();
    assert_eq!(four.counts.games, 4);
    assert_eq!(four.lookup_move("f1b5"), Some(1));
    assert_eq!(four.lookup_move("f1c4"), Some(1));
    // The best rated first; the later game among equals.
    assert_eq!(four.top, vec![2, 1, 3, 4]);
    // Game 7 is alone beyond ply 20 and dropped there, but kept up to it.
    let line7 = "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7";
    assert!(idx.lookup(key_after(line7)).unwrap().is_some(), "ply 20 is kept");
    assert!(idx.lookup(key_after(&format!("{line7} e1g1"))).unwrap().is_none(), "ply 21 alone is dropped");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A classic copy of the games gives the same index: every position of their
/// main lines has the same counts, moves and top games.
#[test]
fn a_classic_copy_is_indexed_the_same() {
    let (two_db, classic_db) = (database("explorer-pair-2cbh"), classic_database("explorer-pair-cbh"));
    let (two_dir, classic_dir) = (index_dir("pair-2cbh"), index_dir("pair-cbh"));
    let two = prepared(&two_db, &two_dir);
    let d = cbh::Database::open(classic_db.dir().join("db.cbh")).unwrap();
    let classic = explorer::prepare(&d, 1, &classic_dir, "db", &Progress::default()).unwrap();
    assert_eq!((classic.records(), classic.games()), (two.records(), two.games()));
    assert_eq!(classic.lookup(key_after("")).unwrap().unwrap().counts.games, 5);
    let lines = [
        "e2e4 e7e5 g1f3 b8c6 f1b5",
        "g1f3 b8c6 e2e4 e7e5",
        "e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1",
        "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7 e1g1",
    ];
    let mut compared = 0;
    for line in lines {
        let plies: Vec<&str> = line.split_whitespace().collect();
        for n in 0..=plies.len() {
            let key = key_after(&plies[..n].join(" "));
            assert_eq!(classic.lookup(key).unwrap(), two.lookup(key).unwrap(), "{n} plies of {line}");
            compared += 1;
        }
    }
    assert!(compared > 40);
    std::fs::remove_dir_all(&two_dir).unwrap();
    std::fs::remove_dir_all(&classic_dir).unwrap();
}

/// The game of a move record just over the index's limit is left out and
/// one at the limit is indexed, in both formats, whether the record is read
/// from a run's window or on its own: a hole after it in the move file makes
/// the run's span too large for the window's buffer.
#[test]
fn the_move_record_limit_holds_on_both_reading_paths() {
    use bridge::explorer::source::MAX_MOVE_RECORD;
    for format in ["2cbh", "cbh"] {
        for over in [0, 1] {
            for hole in [0usize, 128] {
                let name = format!("explorer-limit-{format}-{over}-{hole}");
                let db = if format == "2cbh" {
                    let mut b = Builder::new();
                    let mut words = vec![MOVES, movetable::encode(e4_word()).unwrap(), END_OF_LINE];
                    // Content of the limit, or one word over it.
                    words.resize(MAX_MOVE_RECORD / 2 + over, 0);
                    let at = b.moves(1, &words);
                    b.game(at);
                    b.write(&name)
                } else {
                    let mut b = fixture_cbh::Builder::new();
                    let mut stream = encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false);
                    // A whole record of the limit, or one byte over it.
                    stream.resize(MAX_MOVE_RECORD - 4 + over, 0);
                    b.game(&move_record(0, None, None, &stream));
                    b.write(&name)
                };
                let moves = db.dir().join(if format == "2cbh" { "db.2cbg" } else { "db.cbg" });
                let mut file = std::fs::OpenOptions::new().append(true).open(&moves).unwrap();
                file.write_all(&vec![0; hole]).unwrap();
                drop(file);
                let dir = index_dir(&name);
                let progress = Progress::default();
                let base = cbformat::view::Base::open(db.dir().join(format!("db.{format}"))).unwrap();
                let loaded = explorer::prepare(&base, 1, &dir, "db", &progress).unwrap();
                let skipped = progress.skipped.load(std::sync::atomic::Ordering::Relaxed);
                let want = if over == 1 { (0, 1) } else { (1, 0) };
                assert_eq!((loaded.games(), skipped), want, "{format}, {over} over the limit, hole {hole}");
                std::fs::remove_dir_all(&dir).unwrap();
            }
        }
    }
}

/// White's 1.e4.
fn e4_word() -> MoveWord {
    MoveWord::Normal {
        color: Color::White,
        piece: Piece::Pawn,
        from: sq("e2"),
        to: sq("e4"),
        captured: Captured::Nothing,
        promotion: None,
    }
}

trait MoveCount {
    fn lookup_move(&self, uci: &str) -> Option<u64>;
}

impl MoveCount for bridge::explorer::format::Stats {
    fn lookup_move(&self, uci: &str) -> Option<u64> {
        let code = pack_move(uci.parse().unwrap());
        self.moves.iter().find(|m| m.0 == code).map(|m| m.1.games)
    }
}

#[test]
fn a_damaged_index_is_refused_and_rebuilt() {
    let db = database("explorer-damage");
    let dir = index_dir("damage");
    let idx = prepared(&db, &dir);
    let path = idx.base.path.clone();
    drop(idx);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[200] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    let file = IndexFile::open(&path).unwrap();
    assert!(matches!(file.lookup(key_after("")), Err(Bad::Corrupt(_))), "the block's CRC catches it");
    std::fs::write(&path, &bytes[..bytes.len() - 5]).unwrap();
    assert!(matches!(IndexFile::open(&path), Err(Bad::Corrupt(_))), "a cut file is refused");
    // The next check builds it afresh.
    let idx = prepared(&db, &dir);
    assert_eq!(idx.lookup(key_after("")).unwrap().unwrap().counts.games, 5);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Five games of 1.e4 e5, then `extra` games; the first game's first move is
/// `first`, which must be a two-square pawn step so that every version of the
/// database has move records of equal length.
fn five(name: &str, first: &str, extra: &[&str]) -> TempDb {
    let mut b = Builder::new();
    game(&mut b, &format!("{first} e7e5"), 2, (2000, 2000));
    for _ in 0..4 {
        game(&mut b, "e2e4 e7e5", 2, (2000, 2000));
    }
    for ucis in extra {
        game(&mut b, ucis, 1, (2500, 2500));
    }
    b.write(name)
}

#[test]
fn any_change_rebuilds_the_whole_index() {
    let dir = index_dir("rebuild");
    let db = five("explorer-rebuild", "e2e4", &[]);
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let progress = Progress::default();
    let first = explorer::prepare(&d, 1, &dir, "db", &progress).unwrap();
    assert_eq!(progress.phase(), "merging", "built");
    assert_eq!(first.lookup(key_after("")).unwrap().unwrap().lookup_move("e2e4"), Some(5));
    drop(first);
    // The same generation: the file on disk is used.
    let progress = Progress::default();
    let again = explorer::prepare(&d, 1, &dir, "db", &progress).unwrap();
    assert_eq!(progress.phase(), "checking", "not built again");
    drop((again, d));
    // Only a move changed, 1.e4 to 1.d4 in a record of the same length: every
    // header record is as before, and the new generation rebuilds it all.
    let headers = std::fs::read(db.dir().join("db.2cbh")).unwrap();
    let db2 = five("explorer-rebuild", "d2d4", &[]);
    assert_eq!(headers, std::fs::read(db2.dir().join("db.2cbh")).unwrap(), "the header records are unchanged");
    let d = Database::open(db2.dir().join("db.2cbh")).unwrap();
    let progress = Progress::default();
    let moved = explorer::prepare(&d, 2, &dir, "db", &progress).unwrap();
    assert_eq!(progress.phase(), "merging", "built again");
    let start = moved.lookup(key_after("")).unwrap().unwrap();
    assert_eq!((start.lookup_move("e2e4"), start.lookup_move("d2d4")), (Some(4), Some(1)));
    drop((moved, d));
    // Games appended: built again whole, equal to a build from nothing.
    let db3 = five("explorer-rebuild", "d2d4", &["e2e4 c7c5", "c2c4"]);
    let d = Database::open(db3.dir().join("db.2cbh")).unwrap();
    let grown = explorer::prepare(&d, 3, &dir, "db", &Progress::default()).unwrap();
    let cold_dir = index_dir("rebuild-cold");
    let cold = explorer::prepare(&d, 3, &cold_dir, "db", &Progress::default()).unwrap();
    for ucis in ["", "e2e4", "e2e4 c7c5", "c2c4", "d2d4 e7e5"] {
        assert_eq!(grown.lookup(key_after(ucis)).unwrap(), cold.lookup(key_after(ucis)).unwrap(), "{ucis}");
    }
    assert_eq!(grown.lookup(key_after("")).unwrap().unwrap().counts, Counts { games: 7, white: 5, draws: 2, black: 0 });
    for dir in [dir, cold_dir] {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split(' ').nth(1).unwrap().parse().unwrap();
    (status, out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

fn serve(db: &TempDb, dir: &Path) -> (u16, String) {
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let path = db.dir().join("db.2cbh");
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new([path.clone()]),
        between_reads: None,
    };
    app.catalog.explorer.set_dir(dir.to_path_buf());
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listeners, app));
    (port, id_of(&path))
}

fn fen_param(fen: &str) -> String {
    fen.replace(' ', "%20").replace('/', "%2F")
}

#[test]
fn the_endpoint_builds_then_answers() {
    let db = database("explorer-http");
    let dir = index_dir("http");
    let (port, id) = serve(&db, &dir);
    let url = |fen: &str| format!("/v1/databases/{id}/explorer?fen={}", fen_param(fen));
    let start = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    let (status, body) = get(port, &url(start));
    assert_eq!(status, 409, "the first request starts the build: {body}");
    assert!(body.contains(r#""state":"indexing""#) && body.contains(r#""progress":{"phase":"#), "{body}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let body = loop {
        let (status, body) = get(port, &url(start));
        if status == 200 {
            break body;
        }
        assert_eq!(status, 409, "{body}");
        assert!(Instant::now() < deadline, "the index was not built");
        std::thread::sleep(Duration::from_millis(20));
    };
    let generation = format!(
        r#"{{"generation":"{:016x}","#,
        bridge::catalog::Catalog::new([db.dir().join("db.2cbh")]).entries()[0].generation().unwrap()
    );
    assert!(body.starts_with(&generation), "{body}");
    assert!(
        body.contains(r#""games":5,"white":2,"draws":2,"black":1,"moves":[{"uci":"e2e4","san":"e4","games":3"#),
        "{body}"
    );
    assert!(body.contains(r#""index":{"records":7,"games":5,"maxPly":40}"#), "{body}");
    // Castling is written as the king's two-square step.
    let before = "r1bqk1nr/pppp1ppp/2n5/2b1p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4";
    let (status, body) = get(port, &url(before));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""uci":"e1g1","san":"O-O""#), "{body}");
    assert!(body.contains(r#""topGames":[{"number":4,"white":"","black":"","whiteElo":1500,"blackElo":1500,"result":"1-0","year":null,"event":""}]"#), "{body}");
    // A position no game reached.
    let (status, body) = get(port, &url("4k3/8/8/8/8/8/8/4K3 w - - 0 1"));
    assert_eq!(
        (status, body.contains(r#""games":0,"white":0,"draws":0,"black":0,"moves":[],"topGames":[]"#)),
        (200, true),
        "{body}"
    );
    // A changed database is never answered from the index of its former
    // generation: the first request after the change starts a rebuild.
    let _grown = {
        std::thread::sleep(Duration::from_millis(20));
        let mut b = Builder::new();
        game(&mut b, "e2e4 e7e5 g1f3 b8c6", 2, (2400, 2300));
        b.lid(lid_header(1024, 1));
        b.write("explorer-http")
    };
    let (status, body) = get(port, &url(start));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""state":"indexing""#), "{body}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let body = loop {
        let (status, body) = get(port, &url(start));
        if status == 200 {
            break body;
        }
        assert!(Instant::now() < deadline, "the index was not rebuilt");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(body.contains(r#""games":1,"white":1,"draws":0,"black":0"#), "{body}");
    // Chess960: the two positions a Polyglot key cannot tell apart.
    for fen in ["4k3/8/8/8/8/8/8/4KR1R w F - 0 1", "4k3/8/8/8/8/8/8/4KR1R w H - 0 1"] {
        let (status, body) = get(port, &url(fen));
        assert_eq!(status, 422, "{body}");
        assert!(body.contains(r#""code":"unsupported""#) && body.contains(r#""variant":"chess960""#), "{body}");
    }
    let (status, _) = get(port, &format!("/v1/databases/{id}/explorer?fen={}&variant=chess960", fen_param(start)));
    assert_eq!(status, 422);
    for bad in ["", "?fen=nonsense"] {
        let (status, body) = get(port, &format!("/v1/databases/{id}/explorer{bad}"));
        assert_eq!(status, 400, "{body}");
        assert!(body.contains(r#""parameter":"fen""#), "{body}");
    }
    assert_eq!(get(port, &format!("/v1/databases/0000000000000000/explorer?fen={}", fen_param(start))).0, 404);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn chess960_games_never_reach_the_index() {
    let db = database("explorer-960");
    let dir = index_dir("960");
    let idx = prepared(&db, &dir);
    // The aliasing positions of review #24 share this key; no entry holds it.
    let alias = Board::from_fen("4k3/8/8/8/8/8/8/4KR1R w F - 0 1").unwrap();
    assert_eq!(alias.hash(), 0x74c1_e278_8bfe_39d9);
    assert_eq!(alias.hash(), Board::from_fen("4k3/8/8/8/8/8/8/4KR1R w H - 0 1").unwrap().hash());
    assert!(idx.lookup(alias.hash()).unwrap().is_none());
    // The Chess960 game from the standard arrangement played 1.e4 too, and is
    // not among the three games of 1.e4.
    let start = idx.lookup(key_after("")).unwrap().unwrap();
    assert_eq!(start.lookup_move("e2e4"), Some(3));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_promotion_from_a_set_up_position() {
    let mut b = Builder::new();
    let p = |c, pc, at| movetable::encode_piece_word(c, pc, sq(at)).unwrap();
    let stream = [
        movetable::START_POSITION,
        1,
        0,
        0,
        p(Color::White, Piece::King, "a1"),
        p(Color::White, Piece::Pawn, "a7"),
        p(Color::Black, Piece::King, "h8"),
        MOVES,
        movetable::encode(MoveWord::Normal {
            color: Color::White,
            piece: Piece::Pawn,
            from: sq("a7"),
            to: sq("a8"),
            captured: Captured::Nothing,
            promotion: Some(Piece::Knight),
        })
        .unwrap(),
        END_OF_LINE,
    ];
    let at = b.moves(1, &stream);
    b.game(at);
    let db = b.write("explorer-promotion");
    let dir = index_dir("promotion");
    let idx = prepared(&db, &dir);
    let before = Board::from_fen("7k/P7/8/8/8/8/8/K7 w - - 0 1").unwrap();
    let s = idx.lookup(before.hash()).unwrap().unwrap();
    assert_eq!(s.lookup_move("a7a8n"), Some(1));
    assert_eq!(s.lookup_move("a7a8q"), None);
    std::fs::remove_dir_all(&dir).unwrap();
}
