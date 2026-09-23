//! Annotations placed by PGN-order position, with texts, symbols and graphics.

use cbformat::fixture::{Builder, TempDb, annotations, arrows, quiet, squares, symbols, text};
use cbformat::movetable::{self, Color, Piece};
use cbformat::pgn::{self, AnnotationStatus, Options};
use cbformat::v2::{Database, language};

use Color::{Black as B, White as W};
use Piece::{Bishop, Knight, Pawn};

const MOVES: u16 = movetable::MOVES;
const ALT: u16 = movetable::ALTERNATIVE;
const END: u16 = movetable::END_OF_LINE;
const NULL: u16 = movetable::NULL_MOVE;

fn after(i: i32) -> (i32, Vec<Vec<u8>>) {
    (i, vec![text(false, language::ENGLISH, &format!("p{i}"))])
}

/// One game with `words` and annotation record `content`; the movetext and
/// annotation status of its PGN under `options`.
fn render(name: &str, words: &[u16], content: &[u8], options: &Options) -> (String, AnnotationStatus) {
    let db = one_game(name, words, Some(content));
    rendered(&db, options)
}

fn one_game(name: &str, words: &[u16], content: Option<&[u8]>) -> TempDb {
    let mut b = Builder::new();
    let moves = b.moves(1, words);
    match content {
        Some(c) => {
            let a = b.annotations(c);
            b.annotated_game(moves, a);
        }
        None => {
            b.game(moves);
        }
    }
    b.write(name)
}

fn rendered(db: &TempDb, options: &Options) -> (String, AnnotationStatus) {
    let db = Database::open(db.base()).unwrap();
    let r = pgn::game_with(&db, 1, options).unwrap();
    let movetext = r.pgn.split("\n\n").nth(1).unwrap().trim_end().trim_end_matches("1-0").trim_end().to_string();
    (movetext, r.annotations)
}

#[test]
fn positions_follow_pgn_order_through_a_variation() {
    // 1.e4 c5 (1...c6 2.d4) 2.Nf3, stored as e4 c5 Nf3 | c6 d4.
    let words = [
        MOVES,
        quiet(W, Pawn, "e2", "e4"),
        quiet(B, Pawn, "c7", "c5"),
        ALT,
        quiet(W, Knight, "g1", "f3"),
        END,
        quiet(B, Pawn, "c7", "c6"),
        quiet(W, Pawn, "d2", "d4"),
        END,
    ];
    let mut blocks = vec![(-1, vec![text(false, language::ENGLISH, "game")])];
    blocks.extend((0..5).map(after));
    let (movetext, status) = render("pgn-order", &words, &annotations(&blocks), &Options::default());
    assert_eq!(movetext, "{game} 1. e4 {p0} 1... c5 {p1} (1... c6 {p2} 2. d4 {p3}) 2. Nf3 {p4}");
    assert_eq!(status, AnnotationStatus::Complete);
}

#[test]
fn sibling_and_nested_variations() {
    // 1. e4 c5 (1... c6 2. d4) (1... Nf6 2. e5) 2. Nf3 d6 (2... Nc6 3. Bb5) 3. d4
    let words = [
        MOVES,
        quiet(W, Pawn, "e2", "e4"),
        quiet(B, Pawn, "c7", "c5"),
        ALT,
        quiet(W, Knight, "g1", "f3"),
        quiet(B, Pawn, "d7", "d6"),
        ALT,
        quiet(W, Pawn, "d2", "d4"),
        END,
        quiet(B, Knight, "b8", "c6"),
        quiet(W, Bishop, "f1", "b5"),
        END,
        quiet(B, Pawn, "c7", "c6"),
        ALT,
        quiet(W, Pawn, "d2", "d4"),
        END,
        quiet(B, Knight, "g8", "f6"),
        quiet(W, Pawn, "e4", "e5"),
        END,
    ];
    let blocks: Vec<_> = (0..11).map(after).collect();
    let (movetext, _) = render("siblings", &words, &annotations(&blocks), &Options::default());
    assert_eq!(
        movetext,
        "1. e4 {p0} 1... c5 {p1} (1... c6 {p2} 2. d4 {p3}) (1... Nf6 {p4} 2. e5 {p5}) \
         2. Nf3 {p6} 2... d6 {p7} (2... Nc6 {p8} 3. Bb5 {p9}) 3. d4 {p10}"
    );

    // 1. e4 c5 (1... c6 2. d4 (2. Nf3 d5) 2... d5) 2. Nf3: a variation inside one.
    let words = [
        MOVES,
        quiet(W, Pawn, "e2", "e4"),
        quiet(B, Pawn, "c7", "c5"),
        ALT,
        quiet(W, Knight, "g1", "f3"),
        END,
        quiet(B, Pawn, "c7", "c6"),
        quiet(W, Pawn, "d2", "d4"),
        ALT,
        quiet(B, Pawn, "d7", "d5"),
        END,
        quiet(W, Knight, "g1", "f3"),
        quiet(B, Pawn, "d7", "d5"),
        END,
    ];
    let blocks: Vec<_> = (0..8).map(after).collect();
    let (movetext, _) = render("nested", &words, &annotations(&blocks), &Options::default());
    assert_eq!(
        movetext,
        "1. e4 {p0} 1... c5 {p1} (1... c6 {p2} 2. d4 {p3} (2. Nf3 {p4} 2... d5 {p5}) 2... d5 {p6}) 2. Nf3 {p7}"
    );
}

