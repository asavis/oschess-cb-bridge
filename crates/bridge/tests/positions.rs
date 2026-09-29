//! `GET /v1/databases/{id}/games?fen=` (#148): every game that reaches a
//! position, at any ply, on databases built by hand.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::explorer::paths;
use bridge::explorer::stream::{BATCH, Header, SLOT_BYTES, TABLE_ENTRY};
use bridge::search::memory::{Hold, budget, held};
use cbformat::fixture::{Builder, TempDb, words};
use cbformat::movetable::{self, Color, END_OF_LINE, MOVES, Piece};
use chesscore::{Board, Color as CColor, Move, Piece as CPiece, Square};

mod common;
use common::{Served, get, lid, policy, put, serve_shared};

const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
/// 30 plies of a Queen's Gambit, which fourteen games of the fixture play.
const LINE_30: &str = "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5 g5e7 d8e7 e1g1 d5c3 c1c3 e6e5 d1c2 e5e4 f3d2 d7f6 f1e1 c8f5";
const SORT_KEYS: [&str; 12] = [
    "number",
    "white",
    "black",
    "whiteElo",
    "blackElo",
    "result",
    "moves",
    "eco",
    "tournament",
    "date",
    "round",
    "annotator",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Game,
    Deleted,
    Text,
    Analysis,
    Chess960,
}

/// A record of the fixture: its start, the standard one without, and its
/// moves in UCI, castling as the king's step.
struct Game {
    start: Option<String>,
    ucis: String,
    kind: Kind,
}

impl Game {
    fn new(kind: Kind, start: Option<&str>, ucis: &str) -> Game {
        Game { start: start.map(String::from), ucis: ucis.trim().to_string(), kind }
    }

    /// The index holds standard games that are not deleted.
    fn indexed(&self) -> bool {
        self.kind == Kind::Game
    }

    fn board(&self) -> Board {
        self.start.as_deref().map_or_else(Board::startpos, |f| Board::from_fen(f).unwrap())
    }

    /// Its positions, the start first, one after each move.
    fn positions(&self) -> Vec<Board> {
        let mut board = self.board();
        let mut out = vec![board.clone()];
        for uci in self.ucis.split_whitespace() {
            play(&mut board, uci);
            out.push(board.clone());
        }
        out
    }
}

/// Knights out and back, `n` times: plies of play that keep one position.
fn hops(n: usize) -> String {
    "g1f3 g8f6 f3g1 f6g8 ".repeat(n)
}

/// Plays `uci`, castling as the king's step.
fn play(board: &mut Board, uci: &str) {
    let mut mv: Move = uci.parse().unwrap();
    if board.piece_at(mv.from).map(|p| p.0) == Some(CPiece::King) && mv.from.file().abs_diff(mv.to.file()) == 2 {
        mv.to = Square::new(if mv.to.file() == 6 { 7 } else { 0 }, mv.from.rank());
    }
    board.play_checked(mv).unwrap();
}

fn board_after(ucis: &str) -> Board {
    let mut board = Board::startpos();
    for uci in ucis.split_whitespace() {
        play(&mut board, uci);
    }
    board
}

/// `games` lines of legal moves drawn from `seed`, 20 to 100 plies each,
/// their first plies from few moves, so that they share openings, then part
/// ways.
fn random_lines(games: usize, seed: u64) -> Vec<String> {
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..games)
        .map(|_| {
            let mut board = Board::startpos();
            let mut ucis = Vec::new();
            for ply in 0..20 + next() % 81 {
                let moves = board.legal_moves();
                if moves.is_empty() {
                    break;
                }
                let few = if ply < 8 { moves.len().min(3) } else { moves.len() };
                let mv = moves[(next() % few as u64) as usize];
                ucis.push(bridge::explorer::uci(&board, mv));
                board.play_checked(mv).unwrap();
            }
            ucis.join(" ")
        })
        .collect()
}

