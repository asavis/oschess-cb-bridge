//! `GET /v1/databases/{id}/games` by a position fragment and by material
//! (#272): the games a brute-force replay finds, with the masks and without
//! them, the ply of each game's first match, and how the filter combines with
//! the other parameters.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bridge::catalog::id_of;
use bridge::explorer::{masks, paths};
use cbformat::fixture::{Builder, TempDb, words};
use cbformat::movetable::{self, Color, END_OF_LINE, MOVES, Piece};
use chesscore::{Board, Color as CColor, Piece as CPiece, Square};

mod common;
use common::{TestBridge, answered, fen_param, get, index_dir, lid, objects, play, put, settle};

const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
/// A Najdorf with a white knight on d5 against the pawn on d6 at its 15th
/// ply, until the knight is taken at the 16th.
const NAJDORF: &str = "e2e4 c7c5 g1f3 d7d6 d2d4 c5d4 f3d4 g8f6 b1c3 a7a6 c1e3 e7e5 d4b3 f8e7 c3d5 f6d5 e4d5 e8g8";
/// A Queen's Gambit exchange: the Carlsbad structure from its 10th ply on.
const CARLSBAD: &str =
    "d2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 c4d5 e6d5 e2e3 c7c6 f1d3 b8d7 d1c2 e8g8 g1f3 f8e8 e1g1 d7f8";
/// A Greek gift: a white bishop on h7 at the 15th ply, taken at the 16th.
const GREEK_GIFT: &str = "e2e4 e7e6 d2d4 d7d5 b1c3 g8f6 c1g5 f8e7 e4e5 f6d7 h2h4 e8g8 f1d3 c7c5 d3h7 g8h7";
/// A black bishop on h2 at the 6th ply: the Greek gift's bishop mirrored
/// across the middle rank, colours changed.
const BISHOP_H2: &str = "h2h4 e7e6 h1h3 f8d6 h3a3 d6h2 a3a4";
/// A rook endgame, set up.
const ROOK_ENDING: &str = "4k3/r7/8/8/8/8/R7/4K3 w - - 0 1";

/// A record of the fixture: its start, the standard one without, its moves
/// in UCI, and whether it is deleted.
struct Game {
    start: Option<&'static str>,
    ucis: String,
    deleted: bool,
}

impl Game {
    fn board(&self) -> Board {
        self.start.map_or_else(Board::startpos, |f| Board::from_fen(f).unwrap())
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

/// `games` lines of legal moves drawn from `seed`, 20 to 160 plies each,
/// captures preferred, so that some reach endgames.
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
            for _ in 0..20 + next() % 141 {
                let moves = board.legal_moves();
                if moves.is_empty() {
                    break;
                }
                let captures: Vec<_> = moves.iter().copied().filter(|m| board.piece_at(m.to).is_some()).collect();
                let pool = if !captures.is_empty() && next() % 3 == 0 { &captures } else { &moves };
                let mv = pool[(next() % pool.len() as u64) as usize];
                ucis.push(bridge::explorer::uci(&board, mv));
                board.play_checked(mv).unwrap();
            }
            ucis.join(" ")
        })
        .collect()
}

/// Knights out and back, `n` times: every pawn stays home.
fn hops(n: usize) -> String {
    "g1f3 g8f6 f3g1 f6g8 ".repeat(n)
}

/// The records of the fixture, numbered from 1: the games above, a deleted
/// Najdorf, knights hopping to move 13, and 160 random games.
fn games() -> Vec<Game> {
    let standard = |ucis: &str| Game { start: None, ucis: ucis.to_string(), deleted: false };
    let mut games = vec![
        standard(NAJDORF),
        standard(CARLSBAD),
        standard(GREEK_GIFT),
        standard(BISHOP_H2),
        Game { start: Some(ROOK_ENDING), ucis: "a2a3 a7a6 e1e2 e8e7 a3a4".into(), deleted: false },
        Game { start: None, ucis: NAJDORF.into(), deleted: true },
        standard(&format!("{}e2e4", hops(6))),
    ];
    games.extend(random_lines(160, 0x0f4a_6272_5eed).iter().map(|l| standard(l)));
    games
}