#[test]
fn null_moves_are_positions_too() {
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), NULL, quiet(W, Pawn, "d2", "d4"), END];
    let blocks: Vec<_> = (1..3).map(after).collect();
    let (movetext, _) = render("null", &words, &annotations(&blocks), &Options::default());
    assert_eq!(movetext, "1. e4 -- {p1} 2. d4 {p2}");
}

#[test]
fn symbols_graphics_and_text_before() {
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), quiet(B, Pawn, "e7", "e5"), END];
    let blocks = [
        (
            0,
            vec![
                symbols(1, 18, 142),
                squares(&[(2, "e4"), (4, "d5"), (8, "a1")]),
                arrows(&[(3, "g1", "f3"), (9, "b1", "c3")]),
            ],
        ),
        (1, vec![text(true, language::ENGLISH, "then"), symbols(2, 0, 0)]),
    ];
    let (movetext, _) = render("symbols", &words, &annotations(&blocks), &Options::default());
    // Colours 8 and 9 are unknown and left out.
    assert_eq!(movetext, "1. e4 $1 $18 $142 {[%csl Ge4,Rd5][%cal Yg1f3]} {then} 1... e5 $2");
}

#[test]
fn one_language_per_game() {
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), quiet(B, Pawn, "e7", "e5"), END];
    let blocks = [
        (
            0,
            vec![
                text(false, language::ENGLISH, "en"),
                text(false, language::GERMAN, "de"),
                text(false, language::ANY, "all"),
            ],
        ),
        (1, vec![text(false, language::GERMAN, "de2")]),
    ];
    let content = annotations(&blocks);
    let (default, _) = render("lang-default", &words, &content, &Options::default());
    assert_eq!(default, "1. e4 {en all} 1... e5");
    let (german, _) = render("lang-de", &words, &content, &Options::with_languages(["uk", "de"]));
    assert_eq!(german, "1. e4 {de all} 1... e5 {de2}");
    let (fallback, _) = render("lang-uk", &words, &content, &Options::with_languages(["uk"]));
    assert_eq!(fallback, default);

    // No English: the first language stored.
    let blocks = [(0, vec![text(false, language::FRENCH, "fr"), text(false, language::GERMAN, "de")])];
    let (first, _) = render("lang-first", &words, &annotations(&blocks), &Options::default());
    assert_eq!(first, "1. e4 {fr} 1... e5");
}

#[test]
fn texts_are_cleaned_for_pgn() {
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), END];
    let blocks = [(0, vec![text(false, language::ENGLISH, "a {b}\r\n  c")])];
    let (movetext, _) = render("clean", &words, &annotations(&blocks), &Options::default());
    assert_eq!(movetext, "1. e4 {a (b) c}");
}