/// The records of the fixture, numbered from 1:
/// - 1-3: a Ruy Lopez, the same by another order, and an Italian game;
/// - 4-7: a deleted game, a guiding text, an analysis and a Chess960 game,
///   none of which the index holds;
/// - 8: a set-up game from the position after 1.e4 that reaches the Ruy
///   Lopez's positions;
/// - 9: a set-up game that plays far past the tree's plies;
/// - 10: knights out and back to the start, then 1.e4 e5 within the tree's
///   plies, and again beyond them;
/// - 11: knights out and back until ply 20, then 1.e4 e5 first beyond the
///   tree's plies;
/// - 12-25: fourteen Queen's Gambits, to ply 30;
/// - 26-55: thirty random games;
/// - 56: a set-up game from the position after 1.d4 that reaches the Queen's
///   Gambit's positions, which more than twelve games reach;
/// - 57: a set-up game with both sides' king's knights out that brings them
///   home to the standard start.
fn games() -> Vec<Game> {
    let standard = |ucis: &str| Game::new(Kind::Game, None, ucis);
    let set_up = |fen: &str, ucis: &str| Game::new(Kind::Game, Some(fen), ucis);
    let mut games = vec![
        standard("e2e4 e7e5 g1f3 b8c6 f1b5 a7a6 b5a4 g8f6 e1g1 f8e7"),
        standard("g1f3 b8c6 e2e4 e7e5 f1b5 a7a6 b5a4 g8f6"),
        standard("e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1 g8f6"),
        Game::new(Kind::Deleted, None, "e2e4 e7e5 g1f3 b8c6"),
        Game::new(Kind::Text, None, "e2e4"),
        Game::new(Kind::Analysis, None, "e2e4 e7e5"),
        Game::new(Kind::Chess960, None, "e2e4"),
        set_up("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1", "e7e5 g1f3 b8c6 f1b5 a7a6"),
        set_up("4k3/8/8/8/8/8/P7/4K3 w - - 0 1", &format!("{}a2a4 e8d8 e1f2", "e1d1 e8d8 d1e1 d8e8 ".repeat(6))),
        standard(&format!("{}e2e4 e7e5 {}d2d4", hops(3), hops(4))),
        standard(&format!("{}e2e4 e7e5 g1f3", hops(5))),
    ];
    games.extend((0..14).map(|_| standard(LINE_30)));
    games.extend(random_lines(30, 0x5eed_1234_abcd).iter().map(|l| standard(l)));
    games.push(set_up("rnbqkbnr/pppppppp/8/8/3P4/8/PPP1PPPP/RNBQKBNR b KQkq - 0 1", "d7d5 c2c4 e7e6 b1c3 g8f6"));
    games.push(set_up("rnbqkb1r/pppppppp/5n2/8/8/5N2/PPPPPPPP/RNBQKB1R w KQkq - 4 3", "f3g1 f6g8 e2e4"));
    games
}

