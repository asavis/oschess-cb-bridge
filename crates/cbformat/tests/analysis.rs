//! Analyses (asavis/oschess-cb-bridge#323): a 2CBH record of a kind of its
//! own, with moves and annotations as a game has, under a header that holds
//! only a title and an author. Its PGN is a game's movetext under the tags the
//! header holds.

use cbformat::Limits;
use cbformat::fixture::{Builder, TempDb, annotations, lid_with, quiet, strings, symbols, text, titles};
use cbformat::game::language;
use cbformat::movetable::{self, Color, Piece};
use cbformat::pgn::{self, AnnotationStatus, Options};
use cbformat::v2::Database;

use Color::{Black as B, White as W};
use Piece::Pawn;

const MOVES: u16 = movetable::MOVES;
const END: u16 = movetable::END_OF_LINE;

/// Record 1 a game and record 2 an analysis, both of `1. e4 e5` with a
/// comment and a symbol on `1. e4`. The analysis's title is game tag 0 and its
/// author player 1. Read through the game layout, its offsets would name
/// player 0 (`Morphy, Paul`) as White and tournament 1 (`Paris m`) as the
/// event.
fn database(name: &str) -> TempDb {
    let mut b = Builder::new();
    let moves = b.moves(1, &[MOVES, quiet(W, Pawn, "e2", "e4"), quiet(B, Pawn, "e7", "e5"), END]);
    let content = annotations(&[(
        0,
        vec![symbols(1, 0, 0), text(false, language::ENGLISH, "Central"), text(false, language::GERMAN, "Zentral")],
    )]);
    let game_annotations = b.annotations(&content);
    b.annotated_game(moves, game_annotations);
    let analysis_annotations = b.annotations(&content);
    let analysis = b.annotated_game(moves, analysis_annotations);
    analysis[2] = 2;
    analysis[0x18..0x20].copy_from_slice(&0i64.to_le_bytes()); // title: game tag 0
    analysis[0x28..0x30].copy_from_slice(&1i64.to_le_bytes()); // author: player 1
    b.lid(lid_with(
        256,
        2,
        &[
            (0, 0, strings(&["Morphy", "Paul"])),
            (0, 1, strings(&["Author", "An"])),
            (1, 1, [strings(&["Paris", "Paris m"]), 0i32.to_le_bytes().to_vec()].concat()),
            (5, 0, titles("1.e4 e5 2.♘f3")),
        ],
    ));
    b.write(name)
}

/// The tags and the movetext of a PGN record.
fn split(pgn: &str) -> (&str, &str) {
    pgn.split_once("\n\n").unwrap()
}

#[test]
fn an_analysis_is_written_as_its_header_holds() {
    let tmp = database("analysis-pgn");
    let db = Database::open(tmp.base()).unwrap();
    for full in [false, true] {
        let options = Options { full, languages: vec![language::ENGLISH] };
        let game = pgn::game_with(&db, 1, &options, Limits::default()).unwrap();
        let analysis = pgn::game_with(&db, 2, &options, Limits::default()).unwrap();
        let (tags, movetext) = split(&analysis.pgn);
        assert_eq!(
            tags,
            "[Event \"1.e4 e5 2.♘f3\"]\n[Site \"?\"]\n[Date \"????.??.??\"]\n[Round \"?\"]\n\
             [White \"?\"]\n[Black \"?\"]\n[Result \"*\"]",
            "full: {full}"
        );
        // The game's movetext, its result `1-0` in place of `*`.
        let (_, game_movetext) = split(&game.pgn);
        assert_eq!(movetext.strip_suffix("*\n"), game_movetext.strip_suffix("1-0\n"), "full: {full}");
        assert!(movetext.contains("Central"), "full: {full}: {movetext}");
        assert_eq!(analysis.annotations, AnnotationStatus::Complete);
    }
}

/// An analysis whose title tag is unused is `Event "?"`, as a game with no
/// tournament is.
#[test]
fn an_analysis_without_a_title_has_an_unknown_event() {
    let mut b = Builder::new();
    let moves = b.moves(1, &[MOVES, quiet(W, Pawn, "e2", "e4"), END]);
    let analysis = b.game(moves);
    analysis[2] = 2;
    analysis[0x18..0x20].copy_from_slice(&7i64.to_le_bytes());
    let tmp = b.write("analysis-untitled");
    let db = Database::open(tmp.base()).unwrap();
    let pgn = pgn::game_with(&db, 1, &Options::default(), Limits::default()).unwrap().pgn;
    assert!(pgn.starts_with("[Event \"?\"]\n"), "{pgn}");
    assert!(pgn.ends_with("\n\n1. e4 *\n"), "{pgn}");
}

/// A guiding text is still not written as PGN.
#[test]
fn a_guiding_text_is_not_a_game() {
    let mut b = Builder::new();
    let moves = b.moves(1, &[MOVES, END]);
    b.game(moves)[0] |= 2;
    let tmp = b.write("analysis-text");
    let db = Database::open(tmp.base()).unwrap();
    assert!(pgn::game_with(&db, 1, &Options::default(), Limits::default()).is_err());
}