/// The 2CBH move record of `g`.
fn move_record(g: &Game) -> Vec<u16> {
    let mut stream = Vec::new();
    if let Some(fen) = g.start {
        let board = Board::from_fen(fen).unwrap();
        let side = u16::from(board.side_to_move() == CColor::Black);
        stream.extend([movetable::START_POSITION, 1, side, 0]);
        for i in 0..64u8 {
            if let Some((p, c)) = board.piece_at(Square::from_index(i).unwrap()) {
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

/// White's rating in game `n`: from 1800 to 2499, by turns.
fn white_elo(n: u32) -> u32 {
    1800 + n * 37 % 700
}

/// The fixture's records written as a 2CBH database named `name`.
fn database(name: &str, games: &[Game]) -> TempDb {
    let mut b = Builder::new();
    for (n, g) in (1u32..).zip(games) {
        let at = b.moves(1, &move_record(g));
        let rec = b.game(at);
        if g.deleted {
            rec[0] |= 0x80;
        }
        put(rec, 0x18, &1i32.to_le_bytes());
        put(rec, 0x20, &2i32.to_le_bytes());
        put(rec, 0x60, &(white_elo(n) as i16).to_le_bytes());
        put(rec, 0x8a, &((g.ucis.split_whitespace().count() as i16 + 1) / 2).to_le_bytes());
    }
    b.lid(lid(&["".into(), "Alpha, Ann".into(), "Beta, Bob".into()], &[String::new()], &[String::new()]));
    b.write(name)
}

/// A filter as the test reads it, apart from the bridge's own reading: its
/// parameters, and the brute force that finds its games.
struct Want {
    params: Vec<(&'static str, &'static str)>,
}

/// A piece a board lists: its colour, its kind and its square.
type Man = (CColor, CPiece, Square);

fn men(text: &str) -> Vec<Man> {
    text.split(',')
        .filter(|t| !t.is_empty())
        .map(|t| {
            let letter = t.as_bytes()[0];
            let color = if letter.is_ascii_uppercase() { CColor::White } else { CColor::Black };
            let piece = match letter.to_ascii_uppercase() {
                b'K' => CPiece::King,
                b'Q' => CPiece::Queen,
                b'R' => CPiece::Rook,
                b'B' => CPiece::Bishop,
                b'N' => CPiece::Knight,
                _ => CPiece::Pawn,
            };
            (color, piece, t[1..].parse().unwrap())
        })
        .collect()
}

fn flip(sq: Square, files: bool, ranks: bool) -> Square {
    let (f, r) = (sq.file(), sq.rank());
    Square::new(if files { 7 - f } else { f }, if ranks { 7 - r } else { r })
}

impl Want {
    fn new(params: &[(&'static str, &'static str)]) -> Want {
        Want { params: params.to_vec() }
    }

    fn param(&self, name: &str) -> &'static str {
        self.params.iter().find(|(n, _)| *n == name).map_or("", |(_, v)| v)
    }

    fn query(&self) -> String {
        self.params.iter().map(|(n, v)| format!("&{n}={v}")).collect()
    }

    /// Whether `board` holds one form of the fragment: the listed one, or a
    /// mirror `mirror` asks for.
    fn fragment_holds(&self, board: &Board) -> bool {
        let at = |sq: Square| board.piece_at(sq);
        let forms: &[(bool, bool)] = match self.param("mirror") {
            "horizontal" => &[(false, false), (true, false)],
            "vertical" => &[(false, false), (false, true)],
            "both" => &[(false, false), (true, false), (false, true), (true, true)],
            _ => &[(false, false)],
        };
        forms.iter().any(|&(files, ranks)| {
            let man = |&(c, p, sq): &Man| {
                let c = if ranks { !c } else { c };
                at(flip(sq, files, ranks)) == Some((p, c))
            };
            let point = |text: &str, color: CColor| {
                text.split(',').filter(|t| !t.is_empty()).all(|t| {
                    let color = if ranks { !color } else { color };
                    at(flip(t.parse().unwrap(), files, ranks)).is_none_or(|(_, c)| c != color)
                })
            };
            let or = men(self.param("or"));
            men(self.param("look")).iter().all(man)
                && point(self.param("nowhite"), CColor::White)
                && point(self.param("noblack"), CColor::Black)
                && !men(self.param("exclude")).iter().any(man)
                && (or.is_empty() || or.iter().any(man))
        })
    }

    /// Whether `board` holds the material: each kind named counts within its
    /// range.
    fn material_holds(&self, board: &Board) -> bool {
        self.param("material").split(',').filter(|t| !t.is_empty()).all(|t| {
            let (&(color, piece, _), range) = (&men(&format!("{}a1", &t[..1]))[0], &t[1..]);
            let (low, high) = match range.split_once("..") {
                Some((l, h)) => (l.parse().unwrap_or(0), h.parse().unwrap_or(16)),
                None => (range.parse().unwrap(), range.parse().unwrap()),
            };
            let n =
                (0..64u8).filter(|&i| board.piece_at(Square::from_index(i).unwrap()) == Some((piece, color))).count();
            (low..=high).contains(&n)
        })
    }

    /// The ply of the first stretch of `length` positions of `g` that hold
    /// the filter at move numbers from `first` to `last`; `None` for none.
    fn first_match(&self, g: &Game) -> Option<u32> {
        let number = |name: &str, default: u32| self.param(name).parse().unwrap_or(default);
        let (first, last, length) = (number("first", 1), number("last", 999), number("length", 1));
        let mut run: Option<u32> = None;
        for (ply, board) in (0u32..).zip(g.positions()) {
            let move_number = u32::from(board.fullmove_number());
            if (first..=last).contains(&move_number) && self.material_holds(&board) && self.fragment_holds(&board) {
                let from = *run.get_or_insert(ply);
                if ply - from + 1 >= length {
                    return Some(from);
                }
            } else {
                run = None;
            }
        }
        None
    }

    /// The games of the fixture that match, by number, with the ply of each
    /// one's first match: deleted games never do.
    fn games(&self, games: &[Game]) -> BTreeMap<u32, u32> {
        (1u32..).zip(games).filter(|(_, g)| !g.deleted).filter_map(|(n, g)| Some((n, self.first_match(g)?))).collect()
    }
}

/// The filters the oracle checks: each board, the points, mirrors, the
/// window, the stretch and material, alone and together.
fn wants() -> Vec<Want> {
    vec![
        Want::new(&[("look", "Nd5,pd6")]),
        Want::new(&[("look", "Nd5,pd6"), ("mirror", "horizontal")]),
        Want::new(&[("look", "Bh7")]),
        Want::new(&[("look", "Bh7"), ("mirror", "vertical")]),
        Want::new(&[("look", "Bh7"), ("mirror", "both")]),
        Want::new(&[("look", "Pd4,Pe3,pd5,pc6"), ("nowhite", "c2,c4,c5"), ("noblack", "e6,e5")]),
        // The start position holds it: every game matches at ply 0.
        Want::new(&[("nowhite", "e4"), ("noblack", "e4")]),
        Want::new(&[("or", "Qh5,Qa4,qh4,qa5")]),
        Want::new(&[("look", "Ke1"), ("exclude", "Pe4,Ne4,Be4,Qe4"), ("first", "5")]),
        Want::new(&[("look", "Pe4"), ("exclude", "pe5,pd5,Nf3")]),
        Want::new(&[("look", "Pe4"), ("first", "3"), ("last", "10"), ("length", "6")]),
        Want::new(&[("look", "Nf3"), ("length", "12")]),
        Want::new(&[("look", "Pa2,Pb2,Pc2,Pd2,Pe2,Pf2,Pg2,Ph2"), ("first", "12")]),
        Want::new(&[("material", "Q0,q0")]),
        Want::new(&[("material", "R1,r1,Q0,q0,B0,b0,N0,n0")]),
        Want::new(&[("material", "P..4,p..4"), ("first", "20")]),
        // The rook ending's white rook on a3 is this one mirrored a↔h.
        Want::new(&[("look", "Rh3"), ("material", "R1,r1,P0,p0"), ("mirror", "both")]),
        Want::new(&[("or", "Ke2,ke7"), ("material", "Q0"), ("length", "3")]),
    ]
}

/// The path of the list of the games of database `id` that `want` finds,
/// with `extra` parameters.
fn list(id: &str, want: &Want, extra: &str) -> String {
    format!("/v1/databases/{id}/games?limit=500{}{extra}", want.query())
}

/// Each row's number and the ply of its match, in order.
fn rows(body: &str) -> Vec<(u32, Option<u32>)> {
    objects(body, "rows")
        .iter()
        .map(|row| {
            let digits = row.strip_prefix(r#"{"number":"#).unwrap_or_else(|| panic!("no number first in {row}"));
            let number = digits[..digits.find(',').unwrap()].parse().unwrap();
            let ply = row.find(r#""match":{"ply":"#).map(|at| {
                let rest = &row[at + 15..];
                rest[..rest.find('}').unwrap()].parse().unwrap()
            });
            (number, ply)
        })
        .collect()
}

/// The fixture served from a data folder of its own, the index built: the
/// games, the database, the bridge and its id, the masks switched off when
/// `masks_off`.
fn served(name: &str, masks_off: bool) -> (Vec<Game>, TempDb, TestBridge, String, PathBuf) {
    let games = games();
    let db = database(&format!("fragments-{name}"), &games);
    let dir = index_dir(name);
    let (bridge, id) = TestBridge::database(&db, &dir);
    bridge.app.catalog.explorer.set_masks_off(masks_off);
    answered(bridge.port, &format!("/v1/databases/{id}/games?fen={}", fen_param(START)));
    (games, db, bridge, id, dir)
}

/// Every filter lists exactly the games a brute-force replay finds, in
/// number order, each once with the ply of its first match, and counts them
/// in `total` and in the acknowledgement: with the masks, which rule games
/// out first, and without them, which replays every game.
#[test]
fn every_filter_lists_the_games_a_replay_finds_with_masks_and_without() {
    for masks_off in [false, true] {
        let (games, _db, bridge, id, dir) = served(if masks_off { "oracle-plain" } else { "oracle" }, masks_off);
        for want in wants() {
            let expected = want.games(&games);
            let body = answered(bridge.port, &list(&id, &want, ""));
            let got: BTreeMap<u32, u32> =
                rows(&body).into_iter().map(|(n, ply)| (n, ply.unwrap_or_else(|| panic!("no ply: {body}")))).collect();
            assert_eq!(got, expected, "{}", want.query());
            let numbers: Vec<u32> = rows(&body).iter().map(|r| r.0).collect();
            assert!(numbers.windows(2).all(|w| w[0] < w[1]), "in number order, each once: {body}");
            let n = expected.len();
            assert!(body.contains(&format!(r#""total":{n},"#)), "{body}");
            assert!(body.contains(&format!(r#""games":{n}}},"rows":["#)), "{body}");
            assert!(n < games.len() || want.param("nowhite") == "e4", "{} narrows nothing", want.query());
            assert!(n > 0, "{} finds games: the oracle compares lists, not empty answers", want.query());
        }
        let masks = masks::path_of(&paths(&dir.join("index"), &id).0);
        assert_eq!(masks.exists(), !masks_off, "the masks are kept beside the index, when used");
        drop(bridge);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The first fragment search on a database answers `409` while the masks are
/// built, with their own phase, and the same search answers once they are.
/// The acknowledgement writes each parameter in one form, with the window.
#[test]
fn the_masks_are_built_on_the_first_search_and_acknowledged() {
    let (games, _db, bridge, id, dir) = served("first", false);
    let want = Want::new(&[("look", "Nd5,pd6"), ("mirror", "horizontal")]);
    // Written in another order, with a space.
    let path = format!("/v1/databases/{id}/games?look=%20pd6,Nd5&mirror=horizontal");
    let (status, body) = get(bridge.port, &path);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""code":"database_unavailable""#) && body.contains(r#""state":"indexing""#), "{body}");
    assert!(body.contains(r#""phase":"masks""#), "{body}");
    let body = answered(bridge.port, &path);
    let n = want.games(&games).len();
    assert!(
        body.contains(&format!(
            r#""fragment":{{"look":"Nd5,pd6","nowhite":"","noblack":"","or":"","exclude":"","material":"","mirror":"horizontal","first":1,"last":999,"length":1,"games":{n}}}"#
        )),
        "{body}"
    );
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// With `q`, a game must match both; `sort`, `offset` and `limit` apply as
/// without a fragment; `fen` with one is refused, as are its own parameters
/// and Chess960; and a list without one is as it was.
#[test]
fn a_fragment_combines_with_the_other_parameters() {
    let (games, _db, bridge, id, dir) = served("combined", false);
    let want = Want::new(&[("material", "Q0,q0")]);
    let all = want.games(&games);
    answered(bridge.port, &list(&id, &want, ""));
    // `q`: the acknowledgement counts the fragment's games before it.
    let body = answered(bridge.port, &list(&id, &want, "&q=whiteelo%3A2200.."));
    let rated: Vec<u32> = all.keys().copied().filter(|&n| white_elo(n) >= 2200).collect();
    assert_eq!(rows(&body).iter().map(|r| r.0).collect::<Vec<_>>(), rated, "{body}");
    assert!(body.contains(&format!(r#""total":{},"#, rated.len())), "{body}");
    assert!(body.contains(&format!(r#""games":{}}},"rows":["#, all.len())), "{body}");
    // `sort`, `offset`, `limit`.
    let body =
        answered(bridge.port, &format!("/v1/databases/{id}/games?material=Q0,q0&sort=number-desc&offset=2&limit=3"));
    let window: Vec<(u32, Option<u32>)> = all.iter().rev().skip(2).take(3).map(|(&n, &p)| (n, Some(p))).collect();
    assert_eq!(rows(&body), window, "{body}");
    // Refusals, each naming its parameter.
    for (query, parameter) in [
        (format!("fen={}&look=Pe4", fen_param(START)), "fen"),
        ("look=Xe4".to_string(), "look"),
        ("material=Q1..0".to_string(), "material"),
        ("first=3".to_string(), "first"),
        ("look=Pe4&length=0".to_string(), "length"),
    ] {
        let (status, body) = get(bridge.port, &format!("/v1/databases/{id}/games?{query}"));
        assert_eq!(status, 400, "{query}: {body}");
        assert!(body.contains(&format!(r#""parameter":"{parameter}""#)), "{query}: {body}");
    }
    let (status, body) = get(bridge.port, &format!("/v1/databases/{id}/games?look=Pe4&variant=chess960"));
    assert_eq!(status, 422, "{body}");
    // A list without a fragment: no acknowledgement and no ply.
    let body = answered(bridge.port, &format!("/v1/databases/{id}/games?limit=500"));
    assert!(!body.contains("fragment") && !body.contains("\"match\""), "{body}");
    assert!(body.contains(&format!(r#""total":{},"#, games.len())), "{body}");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A change to the database makes its masks stale: they are built again
/// with its new index, before any search asks for them, and the next search
/// finds the new games.
#[test]
fn a_changed_database_builds_its_masks_again_with_its_index() {
    let mut games = games();
    let want = Want::new(&[("look", "Nd5,pd6")]);
    let db = database("fragments-changed", &games);
    let path = db.dir().join("db.2cbh");
    let dir = index_dir("changed");
    let (bridge, id) = TestBridge::database(&db, &dir);
    assert_eq!(id, id_of(&path));
    let body = answered(bridge.port, &list(&id, &want, ""));
    assert_eq!(rows(&body).len(), want.games(&games).len(), "{body}");
    let masks = masks::path_of(&paths(&dir.join("index"), &id).0);
    let before = std::fs::read(&masks).unwrap();
    // The bridge lets go of its files before the database changes under them.
    drop(bridge);
    games.push(Game { start: None, ucis: NAJDORF.into(), deleted: false });
    let changed = database("fragments-changed", &games);
    assert_eq!(changed.dir(), db.dir());
    let (bridge, _) = TestBridge::database(&changed, &dir);
    // A position's games build the new index, and the masks after it.
    answered(bridge.port, &format!("/v1/databases/{id}/games?fen={}", fen_param(START)));
    settle(&bridge.app.catalog);
    let after = std::fs::read(&masks).unwrap();
    assert_ne!(after, before, "the masks were built again with the index");
    let (status, body) = get(bridge.port, &list(&id, &want, ""));
    assert_eq!(status, 200, "the new masks answer at once: {body}");
    let expected = want.games(&games);
    assert!(expected.contains_key(&(games.len() as u32)), "the new game matches");
    assert_eq!(rows(&body).into_iter().map(|(n, p)| (n, p.unwrap())).collect::<BTreeMap<_, _>>(), expected, "{body}");
    drop(bridge);
    std::fs::remove_dir_all(&dir).unwrap();
}