#[test]
fn known_types_are_skipped_and_unknown_ones_stop_decoding() {
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), quiet(B, Pawn, "e7", "e5"), END];
    // A critical-position mark (18) between two texts: both texts kept.
    let blocks = [(0, vec![text(false, 0, "a"), vec![0x18, 0, 1], text(false, 0, "b")])];
    let (movetext, status) = render("known", &words, &annotations(&blocks), &Options::default());
    assert_eq!((movetext.as_str(), status), ("1. e4 {a b} 1... e5", AnnotationStatus::Complete));

    // Type 1a has no known layout: what follows it cannot be found.
    let blocks = [(0, vec![text(false, 0, "a"), vec![0x1a, 0, 9, 9], text(false, 0, "b")]), after(1)];
    let (movetext, status) = render("unknown", &words, &annotations(&blocks), &Options::default());
    assert_eq!(movetext, "1. e4 {a} 1... e5");
    assert_eq!(status, AnnotationStatus::Incomplete { type_code: 0x1a });
}

#[test]
fn damaged_records_are_errors() {
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), END];
    let good = annotations(&[(0, vec![text(false, 0, "abc")])]);
    for cut in [0, 4, 9, good.len() - 5, good.len() - 1] {
        let db = one_game(&format!("damaged-{cut}"), &words, Some(&good[..cut]));
        let db = Database::open(db.base()).unwrap();
        assert!(pgn::game(&db, 1).is_err(), "cut at {cut}");
    }
}

#[test]
fn without_an_annotation_file_the_pgn_is_unchanged() {
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), quiet(B, Pawn, "e7", "e5"), END];
    let bare = one_game("bare", &words, None);
    let (movetext, status) = rendered(&bare, &Options::default());
    assert_eq!((movetext.as_str(), status), ("1. e4 e5", AnnotationStatus::None));
    let empty = one_game("empty", &words, Some(&annotations(&[])));
    let (movetext, status) = rendered(&empty, &Options::default());
    assert_eq!((movetext.as_str(), status), ("1. e4 e5", AnnotationStatus::None));
}

#[test]
fn batches_read_the_same_annotations() {
    let mut b = Builder::new();
    let moves = b.moves(1, &[MOVES, quiet(W, Pawn, "e2", "e4"), END]);
    for g in 0..40 {
        if g % 3 == 0 {
            b.game(moves);
        } else {
            let a = b.annotations(&annotations(&[(0, vec![text(false, 0, &format!("game {g}"))])]));
            b.annotated_game(moves, a);
        }
    }
    let f = b.write("batch-annotations");
    let db = Database::open(f.base()).unwrap();
    assert!(db.has_annotations());
    for size in [1, 7, 40] {
        let mut first = 1;
        while first <= 40 {
            let batch = db.batch(first, first + size - 1).unwrap();
            for id in batch.ids() {
                let r = batch.record(id).unwrap();
                assert_eq!(batch.annotations_of(&r).unwrap(), db.annotations_of(&r).unwrap(), "{id}");
            }
            first = batch.ids().end() + 1;
        }
    }
    assert!(db.annotations_of(&db.record(1).unwrap()).unwrap().unwrap().is_empty());
    assert!(!db.annotations_of(&db.record(2).unwrap()).unwrap().unwrap().is_empty());
}

#[test]
fn annotations_on_no_move_are_damage() {
    // A one-move game: positions -1 and 0 exist, nothing else does.
    let words = [MOVES, quiet(W, Pawn, "e2", "e4"), END];
    let english = |t: &str| text(false, language::ENGLISH, t);
    let cases = [
        ("past-end", annotations(&[(0, vec![english("ok")]), (1, vec![english("past the end")])])),
        ("below-game", annotations(&[(-2, vec![english("before the game")])])),
        ("near-max", annotations(&[(2_147_483_646, vec![english("far away")])])),
        // Decoding stops at an unknown layout on a position past the end.
        ("stopped-past-end", annotations(&[(0, vec![english("ok")]), (1, vec![vec![0x1a, 0, 1]])])),
    ];
    for (name, content) in cases {
        let db = one_game(&format!("no-move-{name}"), &words, Some(&content));
        let db = Database::open(db.base()).unwrap();
        let err = pgn::game(&db, 1).unwrap_err().to_string();
        assert!(err.contains("position"), "{name}: {err}");
    }
    // The game comment and the last move are fine.
    let ok = annotations(&[(-1, vec![english("game")]), (0, vec![english("last")])]);
    let (movetext, status) = render("no-move-ok", &words, &ok, &Options::default());
    assert_eq!((movetext.as_str(), status), ("{game} 1. e4 {last}", AnnotationStatus::Complete));
}
