//! The position index and `GET /v1/databases/{id}/explorer`, on databases
//! built by hand.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use bridge::catalog::id_of;
use bridge::explorer::file::{Bad, IndexFile};
use bridge::explorer::format::{
    BLOCK_ENTRY, Block, Counts, DEEP_BLOCK_ENTRY, HEADER_LEN, Header, KEY_ENTRY, Stats, pack_move,
};
use bridge::explorer::runs::Progress;
use bridge::explorer::{self, Loaded};
use bridge::indexdir::crc32;
use bridge::search::memory::Cancel;
use cbformat::cbh;
use cbformat::fixture::{Builder, TempDb, lid_header, quiet, sq, words};
use cbformat::fixture_cbh::{self, Tok, encode, move_record, start_position};
use cbformat::movetable::{self, Captured, Color, END_OF_LINE, MOVES, MoveWord, Piece};
use cbformat::v2::Database;
use chesscore::{Board, Color as CColor, Move, Piece as CPiece};

mod common;
use common::{TestBridge, answered, app_of, board_after, fen_param, get, index_dir, objects};

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
/// order; game 4 castles, played in a known month on an unknown day; game 5 is
/// deleted; game 6 is Chess960 from the standard arrangement; game 7 is 30
/// plies long and alone past its fourth.
fn database(name: &str) -> TempDb {
    let mut b = Builder::new();
    game(&mut b, "e2e4 e7e5 g1f3 b8c6", 2, (2400, 2300));
    game(&mut b, "e2e4 e7e5 g1f3 b8c6 f1b5", 1, (2600, 2600));
    game(&mut b, "g1f3 b8c6 e2e4 e7e5", 0, (0, 2100));
    let castles = game(&mut b, "e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1", 2, (1500, 1500));
    castles[0xbc..0xc0].copy_from_slice(&((2003i32 << 9) | (7 << 5)).to_le_bytes());
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
    let castles = classic_game(&mut b, "e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1", 2, (1500, 1500));
    castles[0x18..0x1b].copy_from_slice(&((2003u32 << 9) | (7 << 5)).to_be_bytes()[1..]);
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
    board_after(ucis).hash()
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
    drop(idx);
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
    drop((two, classic));
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
                drop(loaded);
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
    // A byte of the block that holds the starting position.
    let header = Header::decode(&bytes).unwrap();
    let block = (0..header.blocks as usize)
        .map(|i| Block::decode(&bytes[header.table_offset as usize + i * BLOCK_ENTRY..]))
        .rfind(|b| b.first_key <= key_after(""))
        .unwrap();
    bytes[block.offset as usize + 4] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    let file = IndexFile::open(&path).unwrap();
    assert!(matches!(file.lookup(key_after("")), Err(Bad::Corrupt(_))), "the block's CRC catches it");
    drop(file);
    std::fs::write(&path, &bytes[..bytes.len() - 5]).unwrap();
    assert!(matches!(IndexFile::open(&path), Err(Bad::Corrupt(_))), "a cut file is refused");
    // The next check builds it afresh.
    let idx = prepared(&db, &dir);
    assert_eq!(idx.lookup(key_after("")).unwrap().unwrap().counts.games, 5);
    drop(idx);
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
    assert_eq!(progress.phase(), "structures", "built");
    assert_eq!(first.lookup(key_after("")).unwrap().unwrap().lookup_move("e2e4"), Some(5));
    // Where the build's time went, a pass at a time.
    let timings = first.built.clone().unwrap();
    assert_eq!(timings, progress.timings());
    let passes = (timings.tree.len() as u64, timings.deep.len() as u64);
    let relaxed = std::sync::atomic::Ordering::Relaxed;
    assert_eq!(passes, (progress.tree_passes.load(relaxed), progress.deep_passes.load(relaxed)));
    assert_eq!(passes, (1, 1));
    drop(first);
    // The same generation: the file on disk is used.
    let progress = Progress::default();
    let again = explorer::prepare(&d, 1, &dir, "db", &progress).unwrap();
    assert_eq!(progress.phase(), "checking", "not built again");
    assert!(again.built.is_none());
    drop((again, d));
    // Only a move changed, 1.e4 to 1.d4 in a record of the same length: every
    // header record is as before, and the new generation rebuilds it all.
    let headers = std::fs::read(db.dir().join("db.2cbh")).unwrap();
    let db2 = five("explorer-rebuild", "d2d4", &[]);
    assert_eq!(headers, std::fs::read(db2.dir().join("db.2cbh")).unwrap(), "the header records are unchanged");
    let d = Database::open(db2.dir().join("db.2cbh")).unwrap();
    let progress = Progress::default();
    let moved = explorer::prepare(&d, 2, &dir, "db", &progress).unwrap();
    assert_eq!(progress.phase(), "structures", "built again");
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
    drop((grown, cold));
    for dir in [dir, cold_dir] {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn the_endpoint_builds_then_answers() {
    let db = database("explorer-http");
    let dir = index_dir("http");
    let (bridge, id) = TestBridge::database(&db, &dir);
    let port = bridge.port;
    let url = |fen: &str| format!("/v1/databases/{id}/explorer?fen={}", fen_param(fen));
    let start = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    let (status, body) = get(port, &url(start));
    assert_eq!(status, 409, "the first request starts the build: {body}");
    assert!(body.contains(r#""state":"indexing""#) && body.contains(r#""progress":{"phase":"#), "{body}");
    let body = answered(port, &url(start));
    let generation = format!(
        r#"{{"generation":"{:016x}","#,
        bridge::catalog::Catalog::new([db.dir().join("db.2cbh")]).entries()[0].generation().unwrap()
    );
    assert!(body.starts_with(&generation), "{body}");
    assert!(
        body.contains(r#""games":5,"white":2,"draws":2,"black":1,"moves":[{"uci":"e2e4","san":"e4","games":3"#),
        "{body}"
    );
    assert!(body.contains(r#""index":{"records":7,"games":5}"#), "{body}");
    // A game without a date.
    assert!(
        body.contains(
            r#""date":"????.??.??","round":"","annotator":"","flags":{"deleted":false,"chess960":false},"year":null}"#
        ),
        "{body}"
    );
    // Castling is written as the king's two-square step.
    let before = "r1bqk1nr/pppp1ppp/2n5/2b1p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4";
    let (status, body) = get(port, &url(before));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""uci":"e1g1","san":"O-O""#), "{body}");
    assert!(body.contains(r#""topGames":[{"number":4,"kind":"game","white":"","whiteElo":1500,"black":"","blackElo":1500,"result":"1-0","moves":0,"eco":"","event":"","site":"","date":"2003.07.??","round":"","annotator":"","flags":{"deleted":false,"chess960":false},"year":2003}]"#), "{body}");
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
    let body = answered(port, &url(start));
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
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The `number` a row or a notable game begins with.
fn number_of(object: &str) -> u32 {
    let digits = object.strip_prefix(r#"{"number":"#).unwrap_or_else(|| panic!("no number first in {object}"));
    digits[..digits.find(',').unwrap()].parse().unwrap()
}

/// Every notable game is its `/games` row whole, then `year` for clients
/// written before rows (#144). In a 2CBH, a classic and a PGN database, each
/// `topGames` entry is its number's row, member for member and value for
/// value, plus `year`: the year of the row's `date`, `null` when it has none.
#[test]
fn every_notable_game_is_its_games_row_and_its_year() {
    // A game dated without a year, beside the fixture's.
    let extra = [
        "11 | game | Tal, Mikhail | Morphy, Paul | Riga Club Ch | ????.??.?? | 3 | 1-0 | B20 | 20 | 2400 | 0 | Nimzowitsch, Aron | blitz",
    ];
    let two = common::fixture("explorer-rows-2cbh", &extra);
    let classic = common::classic_fixture("explorer-rows-cbh", &extra);
    // A PGN file holds games only: the guiding text and the deleted game are games there.
    let pgn_rows: Vec<String> = common::rows(&extra)
        .into_iter()
        .map(|line| {
            let mut f: Vec<&str> = line.split('|').map(str::trim).collect();
            f[1] = "game";
            f.join(" | ")
        })
        .collect();
    let pgn = common::pgn_fixture("explorer-rows-pgn", &pgn_rows);
    let paths = [two.dir().join("db.2cbh"), classic.dir().join("db.cbh"), pgn.dir().join("db.pgn")];
    let dir = index_dir("rows");
    let bridge = TestBridge::in_dir(app_of(paths.clone()), &dir);
    let port = bridge.port;
    let start = fen_param("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1");
    // Games 1-7, 10 and 11 are indexed, and in the PGN file 8 and 9 as well.
    for (path, indexed) in paths.iter().zip([9, 9, 11]) {
        let id = id_of(path);
        let format = path.extension().unwrap().to_str().unwrap();
        // A PGN file is opened, and each index built, in the background.
        let answer = answered(port, &format!("/v1/databases/{id}/explorer?fen={start}"));
        let list = answered(port, &format!("/v1/databases/{id}/games?limit=500"));
        let rows = objects(&list, "rows");
        let top = objects(&answer, "topGames");
        assert_eq!(top.len(), indexed, "{format}: {answer}");
        for entry in &top {
            let row = rows.iter().find(|r| number_of(r) == number_of(entry)).unwrap();
            let date = &row[row.find(r#""date":""#).unwrap() + 8..][..10];
            let year = date[..4].parse::<u16>().map_or("null".to_string(), |y| y.to_string());
            assert_eq!(*entry, format!("{},\"year\":{year}}}", &row[..row.len() - 1]), "{format}");
        }
        let year = |n: u32| {
            let entry = top.iter().find(|e| number_of(e) == n).unwrap();
            entry[entry.rfind(r#""year":"#).unwrap() + 7..entry.len() - 1].to_string()
        };
        assert_eq!([year(1), year(7), year(11)], ["1858", "1951", "null"], "{format}: {answer}");
        let eleven = top.iter().find(|e| number_of(e) == 11).unwrap();
        assert!(eleven.contains(r#""white":"Tal, Mikhail","whiteElo":2400,"black":"Morphy, Paul""#), "{eleven}");
        assert!(eleven.contains(r#""event":"Riga Club Ch","site":"","date":"????.??.??","round":"3""#), "{eleven}");
        assert!(eleven.contains(r#""annotator":"Nimzowitsch, Aron""#), "{eleven}");
    }
    drop(bridge);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A bridge started again answers from the first request with the index the
/// one before it kept on disk, and leaves the file as it was.
#[test]
fn a_restarted_bridge_answers_from_the_kept_index() {
    let db = database("explorer-restart");
    let dir = index_dir("restart");
    let start = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    let (first, id) = TestBridge::database(&db, &dir);
    let url = format!("/v1/databases/{id}/explorer?fen={}", fen_param(start));
    answered(first.port, &url);
    let file = dir.join("index").join(format!("{id}.idx"));
    let written = std::fs::metadata(&file).unwrap().modified().unwrap();
    drop(first);
    let (bridge, _) = TestBridge::database(&db, &dir);
    let (status, body) = get(bridge.port, &url);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""games":5,"white":2,"draws":2,"black":1"#), "{body}");
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), written, "the file was rewritten");
    drop(bridge);
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
    drop(idx);
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
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Knights out and back, `n` times: plies of play that keep one structure.
fn hops(n: usize) -> String {
    "g1f3 g8f6 f3g1 f6g8 ".repeat(n)
}

/// Every position of every game is found, at any depth (#133): one game alone
/// just past the tree's depth, one far past it, one after captures that
/// leave the pawns as they were, two games reaching one position by different
/// move orders; and a position of the same structure that no game reached is
/// not.
#[test]
fn every_position_of_every_game_is_found_at_any_depth() {
    let line7 = "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7";
    let long = format!("e2e4 e7e5 {}d2d3", hops(15));
    let one = format!("e2e4 e7e5 {}a2a3 a7a6 h2h3", hops(12));
    let other = format!("e2e4 e7e5 {}h2h3 a7a6 a2a3", hops(12));
    let mut b = Builder::new();
    game(&mut b, &format!("{line7} e1g1 d5c3 c1c3"), 1, (2200, 2200));
    game(&mut b, &long, 2, (2500, 2400));
    game(&mut b, &format!("{one} g8f6"), 1, (2300, 2300));
    game(&mut b, &other, 0, (2100, 2000));
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-deep");
    let dir = index_dir("deep");
    let idx = prepared(&db, &dir);
    let find = |ucis: &str| {
        let board = board_after(ucis);
        assert!(idx.lookup(board.hash()).unwrap().is_none(), "the tree does not hold it: {ucis}");
        explorer::deep_stats(&idx, &board, &Cancel::never()).unwrap()
    };

    // Ply 21, reached by game 1 alone, the first past the tree.
    let alone = find(&format!("{line7} e1g1")).unwrap();
    assert_eq!(alone.counts, Counts { games: 1, white: 0, draws: 1, black: 0 });
    assert_eq!(alone.lookup_move("d5c3"), Some(1));
    assert_eq!(alone.top, vec![1]);

    // Two captures that leave the pawns as they were: each is a structure of
    // its own, and game 1's last position is found in the last one.
    let traded = find(&format!("{line7} e1g1 d5c3 c1c3")).unwrap();
    assert_eq!(traded.counts, Counts { games: 1, white: 0, draws: 1, black: 0 });
    assert!(traded.moves.is_empty());

    // Ply 63, past the tree's depth: the game's last position, no move from it.
    let deep = find(&long).unwrap();
    assert_eq!(deep.counts, Counts { games: 1, white: 1, draws: 0, black: 0 });
    assert!(deep.moves.is_empty());

    // Ply 53 by two move orders: both games, the move each played from it, the
    // better rated first.
    let both = find(&one).unwrap();
    assert_eq!(both.counts, Counts { games: 2, white: 0, draws: 1, black: 1 });
    assert_eq!(both.lookup_move("g8f6"), Some(1));
    assert_eq!(both.moves.len(), 1, "game 4 ends there");
    assert_eq!(both.top, vec![3, 4]);
    assert_eq!(find(&other).unwrap(), both, "one position, whichever order reached it");

    // The same pawns with the knights elsewhere: games 3 and 4 share its
    // bucket, and neither reaches it.
    assert_eq!(find(&format!("{one} g8f6 g1f3")), None);

    // Through the endpoint, as the analysis panel asks for it.
    let (bridge, id) = TestBridge::database(&db, &dir);
    let port = bridge.port;
    let fen = board_after(&one).fen();
    let url = format!("/v1/databases/{id}/explorer?fen={}", fen_param(&fen));
    let body = answered(port, &url);
    assert!(
        body.contains(r#""games":2,"white":0,"draws":1,"black":1,"moves":[{"uci":"g8f6","san":"Nf6","games":1"#),
        "{body}"
    );
    drop((idx, bridge));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A bucket of many games is replayed on several workers, and every game in
/// it is counted once, whichever worker replayed it.
#[test]
fn a_crowded_bucket_is_counted_whole() {
    let long = format!("e2e4 e7e5 {}d2d3", hops(15));
    let n = 700;
    let mut b = Builder::new();
    for i in 0..n {
        game(&mut b, &long, (i % 3) as u8, (2000 + i as i16, 2000));
    }
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-crowded");
    let dir = index_dir("crowded");
    let idx = prepared(&db, &dir);
    let board = board_after(&long);
    assert!(idx.lookup(board.hash()).unwrap().is_none(), "past the tree's depth");
    let stats = explorer::deep_stats(&idx, &board, &Cancel::never()).unwrap().unwrap();
    assert_eq!(stats.counts.games, n as u64);
    assert_eq!(stats.counts.white + stats.counts.draws + stats.counts.black, n as u64);
    // The best rated first: the last games written.
    assert_eq!(stats.top, (n as u32 - 11..=n as u32).rev().collect::<Vec<_>>());
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The tree's last ply lists the moves played from it, as every other does.
#[test]
fn the_moves_from_the_trees_last_ply_are_listed() {
    let last = format!("e2e4 e7e5 {}a2a3 a7a6", hops(4));
    assert_eq!(last.split_whitespace().count(), usize::from(explorer::format::MAX_PLY));
    let mut b = Builder::new();
    game(&mut b, &format!("{last} h2h3"), 1, (2200, 2200));
    game(&mut b, &format!("{last} g2g3"), 0, (2100, 2100));
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-last-ply");
    let dir = index_dir("last-ply");
    let idx = prepared(&db, &dir);
    let stats = idx.lookup(key_after(&last)).unwrap().expect("two games reach it");
    assert_eq!(stats.counts.games, 2);
    assert_eq!(stats.lookup_move("h2h3"), Some(1));
    assert_eq!(stats.lookup_move("g2g3"), Some(1));
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A classic set-up game is replayed only as far as the position looked for:
/// its start is resolved once, and a position whose kings and rooks could
/// never castle is not replayed to look for a right first. A position the
/// games reach early is then found far faster than one they never reach,
/// which every game is played to its end for.
#[test]
fn a_classic_set_up_game_is_replayed_only_as_far_as_the_position() {
    let start = Board::from_fen("7k/8/8/8/8/8/P7/K7 w - - 0 1").unwrap();
    let mut ucis = "a1b1 h8g8 b1a1 g8h8 ".repeat(10);
    ucis.push_str("a2a3 h8g8 ");
    ucis.push_str(&"a1b1 g8h8 b1a1 h8g8 ".repeat(5_000));
    let toks: Vec<Tok<'_>> = ucis.split_whitespace().map(Tok::Mv).chain([Tok::End]).collect();
    let stream = encode(&start, &toks, 0, false);
    let white = [("a1", CPiece::King, CColor::White), ("a2", CPiece::Pawn, CColor::White)];
    let position = start_position(&[white[0], white[1], ("h8", CPiece::King, CColor::Black)], false, 0, 0);
    let mut b = fixture_cbh::Builder::new();
    for _ in 0..16 {
        b.game(&move_record(0x40, Some(&position), None, &stream))[0x1b] = 1;
    }
    let db = b.write("explorer-classic-setup");
    let dir = index_dir("classic-setup");
    let base = cbh::Database::open(db.dir().join("db.cbh")).unwrap();
    let idx = explorer::prepare(&base, 1, &dir, "db", &Progress::default()).unwrap();
    // Ply 41, after a2a3, beyond the tree; and the same men never so placed.
    let mut reached = start.clone();
    for uci in ucis.split_whitespace().take(41) {
        reached.play_checked(uci.parse().unwrap()).unwrap();
    }
    let never = Board::from_fen("k7/8/8/8/8/P7/8/7K b - - 0 1").unwrap();
    // The fastest of `runs` lookups: a loaded machine holds back some of
    // them, not all. The quick one is timed many times; holding back the
    // slow one only makes it slower.
    let time = |board: &Board, runs: usize| {
        (0..runs)
            .map(|_| {
                let at = Instant::now();
                let stats = explorer::deep_stats(&idx, board, &Cancel::never()).unwrap();
                (at.elapsed(), stats.map(|s| s.counts.games))
            })
            .min()
            .unwrap()
    };
    let (found, games) = time(&reached, 30);
    assert_eq!(games, Some(16));
    let (missed, none) = time(&never, 3);
    assert_eq!(none, None);
    assert!(found * 5 < missed, "found in {found:?}, missed in {missed:?}");
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The words of `ucis` from the start, or from `fen`; "--" is a null move.
fn stream_words(fen: Option<&str>, ucis: &str) -> Vec<u16> {
    let mut board = fen.map_or_else(Board::startpos, |f| Board::from_fen(f).unwrap());
    let mut out = Vec::new();
    for uci in ucis.split_whitespace() {
        if uci == "--" {
            out.push(movetable::NULL_MOVE);
            board = board.null_move().unwrap();
        } else {
            out.extend(words(&mut board, uci));
        }
    }
    out
}

/// The 2CBH move record of a game from `fen`, or from the standard start.
fn move_words(fen: Option<&str>, ucis: &str) -> Vec<u16> {
    let mut stream = Vec::new();
    if let Some(fen) = fen {
        let board = Board::from_fen(fen).unwrap();
        let castling = [(CColor::White, 1, 0), (CColor::White, 2, 7), (CColor::Black, 4, 0), (CColor::Black, 8, 7)]
            .iter()
            .filter(|(c, _, file)| {
                let side = if *file == 7 { chesscore::CastleSide::Short } else { chesscore::CastleSide::Long };
                board.castling_rook(*c, side).is_some()
            })
            .fold(0u16, |bits, (_, bit, _)| bits | bit);
        let side = u16::from(board.side_to_move() == CColor::Black);
        stream.extend([movetable::START_POSITION, 1, side | castling << 8, 0]);
        for i in 0..64u8 {
            let sq = chesscore::Square::from_index(i).unwrap();
            if let Some((p, c)) = board.piece_at(sq) {
                let color = if c == CColor::White { Color::White } else { Color::Black };
                let piece = match p {
                    CPiece::Pawn => Piece::Pawn,
                    CPiece::Knight => Piece::Knight,
                    CPiece::Bishop => Piece::Bishop,
                    CPiece::Rook => Piece::Rook,
                    CPiece::Queen => Piece::Queen,
                    CPiece::King => Piece::King,
                };
                stream.push(movetable::encode_piece_word(color, piece, i).unwrap());
            }
        }
    }
    stream.push(MOVES);
    stream.extend(stream_words(fen, ucis));
    stream.push(END_OF_LINE);
    stream
}

/// Game 7 of [`database`]: 30 plies, past its prefix slot.
const LINE_30: &str = "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7 e1g1 d5c3 c1c3 e6e5 d1c2 e5e4 f3d2 d7f6 f1e1 c8f5";

/// The move stream (#145) holds every main-line word of each game the index
/// holds, as the database stores it, in the prefix slot and past it in the
/// tail, with the game's result and rating; the games the index leaves out,
/// deleted or Chess960, hold none.
#[test]
fn the_stream_holds_every_main_line_word() {
    let db = database("explorer-stream-words");
    let dir = index_dir("stream-words");
    let idx = prepared(&db, &dir);
    let lines = [
        Some("e2e4 e7e5 g1f3 b8c6"),
        Some("e2e4 e7e5 g1f3 b8c6 f1b5"),
        Some("g1f3 b8c6 e2e4 e7e5"),
        Some("e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1"),
        None,
        None,
        Some(LINE_30),
    ];
    let mut plies = 0;
    for (n, line) in (1..).zip(lines) {
        let game = idx.stream.game(n).unwrap();
        assert_eq!(game.start, None);
        match line {
            Some(ucis) => {
                assert!(game.entry.indexed(), "game {n}");
                assert_eq!(game.words, stream_words(None, ucis), "game {n}");
                plies += game.words.len() as u64;
            }
            None => assert!(!game.entry.indexed() && game.words.is_empty(), "game {n}"),
        }
    }
    let first = idx.stream.entry(1).unwrap();
    assert_eq!((first.elo(), first.outcome()), (2350, explorer::format::Outcome::White));
    assert_eq!(idx.stream.entry(3).unwrap().elo(), 2100, "the one rating known");
    let h = idx.stream.header;
    assert_eq!((h.first_record, h.last_record, h.games, h.plies), (1, 7, 5, plies));
    assert_eq!((h.generation, h.build_id), (idx.generation, idx.base.header.build_id));
    drop(idx);
    // The deep fixture's long lines, each past its prefix slot.
    let long = format!("e2e4 e7e5 {}d2d3", hops(15));
    let mut b = Builder::new();
    game(&mut b, &long, 2, (2500, 2400));
    game(&mut b, &format!("{}a2a3", hops(20)), 1, (2300, 2300));
    let deep = b.write("explorer-stream-words-long");
    let deep_dir = index_dir("stream-words-long");
    let idx = prepared(&deep, &deep_dir);
    assert_eq!(idx.stream.game(1).unwrap().words, stream_words(None, &long));
    assert_eq!(idx.stream.game(2).unwrap().words.len(), 81);
    drop(idx);
    for dir in [dir, deep_dir] {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// A set-up game's start is stored in its tail and replayed from there: a
/// position of it past the tree is found, although its start lacks the home
/// pawns a standard game would have to lose first.
#[test]
fn a_set_up_start_is_stored_and_replayed() {
    let fen = "4k3/8/8/8/8/8/P7/4K3 w - - 0 1";
    let ucis = format!("{}a2a4 e8d8 e1f2", "e1d1 e8d8 d1e1 d8e8 ".repeat(6));
    let mut b = Builder::new();
    let at = b.moves(1, &move_words(Some(fen), &ucis));
    b.game(at)[0x58] = 1;
    let db = b.write("explorer-stream-setup");
    let dir = index_dir("stream-setup");
    let idx = prepared(&db, &dir);
    let game = idx.stream.game(1).unwrap();
    assert!(game.entry.indexed() && game.entry.setup());
    let start = Board::from_fen(fen).unwrap();
    assert_eq!(game.start.as_ref().map(Board::hash), Some(start.hash()));
    assert_eq!(game.words, stream_words(Some(fen), &ucis));
    // Ply 25, after 13. a4, and the move played from it.
    let mut board = start;
    for uci in ucis.split_whitespace().take(25) {
        board.play_checked(uci.parse().unwrap()).unwrap();
    }
    assert!(idx.lookup(board.hash()).unwrap().is_none(), "past the tree");
    let found = explorer::deep_stats(&idx, &board, &Cancel::never()).unwrap().unwrap();
    assert_eq!(found.counts, Counts { games: 1, white: 0, draws: 1, black: 0 });
    assert_eq!(found.lookup_move("e8d8"), Some(1));
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A null move ends a line in the stream as in the tree, and so does damage:
/// the words before it are kept, and nothing after it.
#[test]
fn a_null_move_and_damage_end_a_line() {
    let mut b = Builder::new();
    let at = b.moves(1, &move_words(None, "e2e4 e7e5 -- b8c6"));
    b.game(at);
    let mut damaged = move_words(None, "d2d4 d7d5");
    // A white queen from h5, where none stands, then a move that would be legal.
    damaged.insert(damaged.len() - 1, quiet(Color::White, Piece::Queen, "h5", "h6"));
    damaged.insert(damaged.len() - 1, stream_words(Some(&board_after("d2d4 d7d5").fen()), "c2c4")[0]);
    let at = b.moves(1, &damaged);
    b.game(at);
    let db = b.write("explorer-stream-ends");
    let dir = index_dir("stream-ends");
    let idx = prepared(&db, &dir);
    assert_eq!(idx.stream.game(1).unwrap().words, stream_words(None, "e2e4 e7e5"));
    assert_eq!(idx.stream.game(2).unwrap().words, stream_words(None, "d2d4 d7d5"));
    assert_eq!(idx.stream.entry(2).unwrap().plies, 2);
    for (ucis, key) in [("e2e4 e7e5", key_after("e2e4 e7e5")), ("d2d4 d7d5", key_after("d2d4 d7d5"))] {
        let stats = idx.lookup(key).unwrap().unwrap();
        assert!(stats.moves.is_empty(), "no move from the last position of {ucis}");
    }
    assert_eq!(idx.stream.header.plies, 4);
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A database without records, and one whose only game the index leaves
/// out, build an index of nothing, which answers every position with no
/// games.
#[test]
fn a_database_without_indexed_games_builds_an_empty_index() {
    let mut chess960 = Builder::new();
    let mut board = Board::startpos();
    let mut stream = vec![movetable::START_POSITION, 518, MOVES];
    stream.extend(words(&mut board, "e2e4"));
    stream.push(END_OF_LINE);
    let at = chess960.moves(2, &stream);
    chess960.game(at);
    for (name, b) in [("empty", Builder::new()), ("chess960-only", chess960)] {
        let db = b.write(&format!("explorer-{name}"));
        let dir = index_dir(name);
        let idx = prepared(&db, &dir);
        assert_eq!((idx.games(), idx.base.header.keys, idx.base.header.blocks), (0, 0, 0), "{name}");
        let start = Board::startpos();
        assert_eq!(explorer::stats(&idx, &start, &Cancel::never()).unwrap(), None, "{name}");
        drop(idx);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// A line longer than the stream keeps ends at its 65,535th ply, in the
/// stream as in the index, whose passes replay it to there: its last
/// position, past the tree, is found with no move from it.
#[test]
fn the_longest_line_is_replayed_to_its_end() {
    // Knights out and back to ply 65,532, then 1.d4 Nf6 2.Bf4: the line's
    // last position, at ply 65,535; 2...e6 is not kept.
    let ucis = format!("{}d2d4 g8f6 c1f4 e7e6", hops(16_383));
    let mut b = Builder::new();
    game(&mut b, &ucis, 2, (2200, 2200));
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-longest-line");
    let dir = index_dir("longest-line");
    let idx = prepared(&db, &dir);
    assert_eq!(idx.stream.entry(1).unwrap().plies, u16::MAX);
    let last = board_after(&format!("{}d2d4 g8f6 c1f4", hops(16_383)));
    assert!(idx.lookup(last.hash()).unwrap().is_none(), "past the tree");
    let found = explorer::deep_stats(&idx, &last, &Cancel::never()).unwrap().unwrap();
    assert_eq!(found.counts, Counts { games: 1, white: 1, draws: 0, black: 0 });
    assert!(found.moves.is_empty(), "the line ends there");
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A game in the three formats: its start (the standard one without), its
/// moves in UCI with "--" for a null move, its result (0 black, 1 draw, 2
/// white) and ratings.
struct Same {
    fen: Option<&'static str>,
    ucis: String,
    result: u8,
    elo: (u16, u16),
}

fn same_games() -> Vec<Same> {
    let game = |fen, ucis: &str, result, elo| Same { fen, ucis: ucis.to_string(), result, elo };
    vec![
        game(None, "e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1 g8f6 d2d3 e8g8", 2, (2400, 2300)),
        game(None, LINE_30, 1, (2200, 0)),
        game(None, "e2e4 a7a6 e4e5 d7d5 e5d6 c7d6", 0, (0, 1800)),
        game(None, "a2a4 b7b5 a4b5 a7a6 b5a6 c8b7 a6b7 b8c6 b7a8q d8a8", 2, (2600, 2650)),
        game(Some("4k3/8/8/8/8/8/P7/4K3 w - - 0 1"), "a2a4 e8d8 a4a5", 1, (2000, 2000)),
        game(Some("r3k2r/8/8/8/8/8/8/R3K2R b KQkq - 0 1"), "e8c8 e1g1 d8d1 f1d1", 0, (1900, 2100)),
        game(None, "d2d4 -- c2c4 d7d5", 1, (2100, 2100)),
        game(None, &format!("{}h2h3", hops(8)), 1, (1500, 1600)),
    ]
}

/// The PGN text of `games`.
fn same_pgn(games: &[Same]) -> String {
    let mut text = String::new();
    for (n, g) in games.iter().enumerate() {
        let result = ["0-1", "1/2-1/2", "1-0"][usize::from(g.result)];
        text.push_str(&format!("[Event \"{n}\"]\n[Result \"{result}\"]\n"));
        for (tag, elo) in [("WhiteElo", g.elo.0), ("BlackElo", g.elo.1)] {
            if elo > 0 {
                text.push_str(&format!("[{tag} \"{elo}\"]\n"));
            }
        }
        if let Some(fen) = g.fen {
            text.push_str(&format!("[SetUp \"1\"]\n[FEN \"{fen}\"]\n"));
        }
        text.push('\n');
        let mut board = g.fen.map_or_else(Board::startpos, |f| Board::from_fen(f).unwrap());
        for (i, uci) in g.ucis.split_whitespace().enumerate() {
            let white = board.side_to_move() == CColor::White;
            if white || i == 0 {
                text.push_str(&format!("{}{} ", board.fullmove_number(), if white { "." } else { "..." }));
            }
            if uci == "--" {
                text.push_str("-- ");
                board = board.null_move().unwrap();
                continue;
            }
            let mut mv: Move = uci.parse().unwrap();
            if board.piece_at(mv.from).map(|p| p.0) == Some(CPiece::King) && mv.from.file().abs_diff(mv.to.file()) == 2
            {
                mv.to = chesscore::Square::new(if mv.to.file() == 6 { 7 } else { 0 }, mv.from.rank());
            }
            text.push_str(&format!("{} ", cbformat::pgn::san(&board, mv)));
            board.play_checked(mv).unwrap();
        }
        text.push_str(&format!("{result}\n\n"));
    }
    text
}

/// The classic record of `g`.
fn same_classic(b: &mut fixture_cbh::Builder, g: &Same) {
    let start = g.fen.map_or_else(Board::startpos, |f| Board::from_fen(f).unwrap());
    let mut board = start.clone();
    let mut toks = Vec::new();
    for uci in g.ucis.split_whitespace() {
        if uci == "--" {
            toks.push("--".to_string());
            board = board.null_move().unwrap();
            continue;
        }
        let mut mv: Move = uci.parse().unwrap();
        let castles =
            board.piece_at(mv.from).map(|p| p.0) == Some(CPiece::King) && mv.from.file().abs_diff(mv.to.file()) == 2;
        toks.push(match castles {
            true if mv.to.file() == 6 => "O-O".to_string(),
            true => "O-O-O".to_string(),
            false => uci.to_string(),
        });
        if castles {
            mv.to = chesscore::Square::new(if mv.to.file() == 6 { 7 } else { 0 }, mv.from.rank());
        }
        board.play_checked(mv).unwrap();
    }
    let mut stream: Vec<Tok<'_>> = toks.iter().map(|t| Tok::Mv(t)).collect();
    stream.push(Tok::End);
    let moves = encode(&start, &stream, 0, false);
    let record = match g.fen {
        None => move_record(0, None, None, &moves),
        Some(_) => {
            let names: Vec<String> = (0..64u8).map(|i| format!("{}{}", (b'a' + i % 8) as char, i / 8 + 1)).collect();
            let pieces: Vec<(&str, CPiece, CColor)> = names
                .iter()
                .filter_map(|n| start.piece_at(n.parse().unwrap()).map(|(p, c)| (n.as_str(), p, c)))
                .collect();
            let castling = [(CColor::White, 2, 0), (CColor::White, 1, 7), (CColor::Black, 8, 0), (CColor::Black, 4, 7)]
                .iter()
                .filter(|(c, _, file)| {
                    let side = if *file == 7 { chesscore::CastleSide::Short } else { chesscore::CastleSide::Long };
                    start.castling_rook(*c, side).is_some()
                })
                .fold(0u8, |bits, (_, bit, _)| bits | bit);
            let position = start_position(&pieces, start.side_to_move() == CColor::Black, castling, 0);
            move_record(0x40, Some(&position), None, &moves)
        }
    };
    let rec = b.game(&record);
    rec[0x1b] = g.result;
    rec[0x1f..0x21].copy_from_slice(&g.elo.0.to_be_bytes());
    rec[0x21..0x23].copy_from_slice(&g.elo.1.to_be_bytes());
}

/// 2CBH, classic and PGN copies of the same games give the same stream, byte
/// for byte past the generation and the build id: one format for all three,
/// set-up starts, castling, en passant, promotions and null moves included.
#[test]
fn every_format_gives_the_same_stream() {
    let games = same_games();
    let mut b = Builder::new();
    for g in &games {
        let at = b.moves(1, &move_words(g.fen, &g.ucis));
        let rec = b.game(at);
        rec[0x58] = g.result;
        rec[0x60..0x62].copy_from_slice(&(g.elo.0 as i16).to_le_bytes());
        rec[0x70..0x72].copy_from_slice(&(g.elo.1 as i16).to_le_bytes());
    }
    let two = b.write("explorer-stream-2cbh");
    let mut c = fixture_cbh::Builder::new();
    for g in &games {
        same_classic(&mut c, g);
    }
    let classic = c.write("explorer-stream-cbh");
    let pgn = cbformat::fixture::pgn_file("explorer-stream-pgn", same_pgn(&games).as_bytes());
    let (pgn_path, pgn_index) = (pgn.dir().join("db.pgn"), pgn.dir().join("db.head"));
    let page = cbformat::codepage::CodePage::WESTERN;
    cbformat::pgnfile::build(&pgn_path, &pgn_index, 1, page, &mut |_| true).unwrap();
    let pgn_db = cbformat::pgnfile::Database::open(&pgn_path, &pgn_index, 1, page).unwrap();
    let dirs = [index_dir("stream-2cbh"), index_dir("stream-cbh"), index_dir("stream-pgn")];
    let built = [
        explorer::prepare(&Database::open(two.dir().join("db.2cbh")).unwrap(), 1, &dirs[0], "db", &Progress::default()),
        explorer::prepare(
            &cbh::Database::open(classic.dir().join("db.cbh")).unwrap(),
            2,
            &dirs[1],
            "db",
            &Progress::default(),
        ),
        explorer::prepare(&pgn_db, 3, &dirs[2], "db", &Progress::default()),
    ]
    .map(Result::unwrap);
    let two_cbh = &built[0];
    assert_eq!(two_cbh.stream.header.games, games.len() as u64);
    // Each game's line is as written, to its null move.
    for (n, g) in (1..).zip(&games) {
        let ucis = g.ucis.split(" --").next().unwrap();
        assert_eq!(two_cbh.stream.game(n).unwrap().words, stream_words(g.fen, ucis), "game {n}");
        assert_eq!(two_cbh.stream.entry(n).unwrap().setup(), g.fen.is_some(), "game {n}");
    }
    let bytes = |l: &Loaded| std::fs::read(&l.stream.path).unwrap();
    let reference = bytes(two_cbh);
    for (l, format) in built[1..].iter().zip(["classic", "PGN"]) {
        let other = bytes(l);
        assert_eq!(l.stream.header.build_id, l.base.header.build_id);
        assert_eq!(other[48..124], reference[48..124], "the {format} header's counts and layout");
        assert_eq!(other[128..], reference[128..], "the {format} stream");
    }
    drop(built);
    for dir in dirs {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The games of [`every_position_of_every_game_is_found_at_any_depth`]: one
/// alone just past the tree's depth, and one far past it.
fn deep_games(name: &str) -> TempDb {
    let line7 = "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7";
    let mut b = Builder::new();
    game(&mut b, &format!("{line7} e1g1 d5c3 c1c3"), 1, (2200, 2200));
    game(&mut b, &format!("e2e4 e7e5 {}d2d3", hops(15)), 2, (2500, 2400));
    b.lid(lid_header(1024, 1));
    b.write(name)
}

/// A stream record that fails its CRC, in its slot or in its tail, is found
/// when a replay reads it: nothing is answered from it, the answer is `409`
/// while both files are built again, and the next answers come from the new
/// build.
#[test]
fn a_stream_record_that_fails_its_crc_is_rebuilt() {
    let db = deep_games("explorer-stream-crc");
    let dir = index_dir("stream-crc");
    let (bridge, id) = TestBridge::database(&db, &dir);
    let ucis = format!("e2e4 e7e5 {}d2d3", hops(15));
    let url = format!("/v1/databases/{id}/explorer?fen={}", fen_param(&board_after(&ucis).fen()));
    let found = r#""games":1,"white":1,"draws":0,"black":0"#;
    assert!(answered(bridge.port, &url).contains(found));
    // Dropped, the bridge maps the stream no longer, which on Windows would
    // keep the next bridge from replacing it.
    drop(bridge);
    let path = dir.join("index").join(format!("{id}.moves"));
    for part in ["slot", "tail", "selection"] {
        let before = std::fs::read(&path).unwrap();
        let header = explorer::stream::Header::decode(&before).unwrap();
        // Game 2's slot, one slot into the only block, which the table at
        // the file's end places: a word of its prefix, or of its tail.
        let table = header.table_offset as usize;
        let block = u64::from_le_bytes(before[table..table + 8].try_into().unwrap()) as usize;
        let slot = block + explorer::stream::SLOT_BYTES;
        let at = match part {
            "slot" => slot + 16 + 6,
            "selection" => slot + 60,
            _ => 2 * u32::from_le_bytes(before[slot..slot + 4].try_into().unwrap()) as usize + 10,
        };
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(at as u64)).unwrap();
        file.write_all(&[before[at] ^ 0x5a]).unwrap();
        drop(file);
        // A bridge started now opens the files, whose header and table are
        // sound, and finds the damage on the first replay.
        let (bridge, _) = TestBridge::database(&db, &dir);
        let (status, body) = get(bridge.port, &url);
        assert_eq!(status, 409, "{part}: {body}");
        assert!(body.contains("rebuilt"), "{part}: {body}");
        assert!(answered(bridge.port, &url).contains(found), "{part}");
        let after = explorer::stream::Header::decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_ne!(after.build_id, header.build_id, "{part}: built again");
        let direct = {
            let loaded = explorer::prepare(
                &Database::open(db.dir().join("db.2cbh")).unwrap(),
                after.generation,
                &dir.join("index"),
                &id,
                &Progress::default(),
            )
            .unwrap();
            loaded.stream.header
        };
        assert_eq!(direct, after, "{part}: the new files are kept");
        // Dropped before the next part changes the stream it maps.
        drop(bridge);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Games of three blocks, which the build's workers write in any order:
/// every game's line reads back whole, and a position past the tree is found
/// in all the games of every block that reach it.
#[test]
fn a_stream_of_many_blocks_holds_every_game() {
    let games = 2 * explorer::stream::BATCH as u32 + 300;
    let mut b = Builder::new();
    let long = format!("e2e4 e7e5 {}d2d3", hops(15));
    for n in 1..=games {
        match n % 3 {
            0 => game(&mut b, &long, 2, (2000, 2000)),
            1 => game(&mut b, "d2d4 d7d5", 1, (2000, 2000)),
            _ => game(&mut b, "e2e4", 0, (2000, 2000)),
        };
    }
    let db = b.write("explorer-stream-blocks");
    let dir = index_dir("stream-blocks");
    let idx = prepared(&db, &dir);
    assert_eq!((idx.stream.header.games, idx.stream.header.blocks), (u64::from(games), 3));
    for n in 1..=games {
        let ucis = ["", "d2d4 d7d5", "e2e4"][n as usize % 3];
        let ucis = if ucis.is_empty() { long.as_str() } else { ucis };
        assert_eq!(idx.stream.game(n).unwrap().words, stream_words(None, ucis), "game {n}");
    }
    let found = explorer::deep_stats(&idx, &board_after(&long), &Cancel::never()).unwrap().unwrap();
    assert_eq!(found.counts.games, u64::from(games / 3));
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An index and a stream of different builds are never used together: either
/// file from another build of the same database and generation rebuilds both.
#[test]
fn files_from_different_builds_are_rebuilt() {
    let db = deep_games("explorer-stream-builds");
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let (dir, other) = (index_dir("stream-builds"), index_dir("stream-builds-other"));
    let ids = |dir: &Path| {
        let l = explorer::prepare(&d, 1, dir, "db", &Progress::default()).unwrap();
        (l.base.header.build_id, l.stream.header.build_id)
    };
    let (a, _) = ids(&dir);
    let (b, _) = ids(&other);
    assert_ne!(a, b);
    for name in ["db.moves", "db.idx"] {
        std::fs::copy(other.join(name), dir.join(name)).unwrap();
        let progress = Progress::default();
        let again = explorer::prepare(&d, 1, &dir, "db", &progress).unwrap();
        assert_eq!(progress.phase(), "structures", "{name} of another build: built again");
        assert_eq!(again.stream.header.build_id, again.base.header.build_id);
        let alone =
            explorer::deep_stats(&again, &board_after(&format!("e2e4 e7e5 {}d2d3", hops(15))), &Cancel::never());
        assert_eq!(alone.unwrap().unwrap().counts.games, 1);
    }
    // A stream missing: both are built again.
    std::fs::remove_file(dir.join("db.moves")).unwrap();
    let progress = Progress::default();
    drop(explorer::prepare(&d, 1, &dir, "db", &progress).unwrap());
    assert_eq!(progress.phase(), "structures");
    // Both as built: used as they are.
    let progress = Progress::default();
    drop(explorer::prepare(&d, 1, &dir, "db", &progress).unwrap());
    assert_eq!(progress.phase(), "checking");
    for dir in [dir, other] {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// A line past the tree only ever loses men, pawns and home pawns, so those
/// alone end a replay early: the pieces of each kind do not, since a
/// promotion adds one. A queen taken and a pawn promoted to a queen later, a
/// third knight and a second bishop on the same squares are each found.
#[test]
fn positions_after_a_promotion_are_found() {
    // 1.e4 e5 2.Qh5 Nc6 3.Qxf7+ Kxf7 4.a4 b5 5.axb5 a6 6.bxa6 Bb7 7.axb7 Nf6
    // 8.bxa8, from ply 8.
    let before = format!("{}e2e4 e7e5 d1h5 b8c6 h5f7 e8f7 a2a4 b7b5 a4b5 a7a6 b5a6 c8b7 a6b7 g8f6", hops(2));
    let promotions = ["b7a8q", "b7a8n", "b7a8b"];
    let mut b = Builder::new();
    for (i, p) in promotions.iter().enumerate() {
        game(&mut b, &format!("{before} {p} f8e7"), i as u8, (2000 + i as i16, 2000));
    }
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-stream-promotions");
    let dir = index_dir("stream-promotions");
    let idx = prepared(&db, &dir);
    for (i, p) in promotions.iter().enumerate() {
        let board = board_after(&format!("{before} {p}"));
        assert!(idx.lookup(board.hash()).unwrap().is_none(), "past the tree: {p}");
        let found =
            explorer::deep_stats(&idx, &board, &Cancel::never()).unwrap().unwrap_or_else(|| panic!("{p} not found"));
        assert_eq!((found.counts.games, found.top.clone()), (1, vec![i as u32 + 1]), "{p}");
        assert_eq!(found.lookup_move("f8e7"), Some(1), "{p}");
    }
    let queens = board_after(&format!("{before} b7a8q"));
    assert_eq!(queens.colored(CPiece::Queen, CColor::White).count_ones(), 1, "the queen taken, then a new one");
    let knights = board_after(&format!("{before} b7a8n"));
    assert_eq!(knights.colored(CPiece::Knight, CColor::White).count_ones(), 3);
    let bishops = board_after(&format!("{before} b7a8b")).colored(CPiece::Bishop, CColor::White);
    const LIGHT: u64 = 0x55aa_55aa_55aa_55aa;
    assert_eq!((bishops & LIGHT).count_ones(), 2, "two bishops on light squares");
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A replay whose request was superseded stops at its next game, and is
/// answered as busy.
#[test]
fn a_superseded_replay_stops() {
    let long = format!("e2e4 e7e5 {}d2d3", hops(15));
    let mut b = Builder::new();
    for _ in 0..300 {
        game(&mut b, &long, 1, (2000, 2000));
    }
    let db = b.write("explorer-stream-cancel");
    let dir = index_dir("stream-cancel");
    let idx = prepared(&db, &dir);
    let latest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let old = Cancel::newest(&latest);
    let board = board_after(&long);
    assert_eq!(explorer::deep_stats(&idx, &board, &old).unwrap().unwrap().counts.games, 300);
    let _newer = Cancel::newest(&latest);
    assert!(matches!(explorer::deep_stats(&idx, &board, &old), Err(Bad::Busy)));
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// On Windows a mapped file cannot be replaced: a rebuild waits for an answer
/// still in flight with the old stream mapped, which reads it unchanged
/// meanwhile, then replaces it.
#[cfg(windows)]
#[test]
fn a_rebuild_replaces_a_stream_still_mapped_by_an_answer_in_flight() {
    let dir = index_dir("stream-mapped");
    let db = five("explorer-stream-mapped", "e2e4", &[]);
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let old = explorer::prepare(&d, 1, &dir, "db", &Progress::default()).unwrap();
    drop(d);
    let (started, rebuilding) = std::sync::mpsc::channel::<()>();
    let answer = std::thread::spawn(move || {
        // An answer that reads the old stream throughout the rebuild.
        rebuilding.recv().unwrap();
        let until = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < until {
            assert_eq!(old.stream.game(1).unwrap().words, stream_words(None, "e2e4 e7e5"));
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(old);
    });
    let db2 = five("explorer-stream-mapped", "d2d4", &[]);
    let d = Database::open(db2.dir().join("db.2cbh")).unwrap();
    started.send(()).unwrap();
    let new = explorer::prepare(&d, 2, &dir, "db", &Progress::default()).unwrap();
    answer.join().unwrap();
    assert_eq!((new.generation, new.stream.header.generation), (2, 2));
    assert_eq!(new.stream.header.build_id, new.base.header.build_id);
    let start = new.lookup(key_after("")).unwrap().unwrap();
    assert_eq!((start.lookup_move("e2e4"), start.lookup_move("d2d4")), (Some(4), Some(1)));
    assert_eq!(new.stream.game(1).unwrap().words, stream_words(None, "d2d4 e7e5"));
    assert!(!dir.join("db.moves.partial").exists());
    drop(new);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The moves of `stats` as codes with their games, in their order.
fn moves_of(stats: &bridge::explorer::format::Stats) -> Vec<(u16, u64)> {
    stats.moves.iter().map(|m| (m.0, m.1.games)).collect()
}

fn code(uci: &str) -> u16 {
    pack_move(uci.parse().unwrap())
}

/// A tree position that other games reach only beyond the tree's plies
/// counts them too, each with the move it played from there (#146): counts
/// add, moves add by code, most played first, then by code, and the notable
/// games are the best of both. So does one deeper in the tree, which two
/// games reach within it. The start, which the long games reach again and
/// again up to the tree's last ply, counts each game once.
#[test]
fn a_tree_position_counts_the_games_that_reach_it_only_beyond_the_tree() {
    let beyond = format!("{}e2e4 e7e5", hops(5));
    // After 1.d4 d5 and 4 hops, 6. a3 a6: first reached at ply 12.
    let late = format!("d2d4 d7d5 {}a2a3 a7a6", hops(2));
    let mut b = Builder::new();
    game(&mut b, "e2e4 e7e5 g1f3", 2, (2400, 2400));
    game(&mut b, "e2e4 e7e5 f1c4", 1, (2200, 2200));
    // Ply 22, after 20 plies of hops that never leave the start's structure.
    game(&mut b, &format!("{beyond} g1f3"), 0, (2500, 2500));
    game(&mut b, &format!("{beyond} d2d4"), 2, (2300, 2300));
    game(&mut b, &format!("{late} c1f4"), 1, (2000, 2000));
    game(&mut b, &format!("{late} c2c4"), 1, (2000, 2000));
    // Ply 24.
    game(&mut b, &format!("{}{late} e2e3", hops(3)), 2, (2100, 2100));
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-beyond");
    let dir = index_dir("beyond");
    let idx = prepared(&db, &dir);
    let board = board_after("e2e4 e7e5");
    let tree = idx.lookup(board.hash()).unwrap().unwrap();
    assert_eq!((tree.counts.games, tree.top.clone()), (2, vec![1, 2]), "games 1 and 2 reach it within the tree");
    let all = explorer::stats(&idx, &board, &Cancel::never()).unwrap().unwrap();
    assert_eq!(all.counts, Counts { games: 4, white: 2, draws: 1, black: 1 });
    assert_eq!(moves_of(&all), [(code("g1f3"), 2), (code("f1c4"), 1), (code("d2d4"), 1)]);
    assert_eq!(all.moves[0].1, Counts { games: 2, white: 1, draws: 0, black: 1 }, "game 1's and game 3's");
    assert_eq!(all.top, vec![3, 1, 4, 2]);

    // Two games reach it at ply 12; game 7 at ply 24.
    let board = board_after(&late);
    assert_eq!(idx.lookup(board.hash()).unwrap().unwrap().counts.games, 2);
    let all = explorer::stats(&idx, &board, &Cancel::never()).unwrap().unwrap();
    assert_eq!(all.counts, Counts { games: 3, white: 1, draws: 2, black: 0 });
    assert_eq!(moves_of(&all), [(code("e2e3"), 1), (code("c2c4"), 1), (code("c1f4"), 1)]);
    assert_eq!(all.top, vec![7, 6, 5]);

    // Every game starts at the start: the tree counted them all there.
    let start = Board::startpos();
    let tree = idx.lookup(start.hash()).unwrap();
    assert_eq!(tree.as_ref().map(|s| s.counts.games), Some(7));
    assert_eq!(explorer::stats(&idx, &start, &Cancel::never()).unwrap(), tree);

    // Through the endpoint, as the analysis panel asks for it.
    let (bridge, id) = TestBridge::database(&db, &dir);
    let url = format!("/v1/databases/{id}/explorer?fen={}", fen_param(&board_after("e2e4 e7e5").fen()));
    let body = answered(bridge.port, &url);
    assert!(
        body.contains(r#""games":4,"white":2,"draws":1,"black":1,"moves":[{"uci":"g1f3","san":"Nf3","games":2"#),
        "{body}"
    );
    let numbers: Vec<u32> = objects(&body, "topGames").into_iter().map(number_of).collect();
    assert_eq!(numbers, [3, 1, 4, 2]);
    drop((idx, bridge));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A game that reaches a position both within the tree's plies and beyond
/// them counts once, with the move it played from its first visit, and so
/// does one that reaches it more than once beyond them (#146).
#[test]
fn a_game_reaching_a_position_within_and_beyond_the_tree_counts_once() {
    let mut b = Builder::new();
    game(&mut b, "e2e4 e7e5 f1c4", 1, (2200, 2200));
    // At ply 2, then every fourth ply to ply 46, which plays 24. Nc3.
    game(&mut b, &format!("e2e4 e7e5 {}b1c3", hops(11)), 2, (2400, 2400));
    // At ply 42, then at 46 and 50, which plays 26. c3.
    game(&mut b, &format!("{}e2e4 e7e5 {}c2c3", hops(10), hops(2)), 0, (2300, 2300));
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-within-and-beyond");
    let dir = index_dir("within-and-beyond");
    let idx = prepared(&db, &dir);
    let board = board_after("e2e4 e7e5");
    let tree = idx.lookup(board.hash()).unwrap().unwrap();
    assert_eq!(tree.counts.games, 2);
    assert_eq!(tree.lookup_move("g1f3"), Some(1), "game 2's move from its first visit");
    let all = explorer::stats(&idx, &board, &Cancel::never()).unwrap().unwrap();
    assert_eq!(all.counts, Counts { games: 3, white: 1, draws: 1, black: 1 });
    assert_eq!(moves_of(&all), [(code("g1f3"), 2), (code("f1c4"), 1)]);
    assert_eq!(all.moves[0].1, Counts { games: 2, white: 1, draws: 0, black: 1 });
    assert_eq!(all.top, vec![2, 3, 1]);
    // The same games, all of them, whichever part counts them: game 2 at
    // ply 2 and game 3 at ply 42, each once.
    let every = explorer::deep_stats(&idx, &board, &Cancel::never()).unwrap().unwrap();
    assert_eq!((every.counts.games, every.lookup_move("g1f3")), (2, Some(2)), "game 1 is too short for the bucket");
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A set-up game is counted in a tree position it reaches from its own start
/// only beyond the tree's plies, at ply 21, the first beyond them; one that
/// reaches it within them, at ply 17, is counted by the tree alone (#146).
#[test]
fn a_set_up_game_reaching_a_tree_position_beyond_the_tree_is_counted() {
    // 1.e4 e5 2.Nf3, black to move, knights out and back from there.
    let fen = board_after("e2e4 e7e5 g1f3").fen();
    let cycle = "b8c6 b1c3 c6b8 c3b1 ";
    let mut b = Builder::new();
    game(&mut b, "e2e4 e7e5 g1f3 g8f6 b1c3", 2, (2200, 2200));
    game(&mut b, "e2e4 e7e5 g1f3 g8f6 f3e5", 1, (2300, 2300));
    for (cycles, next, result, elo) in [(5, "d2d3", 0u8, 2500i16), (4, "d2d4", 2, 0)] {
        let at = b.moves(1, &move_words(Some(&fen), &format!("{}g8f6 {next}", cycle.repeat(cycles))));
        let rec = b.game(at);
        rec[0x58] = result;
        rec[0x60..0x62].copy_from_slice(&elo.to_le_bytes());
        rec[0x70..0x72].copy_from_slice(&elo.to_le_bytes());
    }
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-setup-beyond");
    let dir = index_dir("setup-beyond");
    let idx = prepared(&db, &dir);
    assert!(idx.stream.entry(3).unwrap().setup() && idx.stream.entry(4).unwrap().setup());
    let board = board_after("e2e4 e7e5 g1f3 g8f6");
    let tree = idx.lookup(board.hash()).unwrap().unwrap();
    assert_eq!((tree.counts.games, tree.lookup_move("d2d4")), (3, Some(1)), "games 1, 2 and 4");
    let all = explorer::stats(&idx, &board, &Cancel::never()).unwrap().unwrap();
    assert_eq!(all.counts, Counts { games: 4, white: 2, draws: 1, black: 1 });
    assert_eq!(
        moves_of(&all),
        [(code("b1c3"), 1), (code("d2d3"), 1), (code("d2d4"), 1), (code("f3e5"), 1)],
        "one each, by code"
    );
    assert_eq!(all.top, vec![3, 2, 1, 4]);
    drop(idx);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The index file at `path` with the record of position `key` replaced by
/// `stats`, and the offsets, tables and CRCs after it written again to match:
/// a file sound in every check but what that record counts.
fn replace_record(path: &Path, key: u64, stats: &Stats) {
    let bytes = std::fs::read(path).unwrap();
    let mut header = Header::decode(&bytes).unwrap();
    let mut out = vec![0u8; HEADER_LEN];
    let (mut table, mut replaced) = (Vec::new(), false);
    for i in 0..header.blocks as usize {
        let block = Block::decode(&bytes[header.table_offset as usize + i * BLOCK_ENTRY..]);
        let (keys, data) = bytes[block.offset as usize..].split_at(block.keys as usize * KEY_ENTRY);
        let (mut written, mut records) = (Vec::new(), Vec::new());
        for entry in keys.chunks(KEY_ENTRY) {
            let k = u64::from_le_bytes(entry[..8].try_into().unwrap());
            written.extend(k.to_le_bytes());
            written.extend((records.len() as u32).to_le_bytes());
            let at = u32::from_le_bytes(entry[8..].try_into().unwrap()) as usize;
            let record = if k == key { stats.clone() } else { Stats::decode(&data[at..], header.games).unwrap() };
            replaced |= k == key;
            record.encode(&mut records);
        }
        written.extend(&records);
        let offset = out.len() as u64;
        Block { offset, data_len: records.len() as u32, crc: crc32(&written), ..block }.encode(&mut table);
        out.extend(&written);
    }
    assert!(replaced, "the index holds the position");
    let deep = &bytes[header.deep_offset as usize..header.deep_table_offset as usize];
    header.table_offset = out.len() as u64;
    header.table_crc = crc32(&table);
    out.extend(&table);
    let deep_offset = out.len() as u64;
    out.extend(deep);
    let mut deep_table = Vec::new();
    for e in bytes[header.deep_table_offset as usize..].chunks(DEEP_BLOCK_ENTRY) {
        let offset = u64::from_le_bytes(e[..8].try_into().unwrap()) - header.deep_offset + deep_offset;
        deep_table.extend(offset.to_le_bytes());
        deep_table.extend(&e[8..]);
    }
    header.deep_offset = deep_offset;
    header.deep_table_offset = out.len() as u64;
    header.deep_table_crc = crc32(&deep_table);
    out.extend(&deep_table);
    header.file_len = out.len() as u64;
    out[..HEADER_LEN].copy_from_slice(&header.encode());
    std::fs::write(path, out).unwrap();
}

/// A tree record that passes every CRC but counts what no sound index
/// holds: more games than the index, results beyond their games, a move's
/// too, moves of more games than the position's, or counts sound alone that
/// pass the index's games once the games beyond the tree are added (#146),
/// and a header that claims more games than records. Each is damage, never
/// a panic or a sum cut to 64 bits: the answer is `Corrupt`, and the
/// endpoint rebuilds the index and answers from the new one.
#[test]
fn counts_that_no_sound_index_holds_are_rebuilt() {
    let mut b = Builder::new();
    game(&mut b, "e2e4 e7e5 g1f3", 2, (2400, 2400));
    game(&mut b, "e2e4 e7e5 f1c4", 1, (2200, 2200));
    // At ply 42, after 40 plies of hops.
    game(&mut b, &format!("{}e2e4 e7e5 g1f3", hops(10)), 0, (2500, 2500));
    b.lid(lid_header(1024, 1));
    let db = b.write("explorer-bad-counts");
    let dir = index_dir("bad-counts");
    let board = board_after("e2e4 e7e5");
    let idx = prepared(&db, &dir);
    let tree = idx.lookup(board.hash()).unwrap().unwrap();
    assert_eq!((idx.games(), tree.counts.games), (3, 2));
    let all = Counts { games: 3, white: 1, draws: 1, black: 1 };
    assert_eq!(explorer::stats(&idx, &board, &Cancel::never()).unwrap().unwrap().counts, all);
    let (path, build) = (idx.base.path.clone(), idx.base.header.build_id);
    drop(idx);
    let sound = std::fs::read(&path).unwrap();
    let c = |games, white, draws, black| Counts { games, white, draws, black };
    let max = c(u64::MAX, u64::MAX, 0, 0);
    let (nf3, bc4) = (code("g1f3"), code("f1c4"));
    for (counts, moves, alone, why) in [
        // The reviewer's: the sum with the game beyond the tree passes 64 bits.
        (max, vec![(nf3, c(1, 1, 0, 0)), (bc4, c(1, 0, 1, 0))], false, "more games than the index"),
        (c(2, 1, 1, 1), vec![(nf3, c(1, 1, 0, 0)), (bc4, c(1, 0, 1, 0))], false, "results beyond the games"),
        (c(2, 1, 1, 0), vec![(nf3, max), (bc4, c(1, 0, 1, 0))], false, "a move of more games than the position"),
        (c(2, 1, 1, 0), vec![(nf3, c(1, 1, 0, 1)), (bc4, c(1, 0, 1, 0))], false, "a move's results beyond its games"),
        (c(2, 1, 1, 0), vec![(nf3, c(2, 1, 0, 0)), (bc4, c(1, 0, 1, 0))], false, "moves of more games than it"),
        // Sound alone: all three games, which the game beyond the tree passes.
        (c(3, 1, 1, 1), vec![(nf3, c(2, 1, 0, 1)), (bc4, c(1, 0, 1, 0))], true, "a sum beyond the index's games"),
    ] {
        std::fs::write(&path, &sound).unwrap();
        replace_record(
            &path,
            board.hash(),
            &Stats { counts, moves, top: tree.top.clone(), featured: tree.featured.clone() },
        );
        let idx = prepared(&db, &dir);
        assert_eq!(idx.base.header.build_id, build, "{why}: the file was kept");
        assert_eq!(idx.lookup(board.hash()).is_ok(), alone, "{why}");
        assert!(matches!(explorer::stats(&idx, &board, &Cancel::never()), Err(Bad::Corrupt(_))), "{why}");
    }
    // A header that claims more games than the records indexed, every
    // record's count within them.
    let mut header = Header::decode(&sound).unwrap();
    header.games = 4;
    let mut claims = sound.clone();
    claims[..HEADER_LEN].copy_from_slice(&header.encode());
    std::fs::write(&path, &claims).unwrap();
    assert!(matches!(IndexFile::open(&path), Err(Bad::Corrupt(_))), "more games than records");

    // Through the endpoint: `409` while the index is built again, then the
    // new one's answer.
    let (bridge, id) = TestBridge::database(&db, &dir);
    let url = format!("/v1/databases/{id}/explorer?fen={}", fen_param(&board.fen()));
    let found = r#""games":3,"white":1,"draws":1,"black":1"#;
    assert!(answered(bridge.port, &url).contains(found));
    drop(bridge);
    let path = dir.join("index").join(format!("{id}.idx"));
    let moves = vec![(nf3, c(1, 1, 0, 0)), (bc4, c(1, 0, 1, 0))];
    replace_record(
        &path,
        board.hash(),
        &Stats { counts: max, moves, top: tree.top.clone(), featured: tree.featured.clone() },
    );
    let (bridge, _) = TestBridge::database(&db, &dir);
    let (status, body) = get(bridge.port, &url);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("rebuilt"), "{body}");
    assert!(answered(bridge.port, &url).contains(found));
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}