/// The 2CBH move record of `g`.
fn move_record(g: &Game) -> Vec<u16> {
    let mut stream = Vec::new();
    if g.kind == Kind::Chess960 {
        // Chess960 from the standard arrangement: start 518.
        stream.extend([movetable::START_POSITION, 518]);
    } else if let Some(fen) = &g.start {
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
            let sq = Square::from_index(i).unwrap();
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
    stream.extend(words(&mut g.board(), &g.ucis));
    stream.push(END_OF_LINE);
    stream
}

/// The players of the fixture: id 0 is no name.
const PLAYERS: [&str; 4] = ["", "Alpha, Ann", "Beta, Bob", "Gamma, Gus"];

/// The fixture's records written as a 2CBH database, with players, dates,
/// rounds, results, ratings and move counts that differ from game to game.
fn database(name: &str, games: &[Game]) -> TempDb {
    let mut b = Builder::new();
    for (i, g) in games.iter().enumerate() {
        let n = i as i64 + 1;
        let at = b.moves(if g.kind == Kind::Chess960 { 2 } else { 1 }, &move_record(g));
        let rec = b.game(at);
        match g.kind {
            Kind::Text => rec[0] |= 2,
            Kind::Analysis => rec[2] = 2,
            Kind::Deleted => rec[0] |= 0x80,
            Kind::Game | Kind::Chess960 => {}
        }
        if matches!(g.kind, Kind::Text | Kind::Analysis) {
            continue;
        }
        put(rec, 0x18, &(n % 3 + 1).to_le_bytes());
        put(rec, 0x20, &((n + 1) % 3 + 1).to_le_bytes());
        let (year, month, day) = (1990 + (n * 7 % 35) as i32, (n % 12 + 1) as i32, (n * 5 % 28 + 1) as i32);
        put(rec, 0xbc, &((year << 9) | (month << 5) | day).to_le_bytes());
        put(rec, 0x5a, &((n % 9 + 1) as i16).to_le_bytes());
        rec[0x58] = (n % 3) as u8;
        put(rec, 0x8a, &((g.ucis.split_whitespace().count() as i16 + 1) / 2).to_le_bytes());
        put(rec, 0x60, &((1800 + n * 37 % 700) as i16).to_le_bytes());
        put(rec, 0x70, &((1900 + n * 53 % 600) as i16).to_le_bytes());
        // ECO codes A00 to E99 by turns, and none for some.
        let eco = if n % 5 == 0 { 0 } else { ((n * 13 % 500) as u16 + 1) * 128 };
        put(rec, 0x80, &eco.to_le_bytes());
    }
    let players: Vec<String> = PLAYERS.iter().map(|p| p.to_string()).collect();
    b.lid(lid(&players, &[String::new()], &[String::new()]));
    b.write(name)
}

fn index_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-positions-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Serves `db`, with the indexes in `dir` as in a data folder; the bridge
/// and the database's id.
fn serve(db: &TempDb, dir: &Path) -> (Served, String) {
    let path = db.dir().join("db.2cbh");
    let app = App::new("test", policy(), Catalog::new([path.clone()]));
    app.catalog.use_data_dir(dir);
    (Served::new(app), id_of(&path))
}

fn fen_param(fen: &str) -> String {
    fen.replace(' ', "%20").replace('/', "%2F")
}

/// The path of the list of the games of `fen`, with `extra` parameters.
fn list(id: &str, fen: &str, extra: &str) -> String {
    format!("/v1/databases/{id}/games?fen={}{extra}", fen_param(fen))
}

/// Waits for a `200` answer to `path`: the index is built meanwhile.
fn answered(port: u16, path: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = get(port, path);
        if status == 200 {
            return body;
        }
        assert_eq!(status, 409, "{body}");
        assert!(Instant::now() < deadline, "the index was not built: {body}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The answer to `path` once the index is built: `200` at once, never the
/// `409` of a scan that found the index damaged and builds it again, after
/// which [`answered`] would take the new build's answer.
fn at_once(port: u16, path: &str) -> String {
    let (status, body) = get(port, path);
    assert_eq!(status, 200, "{path}: {body}");
    body
}

/// The first number member `"key":123` of a JSON text.
fn number(body: &str, key: &str) -> u64 {
    let pat = format!("\"{key}\":");
    let at = body.find(&pat).unwrap_or_else(|| panic!("no {key} in {body}")) + pat.len();
    body[at..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap()
}

/// The objects of the array member `key` of the JSON text `body`, each as its
/// text: a scan that keeps to strings and nesting.
fn objects<'a>(body: &'a str, key: &str) -> Vec<&'a str> {
    let open = format!(r#""{key}":["#);
    let rest = &body[body.find(&open).unwrap_or_else(|| panic!("no {key} in {body}")) + open.len()..];
    let (mut out, mut depth, mut from, mut in_string, mut escaped) = (Vec::new(), 0, 0, false, false);
    for (at, c) in rest.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            _ if in_string => {}
            '{' => {
                if depth == 0 {
                    from = at;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(&rest[from..=at]);
                }
            }
            ']' if depth == 0 => return out,
            _ => {}
        }
    }
    panic!("{key} is not closed in {body}")
}

/// The numbers of a list's rows, in order.
fn rows(body: &str) -> Vec<u32> {
    objects(body, "rows")
        .iter()
        .map(|row| {
            let digits = row.strip_prefix(r#"{"number":"#).unwrap_or_else(|| panic!("no number first in {row}"));
            digits[..digits.find(',').unwrap()].parse().unwrap()
        })
        .collect()
}

/// The records of the games that reach each position, by brute force: every
/// position of every game the index holds, to its end.
struct Reached(HashMap<u64, BTreeSet<u32>>);

impl Reached {
    fn of(games: &[Game]) -> Reached {
        let mut map: HashMap<u64, BTreeSet<u32>> = HashMap::new();
        for (n, g) in (1..).zip(games).filter(|(_, g)| g.indexed()) {
            for p in g.positions() {
                map.entry(p.hash()).or_default().insert(n);
            }
        }
        Reached(map)
    }

    fn games(&self, board: &Board) -> BTreeSet<u32> {
        self.0.get(&board.hash()).cloned().unwrap_or_default()
    }
}

/// The games of the fixture, served, once the index is built: the games, the
/// database, the bridge, its id, and its index folder.
fn served(name: &str) -> (Vec<Game>, TempDb, Served, String, PathBuf) {
    let games = games();
    let db = database(&format!("positions-{name}"), &games);
    let dir = index_dir(name);
    let (bridge, id) = serve(&db, &dir);
    answered(bridge.port, &list(&id, START, ""));
    (games, db, bridge, id, dir)
}

/// For every position of every game of the fixture to its 60th ply, the list
/// holds exactly the games a brute-force replay finds reaching it, in number
/// order, each once: `total` and `position.games` count them, and so does
/// the explorer. Positions within the tree's plies and beyond them, of more
/// than twelve games and of fewer, of set-up games and of games the index
/// leaves out, are all among them.
#[test]
fn every_position_lists_the_games_a_replay_finds() {
    let (games, _db, bridge, id, dir) = served("brute");
    let reached = Reached::of(&games);
    // Each position with the first ply a game of the fixture reaches it at.
    let mut positions: BTreeMap<u64, (Board, usize)> = BTreeMap::new();
    for g in games.iter().filter(|g| g.kind != Kind::Chess960) {
        for (ply, board) in g.positions().into_iter().take(61).enumerate() {
            let at = positions.entry(board.hash()).or_insert((board, ply));
            at.1 = at.1.min(ply);
        }
    }
    let (mut crowded, mut few, mut beyond) = (0, 0, 0);
    for (board, ply) in positions.values() {
        let fen = board.fen();
        let want = reached.games(board);
        let body = at_once(bridge.port, &list(&id, &fen, "&limit=500"));
        let explorer = at_once(bridge.port, &format!("/v1/databases/{id}/explorer?fen={}", fen_param(&fen)));
        let got = rows(&body);
        assert_eq!(got.iter().copied().collect::<BTreeSet<_>>(), want, "{fen}");
        assert!(got.windows(2).all(|w| w[0] < w[1]), "in number order, each once: {fen}");
        let n = want.len() as u64;
        assert_eq!((number(&body, "total"), number(&explorer, "games")), (n, n), "{fen}");
        assert!(body.contains(&format!(r#""position":{{"fen":"{fen}","games":{n}}},"rows":["#)), "{body}");
        crowded += usize::from(n > 12);
        few += usize::from((1..=12).contains(&n));
        beyond += usize::from(*ply > 20 && n > 0);
    }
    assert!(positions.len() > 1000, "{} positions", positions.len());
    assert!(crowded > 20 && few > 500 && beyond > 500, "{crowded} crowded, {few} of few games, {beyond} beyond");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The first scan of the stream reads every slot, and notes which games start
/// from the standard position and which from a set-up one (#142); a scan for
/// the standard start after it takes the first as they are and replays the
/// others, and finds the games the first found, at once, a set-up game that
/// comes home among them. The scans of other positions read every slot.
#[test]
fn the_start_after_the_first_scan_lists_what_it_listed() {
    let games = games();
    let reached = Reached::of(&games);
    let db = database("positions-starts", &games);
    let dir = index_dir("starts");
    let (bridge, id) = serve(&db, &dir);
    let boards = [Board::startpos(), board_after("d2d4 d7d5 c2c4 e7e6"), board_after("d2d4")];
    assert!(reached.games(&boards[0]).contains(&57), "the set-up game that comes home");
    for (round, sort) in ["number", "white"].into_iter().enumerate() {
        for (i, board) in boards.iter().enumerate() {
            let fen = board.fen();
            let want = reached.games(board);
            assert!(want.len() > 12, "{fen}: a scan finds its games");
            // The first builds the index, then scans every slot.
            let path = list(&id, &fen, &format!("&limit=500&sort={sort}"));
            let body = if round + i == 0 { answered(bridge.port, &path) } else { at_once(bridge.port, &path) };
            assert_eq!(rows(&body).into_iter().collect::<BTreeSet<_>>(), want, "{fen} by {sort}");
        }
    }
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A transposition is listed once, set-up games are listed with the standard
/// ones, and deleted games, guiding texts, analyses and Chess960 games never
/// are. A game that reaches a position first beyond the tree's plies is
/// listed with those that reach it within them, and one that comes back to a
/// position is listed once.
#[test]
fn transpositions_set_up_games_and_what_is_never_listed() {
    let (_games, _db, bridge, id, dir) = served("kinds");
    let numbers = |ucis: &str| rows(&answered(bridge.port, &list(&id, &board_after(ucis).fen(), "&limit=500")));
    // Games 1 and 2 by two orders, game 8 from its set-up start.
    assert_eq!(numbers("e2e4 e7e5 g1f3 b8c6 f1b5 a7a6"), [1, 2, 8]);
    // After 1.e4: never 4 to 7, which all play it; 8 starts there.
    let e4 = numbers("e2e4");
    assert!(e4.starts_with(&[1, 3, 8, 10, 11]), "{e4:?}");
    // After 1.e4 e5: game 10 within the tree's plies and again beyond them,
    // game 11 beyond them only.
    let e5 = numbers("e2e4 e7e5");
    assert!(e5.starts_with(&[1, 3, 8, 10, 11]), "{e5:?}");
    // The start: every standard game once, however often it comes back.
    let start = numbers("");
    assert_eq!(&start[..8], [1, 2, 3, 10, 11, 12, 13, 14]);
    assert_eq!(start.len(), 57 - 7, "all but the four left out and three set-up ones");
    assert_eq!(start.last(), Some(&57), "the set-up game that comes home");
    // More than twelve games reach 1.d4 d5 2.c4 e6, a set-up one among them.
    let gambit = numbers("d2d4 d7d5 c2c4 e7e6");
    assert!(gambit.len() > 12 && gambit.ends_with(&[56]), "{gambit:?}");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A line is given up early only on what it never gets back: its men, its
/// pawns and its home pawns, never on its pieces of one kind, which a
/// promotion adds to (#142). So the positions after a queen was taken and a
/// pawn promoted to a queen, with a third knight, or with a second bishop on
/// squares of one colour list their games, and so do the positions after
/// them: within the tree's plies, where more than twelve games reach them
/// and the stream is scanned, and beyond them, where one game does and it is
/// replayed.
#[test]
fn positions_after_a_promotion_list_their_games() {
    // 1.e4 e5 2.Qh5 Nc6 3.Qxf7+ Kxf7 4.a4 b5 5.axb5 a6 6.bxa6 Bb7 7.axb7 Nf6
    // 8.bxa8, at ply 15, or 23 after the knights' hops.
    let line = "e2e4 e7e5 d1h5 b8c6 h5f7 e8f7 a2a4 b7b5 a4b5 a7a6 b5a6 c8b7 a6b7 g8f6";
    let mut games = Vec::new();
    for promotion in ["b7a8q", "b7a8n", "b7a8b"] {
        let ucis = format!("{line} {promotion} f8e7 g1f3 h8f8");
        games.extend((0..13).map(|_| Game::new(Kind::Game, None, &ucis)));
        games.push(Game::new(Kind::Game, None, &format!("{}{ucis}", hops(2))));
    }
    let after = |promotion: &str| board_after(&format!("{line} {promotion}"));
    assert_eq!(after("b7a8q").colored(CPiece::Queen, CColor::White).count_ones(), 1, "the queen taken, then a new one");
    assert_eq!(after("b7a8n").colored(CPiece::Knight, CColor::White).count_ones(), 3, "a third knight");
    const LIGHT: u64 = 0x55aa_55aa_55aa_55aa;
    let bishops = after("b7a8b").colored(CPiece::Bishop, CColor::White);
    assert_eq!((bishops & LIGHT).count_ones(), 2, "two bishops on light squares");
    let db = database("positions-promotions", &games);
    let dir = index_dir("promotions");
    let (bridge, id) = serve(&db, &dir);
    let reached = Reached::of(&games);
    let (mut within, mut beyond) = (0, 0);
    let mut lines: Vec<&Game> = Vec::new();
    for g in &games {
        if !lines.iter().any(|l| l.ucis == g.ucis) {
            lines.push(g);
        }
    }
    for g in lines {
        let promoted_at = g.ucis.split_whitespace().position(|uci| uci.starts_with("b7a8")).unwrap() + 1;
        for (ply, board) in g.positions().into_iter().enumerate().skip(promoted_at) {
            let fen = board.fen();
            let want = reached.games(&board);
            let got = rows(&answered(bridge.port, &list(&id, &fen, "&limit=500")));
            assert_eq!(got.iter().copied().collect::<BTreeSet<_>>(), want, "{fen}");
            assert!(!want.is_empty(), "{fen}");
            within += usize::from(ply <= 20 && want.len() > 12);
            beyond += usize::from(ply > 20);
        }
    }
    assert_eq!((within, beyond), (12, 12), "each promotion within the tree's plies and beyond them");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The list of a position's games is the database's list narrowed to them:
/// by every sort key in both directions, with `q`, whose `total` counts the
/// games matching both while `position.games` counts the position's; its
/// windows follow each other; `line` adds each row's main line.
#[test]
fn sort_q_and_windows_compose_with_a_position() {
    let (games, _db, bridge, id, dir) = served("compose");
    let reached = Reached::of(&games);
    let port = bridge.port;
    let whole = |extra: &str| rows(&answered(port, &format!("/v1/databases/{id}/games?limit=500{extra}")));
    let deep = board_after(&LINE_30.split_whitespace().take(25).collect::<Vec<_>>().join(" "));
    for board in [Board::startpos(), board_after("d2d4"), board_after("e2e4 e7e5"), deep] {
        let fen = board.fen();
        let members = reached.games(&board);
        assert!(!members.is_empty(), "{fen}");
        for key in SORT_KEYS {
            for sort in [key.to_string(), format!("{key}-asc"), format!("{key}-desc")] {
                let want: Vec<u32> =
                    whole(&format!("&sort={sort}")).into_iter().filter(|n| members.contains(n)).collect();
                let body = answered(port, &list(&id, &fen, &format!("&limit=500&sort={sort}")));
                assert_eq!(rows(&body), want, "{sort} of {fen}");
                assert_eq!(number(&body, "total"), members.len() as u64);
            }
        }
        for q in ["player:alpha", "result:1-0", "-result:1-0 date:>=2005", "white:beta sort:date", "nobody"] {
            let q = q.replace(' ', "%20").replace(':', "%3A").replace('>', "%3E").replace('=', "%3D");
            let want: Vec<u32> = whole(&format!("&q={q}")).into_iter().filter(|n| members.contains(n)).collect();
            let body = answered(port, &list(&id, &fen, &format!("&limit=500&q={q}")));
            assert_eq!(rows(&body), want, "{q} in {fen}");
            assert_eq!(number(&body, "total"), want.len() as u64, "{q} in {fen}");
            assert!(body.contains(&format!(r#""position":{{"fen":"{fen}","games":{}}}"#, members.len())), "{body}");
        }
        // Windows of 7 in date order, one after the other, are the whole list.
        let all = rows(&answered(port, &list(&id, &fen, "&limit=500&sort=date")));
        let mut paged = Vec::new();
        for offset in (0..all.len() + 7).step_by(7) {
            let body = answered(port, &list(&id, &fen, &format!("&offset={offset}&limit=7&sort=date")));
            assert_eq!(number(&body, "total"), all.len() as u64);
            assert_eq!(number(&body, "offset"), offset as u64);
            paged.extend(rows(&body));
        }
        assert_eq!(paged, all, "{fen}");
        // Each game row carries its line.
        let body = answered(port, &list(&id, &fen, "&limit=500&line=4"));
        assert_eq!(objects(&body, "rows").iter().filter(|r| r.contains(r#","line":"#)).count(), members.len());
    }
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A position that no game reaches has no games; a list without `fen`
/// acknowledges none, and `variant` alone narrows nothing.
#[test]
fn a_position_no_game_reaches_and_a_list_without_one() {
    let (_games, _db, bridge, id, dir) = served("none");
    let body = answered(bridge.port, &list(&id, "4k3/8/8/8/8/8/8/4K3 w - - 0 1", ""));
    assert!(body.contains(r#""total":0,"offset":0,"sort":"number-asc","position":{"fen":"4k3/8/8/8/8/8/8/4K3 w - - 0 1","games":0},"rows":[]"#), "{body}");
    // Nor with a sort or a query.
    for extra in ["&sort=date", "&q=player%3Aalpha", "&q=nobody&sort=white-desc"] {
        let body = answered(bridge.port, &list(&id, "4k3/8/8/8/8/8/8/4K3 w - - 0 1", extra));
        assert!(body.contains(r#""games":0},"rows":[]"#) && body.contains(r#""total":0"#), "{body}");
    }
    for extra in ["", "?variant=chess960"] {
        let body = answered(bridge.port, &format!("/v1/databases/{id}/games{extra}"));
        assert!(!body.contains("position") && body.contains(r#""total":57"#), "{body}");
    }
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Until the index is built the list is answered as the explorer is: `409`
/// with `state: "indexing"` and the progress. A FEN that is not a position
/// is `400` naming `fen`, and a Chess960 position or variant `422`.
#[test]
fn errors_are_the_explorers() {
    let games = games();
    let db = database("positions-errors", &games);
    let dir = index_dir("errors");
    let (bridge, id) = serve(&db, &dir);
    let (status, body) = get(bridge.port, &list(&id, START, ""));
    assert_eq!(status, 409, "the first request starts the build: {body}");
    assert!(body.contains(r#""state":"indexing""#) && body.contains(r#""progress":{"phase":"#), "{body}");
    answered(bridge.port, &list(&id, START, ""));
    for fen in ["4k3/8/8/8/8/8/8/4KR1R w F - 0 1", "4k3/8/8/8/8/8/8/4KR1R w H - 0 1"] {
        let (status, body) = get(bridge.port, &list(&id, fen, ""));
        assert_eq!(status, 422, "{body}");
        assert!(body.contains(r#""code":"unsupported""#) && body.contains(r#""variant":"chess960""#), "{body}");
    }
    let (status, _) = get(bridge.port, &list(&id, START, "&variant=chess960"));
    assert_eq!(status, 422);
    assert_eq!(get(bridge.port, &list(&id, START, "&variant=standard")).0, 200);
    for fen in ["", "nonsense"] {
        let (status, body) = get(bridge.port, &list(&id, fen, ""));
        assert_eq!(status, 400, "{fen}: {body}");
        assert!(body.contains(r#""parameter":"fen""#), "{body}");
    }
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The games of a position with a `q` that uses a qualifier only the
/// Library has are refused before the database is opened: nothing is built
/// or queued for its index (#173). A list without the qualifier then starts
/// the build.
#[test]
fn an_unsupported_qualifier_starts_no_build() {
    let games = games();
    let db = database("positions-unsupported", &games);
    let dir = index_dir("unsupported");
    let (bridge, id) = serve(&db, &dir);
    for qualifier in ["tag", "created", "updated", "is", "has", "no"] {
        let (status, body) = get(bridge.port, &list(&id, START, &format!("&stream=tab&q={qualifier}%3Ax")));
        assert_eq!(status, 400, "{body}");
        assert_eq!(
            body,
            format!(
                r#"{{"error":{{"code":"unsupported_qualifier","message":"ChessBase databases do not have this qualifier","qualifier":"{qualifier}"}}}}"#
            )
        );
    }
    let (_, status) = get(bridge.port, "/v1/status");
    assert!(!status.contains(r#""indexing""#), "{status}");
    let (index, stream) = paths(&dir.join("index"), &id);
    assert!(!index.exists() && !stream.exists());
    let (status, body) = get(bridge.port, &list(&id, START, "&q=white%3Aalpha"));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""state":"indexing""#), "{body}");
    answered(bridge.port, &list(&id, START, ""));
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A newer request with `fen` in the same stream supersedes the one still
/// running there, which is answered `409 superseded`; one in another
/// stream, or without a stream, is never superseded.
#[test]
fn a_newer_request_in_the_same_stream_supersedes() {
    let games = games();
    let db = database("positions-streams", &games);
    let dir = index_dir("streams");
    let path = db.dir().join("db.2cbh");
    let app = App::new("test", policy(), Catalog::new([path.clone()]));
    app.catalog.use_data_dir(&dir);
    let (port, app) = serve_shared(app);
    let id = id_of(&path);
    answered(port, &list(&id, START, ""));
    let indexes = app.catalog.get(&id).unwrap().open().unwrap().indexes;
    let held = indexes.gate().hold(3);
    let ask = |fen: String, extra: &'static str| {
        let path = list(&id, &fen, extra);
        std::thread::spawn(move || get(port, &path))
    };
    let d4 = board_after("d2d4").fen();
    let named = ask(d4.clone(), "&stream=tab");
    assert!(held.arrived(1, Duration::from_secs(30)));
    // In orders of their own: the result of either, kept once it is found,
    // would answer the named one before it looks at a game, and so before it
    // could stop.
    let other = ask(d4.clone(), "&stream=other&sort=white");
    let unnamed = ask(d4.clone(), "&sort=number-desc");
    assert!(held.arrived(3, Duration::from_secs(30)));
    // The newest in the stream is answered at once: only three are held.
    let (status, body) = get(port, &list(&id, &board_after("e2e4").fen(), "&stream=tab"));
    assert_eq!(status, 200, "{body}");
    drop(held);
    let (status, body) = named.join().unwrap();
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""code":"superseded""#), "{body}");
    for kept in [other, unnamed] {
        let (status, body) = kept.join().unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(number(&body, "total"), Reached::of(&games).games(&board_after("d2d4")).len() as u64);
    }
    app.catalog.explorer.release();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A superseded request gets `409 superseded` even when its result is kept:
/// the kept result would answer before any game is looked at, and so before
/// any other check could stop it (#148).
#[test]
fn a_superseded_request_is_not_answered_from_kept_results() {
    let games = games();
    let db = database("positions-streams-kept", &games);
    let dir = index_dir("streams-kept");
    let path = db.dir().join("db.2cbh");
    let app = App::new("test", policy(), Catalog::new([path.clone()]));
    app.catalog.use_data_dir(&dir);
    let (port, app) = serve_shared(app);
    let id = id_of(&path);
    let d4 = board_after("d2d4").fen();
    // The result of 1.d4, in the default order, kept.
    answered(port, &list(&id, &d4, ""));
    let indexes = app.catalog.get(&id).unwrap().open().unwrap().indexes;
    let held = indexes.gate().hold(1);
    let older = {
        let path = list(&id, &d4, "&stream=tab");
        std::thread::spawn(move || get(port, &path))
    };
    assert!(held.arrived(1, Duration::from_secs(30)));
    let (status, body) = get(port, &list(&id, &board_after("e2e4").fen(), "&stream=tab"));
    assert_eq!(status, 200, "{body}");
    drop(held);
    let (status, body) = older.join().unwrap();
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""code":"superseded""#), "{body}");
    app.catalog.explorer.release();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Where record `n`'s slot is in the move stream `bytes`: in its block, which
/// the table at the file's end places.
fn slot_at(bytes: &[u8], n: u32) -> usize {
    let header = Header::decode(bytes).unwrap();
    let i = (n - header.first_record) as usize;
    let entry = header.table_offset as usize + i / BATCH * TABLE_ENTRY;
    u64::from_le_bytes(bytes[entry..entry + 8].try_into().unwrap()) as usize + i % BATCH * SLOT_BYTES
}

/// With `bridge` dropped, which then maps the move stream no longer (on
/// Windows it would keep it from being written), changes the stream of the
/// index of `id` in `dir` with `damage` and serves `db` again: the list at
/// `url` is answered `409` while both files are built again, then `want`
/// from the new build. The new bridge.
fn damaged(
    bridge: Served,
    (db, dir, id): (&TempDb, &Path, &str),
    url: &str,
    want: &[u32],
    damage: impl FnOnce(&mut [u8]),
) -> Served {
    drop(bridge);
    let path = dir.join("index").join(format!("{id}.moves"));
    let mut bytes = std::fs::read(&path).unwrap();
    let before = Header::decode(&bytes).unwrap();
    damage(&mut bytes);
    std::fs::write(&path, &bytes).unwrap();
    let (bridge, _) = serve(db, dir);
    let (status, body) = get(bridge.port, url);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("rebuilt") && body.contains(r#""state":"indexing""#), "{body}");
    assert_eq!(rows(&answered(bridge.port, url)), want);
    let after = Header::decode(&std::fs::read(&path).unwrap()).unwrap();
    assert_ne!(after.build_id, before.build_id, "built again");
    bridge
}

/// A byte of a slot of the move stream damaged, in a block the list's scan
/// reads, is never answered from, whether it changes which games are found
/// or not: the answer is `409` while the index is built again, and the next
/// answers come from the new build.
#[test]
fn a_damaged_slot_is_never_listed() {
    let (games, db, mut bridge, id, dir) = served("damaged");
    let url = list(&id, &board_after("d2d4").fen(), "&limit=500");
    let want = rows(&answered(bridge.port, &url));
    assert!(want.len() > 12 && want.contains(&12), "{want:?}");
    assert_eq!(want.len(), Reached::of(&games).games(&board_after("d2d4")).len());
    let [d4, e4] = [words(&mut Board::startpos(), "d2d4")[0], words(&mut Board::startpos(), "e2e4")[0]];
    // Game 12's slot, of the first block: its first word, 1.d4, becomes 1.e4
    // while its entry still says that d2 left first, so that the game is
    // missed; or its rating, its tenth word, which the scan never reaches
    // for 1.d4, or its CRC, none of which changes what is found.
    for k in [16, 7, 16 + 2 * 9, 60] {
        bridge = damaged(bridge, (&db, &dir, &id), &url, &want, |bytes| {
            let at = slot_at(bytes, 12) + k;
            if k == 16 {
                assert_eq!(&bytes[at..at + 2], &d4.to_le_bytes());
                bytes[at..at + 2].copy_from_slice(&e4.to_le_bytes());
            } else {
                bytes[at] ^= 0x10;
            }
        });
    }
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Two slots of the move stream exchanged, each sound but in the other's
/// place, so that the games the scan finds are as many as the tree counts,
/// but not the position's: within a block or across two, the list never
/// answers from them, and is `409` while the index is built again, then the
/// position's games.
#[test]
fn exchanged_slots_are_never_listed() {
    // Records 1-13 play 1.e4, which the scan then finds, having more than
    // twelve games; the others 1.d4, into a second block of the stream.
    let n = BATCH as u32 + 26;
    let games: Vec<Game> =
        (1..=n).map(|r| Game::new(Kind::Game, None, if r <= 13 { "e2e4" } else { "d2d4" })).collect();
    let db = database("positions-exchanged", &games);
    let dir = index_dir("exchanged");
    let (mut bridge, id) = serve(&db, &dir);
    let url = list(&id, &board_after("e2e4").fen(), "&line=1");
    let want: Vec<u32> = (1..=13).collect();
    assert_eq!(rows(&answered(bridge.port, &url)), want);
    // Game 1's slot with game 14's, in the first block, or with the first
    // game's of the second.
    for other in [14, BATCH as u32 + 1] {
        bridge = damaged(bridge, (&db, &dir, &id), &url, &want, |bytes| {
            let (x, y) = (slot_at(bytes, 1), slot_at(bytes, other));
            for k in 0..SLOT_BYTES {
                bytes.swap(x + k, y + k);
            }
        });
    }
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

const CHILD: &str = "BRIDGE_POSITIONS_SMALL_BUDGET_CHILD";

/// Whether this is the child that runs the test's body. The parent runs the
/// test `name` in a child with a 16 MiB budget and one worker, and checks it
/// passed: the budget is read once per process.
fn in_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("OSCHESS_BRIDGE_SEARCH_MIB", "16")
        .env("OSCHESS_BRIDGE_THREADS", "1")
        .output()
        .unwrap();
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
    false
}

/// A list of a position's games in a sort order that is kept is answered
/// whole whenever the budget left holds the list itself: the lists the
/// workers may gather its parts into first are a saving, which never makes
/// the list busy nor evicts the order (#142, review of #163).
#[test]
fn a_list_in_a_kept_order_needs_no_room_but_its_own() {
    if !in_child("a_list_in_a_kept_order_needs_no_room_but_its_own") {
        return;
    }
    const GAMES: u32 = 12_000;
    let games: Vec<Game> = (0..GAMES).map(|_| Game::new(Kind::Game, None, "e2e4")).collect();
    let db = database("positions-tight", &games);
    let dir = index_dir("tight");
    let (bridge, id) = serve(&db, &dir);
    let e4 = board_after("e2e4").fen();
    // The index built and its stream scanned once; the order by White kept.
    assert_eq!(number(&answered(bridge.port, &list(&id, &e4, "&sort=number")), "total"), u64::from(GAMES));
    at_once(bridge.port, &format!("/v1/databases/{id}/games?sort=white&limit=1"));
    // Room for the list, 48 KB, and a little more, but not for part lists
    // besides it, twice as much again.
    let free = GAMES as usize * 4 + (32 << 10);
    let taken = loop {
        if let Ok(hold) = Hold::reserve_quietly(budget().saturating_sub(held() + free)) {
            break hold;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let kept = held();
    let body = at_once(bridge.port, &list(&id, &e4, "&sort=white"));
    assert_eq!(number(&body, "total"), u64::from(GAMES));
    assert!(held() >= kept, "nothing retained was evicted");
    drop(taken);
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The starts a stream keeps are given up when a list needs their room: the
/// standard start's games are listed whole whenever the budget left holds
/// the list and the position's set, by the first scan, which finds the
/// starts, and by those after it (#142, review of #163).
#[test]
fn kept_starts_give_way_to_a_list() {
    if !in_child("kept_starts_give_way_to_a_list") {
        return;
    }
    const GAMES: usize = 100_000;
    let games: Vec<Game> = (0..GAMES).map(|_| Game::new(Kind::Game, None, "e2e4")).collect();
    let db = database("positions-starts-room", &games);
    let dir = index_dir("starts-room");
    let (bridge, id) = serve(&db, &dir);
    // The index built without a list, so that no scan has found the starts.
    answered(bridge.port, &format!("/v1/databases/{id}/explorer?fen={}", fen_param(START)));
    for sort in ["number", "number-desc"] {
        // Room for the list, 400 KB, the set, 12.5 KB, and a little more:
        // not for the starts besides, 25 KB.
        let free = GAMES * 4 + GAMES / 8 + (8 << 10);
        let taken = loop {
            if let Ok(hold) = Hold::reserve_quietly(budget().saturating_sub(held() + free)) {
                break hold;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let body = at_once(bridge.port, &list(&id, START, &format!("&sort={sort}")));
        assert_eq!(number(&body, "total"), GAMES as u64, "{sort}");
        drop(taken);
    }
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// With the search memory all but a little taken, a list of a position's
/// games is answered `503 busy`, or answered whole when what is left holds
/// it, whichever step runs short: never a panic, never another error.
#[test]
fn a_small_budget_answers_busy_and_never_panics() {
    if !in_child("a_small_budget_answers_busy_and_never_panics") {
        return;
    }
    let (games, _db, bridge, id, dir) = served("small-budget");
    let reached = Reached::of(&games);
    let deep = board_after(&LINE_30.split_whitespace().take(25).collect::<Vec<_>>().join(" "));
    let boards = [Board::startpos(), board_after("d2d4"), board_after("e2e4 e7e5"), deep];
    let (mut busy, mut whole) = (0, 0);
    for (round, free) in [0usize, 64, 1 << 10, 8 << 10, 64 << 10, 256 << 10, 1 << 20, 4 << 20].into_iter().enumerate() {
        // A build of the heads file in the background may hold some a while.
        let taken = loop {
            if let Ok(hold) = Hold::reserve_quietly(budget().saturating_sub(held() + free)) {
                break hold;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        for board in &boards {
            // A query and a sort of this round alone, so that no result kept
            // before answers it.
            let sort = SORT_KEYS[round % SORT_KEYS.len()];
            for extra in [format!("&sort={sort}"), format!("&q=-event%3Ax{round}")] {
                let (status, body) = get(bridge.port, &list(&id, &board.fen(), &extra));
                match status {
                    200 => {
                        assert_eq!(number(&body, "total"), reached.games(board).len() as u64, "{body}");
                        whole += 1;
                    }
                    503 if body.contains(r#""code":"busy""#) => busy += 1,
                    _ => panic!("{status} {body}"),
                }
            }
        }
        drop(taken);
    }
    assert!(busy > 0 && whole > 0, "{busy} busy, {whole} whole");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}
