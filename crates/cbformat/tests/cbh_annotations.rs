//! Classic (`.cba`) annotations: decoded, placed by stored-order position and
//! written as PGN exactly as 2CBH annotations are.

use cbformat::cbh::{Database, annotations};
use cbformat::fixture_cbh::{Builder, Tok, annotation_record, encode, move_record};
use cbformat::pgn::{self, AnnotationStatus, Options};
use cbformat::v2::{Annotation, Arrow, Square, language};
use chesscore::Board;

use Tok::{End as E, Mv as M, Var as V};

/// `1.e4 c5 (1...c6 2.d4) (1...Nf6 2.e5) 2.Nf3 d6 (2...Nc6 3.Bb5) 3.d4`, the
/// format description's example, in stored order.
fn documented() -> Vec<u8> {
    let toks = [
        M("e2e4"),
        V,
        M("c7c5"),
        M("g1f3"),
        V,
        M("d7d6"),
        M("d2d4"),
        E,
        M("b8c6"),
        M("f1b5"),
        E,
        V,
        M("c7c6"),
        M("d2d4"),
        E,
        M("g8f6"),
        M("e4e5"),
        E,
    ];
    move_record(0, None, None, &encode(&Board::startpos(), &toks, 0, false))
}

fn one_move() -> Vec<u8> {
    move_record(0, None, None, &encode(&Board::startpos(), &[M("e2e4"), E], 0, false))
}

/// English text `s` after the move.
fn after(s: &str) -> Vec<u8> {
    let mut d = vec![0, 42];
    d.extend(s.as_bytes());
    d
}

/// A one-game database with move record `moves` and annotation items `items`.
fn db(name: &str, moves: &[u8], items: &[(i32, u8, &[u8])]) -> (cbformat::fixture::TempDb, Database) {
    let mut b = Builder::new();
    b.game(moves);
    b.annotations(&annotation_record(1, items));
    let f = b.write(name);
    let db = Database::open(f.base()).unwrap();
    (f, db)
}

fn movetext(db: &Database, options: &Options) -> (String, AnnotationStatus) {
    let r = pgn::classic_game_with(db, 1, options).unwrap();
    let text = r.pgn.split("\n\n").nth(1).unwrap().trim_end().trim_end_matches("1-0").trim_end().to_string();
    (text, r.annotations)
}

#[test]
fn positions_count_in_stored_order() {
    let texts: Vec<Vec<u8>> = (0..11).map(|p| after(&format!("s{p}"))).collect();
    let mut items: Vec<(i32, u8, &[u8])> = vec![(-1, 0x02, b"\x00\x2agame")];
    items.extend(texts.iter().enumerate().map(|(p, t)| (p as i32, 0x02, t.as_slice())));
    let (_f, db) = db("stored-order", &documented(), &items);
    let (text, status) = movetext(&db, &Options::default());
    assert_eq!(
        text,
        "{game} 1. e4 {s0} 1... c5 {s1} (1... c6 {s7} 2. d4 {s8}) (1... Nf6 {s9} 2. e5 {s10}) \
         2. Nf3 {s2} 2... d6 {s3} (2... Nc6 {s5} 3. Bb5 {s6}) 3. d4 {s4}"
    );
    assert_eq!(status, AnnotationStatus::Complete);
}

#[test]
fn symbols_graphics_languages_and_text_before() {
    let items: Vec<(i32, u8, &[u8])> = vec![
        (0, 0x82, b"\x00\x2abefore"),
        (0, 0x03, &[1]),
        (0, 0x04, &[2, 29, 4, 36]),
        (0, 0x05, &[3, 7, 22]),
        (0, 0x02, b"\x00\x2aEnglish"),
        (0, 0x02, b"\x00\x35Deutsch"),
        (0, 0x02, b"\x00\x00any"),
        (0, 0x03, &[0, 14, 142]),
    ];
    let (_f, db) = db("kinds", &one_move(), &items);
    let (text, _) = movetext(&db, &Options::default());
    assert_eq!(text, "{before} 1. e4 $1 $14 $142 {[%csl Gd5,Re4][%cal Ya7c6] English any}");
    // The text before the move is English, so a German PGN leaves it out.
    let (text, _) = movetext(&db, &Options::with_languages(["de"]));
    assert_eq!(text, "1. e4 $1 $14 $142 {[%csl Gd5,Re4][%cal Ya7c6] Deutsch any}");

    let a = db.annotations_of(&db.record(1).unwrap()).unwrap().unwrap();
    let anns = &a.blocks[0].annotations;
    assert_eq!(a.blocks.len(), 1, "one block per position");
    assert_eq!(anns[1], Annotation::Symbols { on_move: 1, on_position: 0, prefix: 0 });
    let sq = |s: &str| {
        let b = s.as_bytes();
        cbformat::movetable::from_cb_square((b[0] - b'a') * 8 + b[1] - b'1')
    };
    assert_eq!(
        anns[2],
        Annotation::Squares(vec![Square { colour: 2, square: sq("d5") }, Square { colour: 4, square: sq("e4") }])
    );
    assert_eq!(anns[3], Annotation::Arrows(vec![Arrow { colour: 3, from: sq("a7"), to: sq("c6") }]));
    assert!(matches!(&anns[5], Annotation::Text { language: language::GERMAN, .. }));
    assert!(matches!(&anns[6], Annotation::Text { language: language::ANY, .. }));
}

#[test]
fn languages_follow_the_nation_numbering() {
    for (nation, lang) in [
        (0, language::ANY),
        (42, language::ENGLISH),
        (53, language::GERMAN),
        (49, language::FRENCH),
        (43, language::SPANISH),
        (70, language::ITALIAN),
        (103, language::DUTCH),
        (117, language::PORTUGUESE),
        (116, language::POLISH),
        (55, language::GREEK),
    ] {
        assert_eq!(annotations::language_of(nation), lang, "nation {nation}");
    }
    // Any other nation is a language of its own, never a preferred one.
    assert!(annotations::language_of(145) > 0xff);
}

#[test]
fn every_type_is_skipped_by_its_size() {
    let items: Vec<(i32, u8, &[u8])> =
        vec![(-1, 0x26, &[0, 1, 0, 20, 0, 30]), (0, 0x18, &[2]), (0, 0x7e, &[9, 9, 9]), (0, 0x02, b"\x00\x2akept")];
    let (_f, db) = db("skipped", &one_move(), &items);
    let (text, status) = movetext(&db, &Options::default());
    assert_eq!(text, "1. e4 {kept}");
    assert_eq!(status, AnnotationStatus::Complete, "a classic record is never incomplete");
}

#[test]
fn a_game_without_a_record_and_a_database_without_a_file() {
    let mut b = Builder::new();
    b.game(&one_move());
    let f = b.write("no-record");
    let db = Database::open(f.base()).unwrap();
    assert!(db.has_annotations());
    assert!(db.annotations_of(&db.record(1).unwrap()).unwrap().unwrap().is_empty());
    assert_eq!(movetext(&db, &Options::default()), ("1. e4".to_string(), AnnotationStatus::None));
    std::fs::remove_file(f.dir().join("db.cba")).unwrap();
    let db = Database::open(f.base()).unwrap();
    assert!(!db.has_annotations());
    assert_eq!(db.annotations_of(&db.record(1).unwrap()).unwrap(), None);
    assert_eq!(movetext(&db, &Options::default()), ("1. e4".to_string(), AnnotationStatus::None));
}

/// A change made to a good record.
type Change = dyn Fn(&mut Vec<u8>);

#[test]
fn damaged_records_are_errors() {
    let good = annotation_record(1, &[(0, 0x02, b"\x00\x2atext")]);
    assert!(annotations::parse(&good, 1).is_ok());
    let with = |f: &Change| {
        let mut r = good.clone();
        f(&mut r);
        annotations::parse(&r, 1)
    };
    let damaged: [(&str, &Change); 9] = [
        ("another game", &|r| r[2] = 2),
        ("head bytes", &|r| r[4] = 7),
        ("count", &|r| r[9] = 5),
        ("record size", &|r| r[13] += 1),
        ("item size below its head", &|r| r[19] = 5),
        ("item past the record", &|r| r[19] = 60),
        ("position below -1", &|r| r[14..17].copy_from_slice(&[0xff, 0xff, 0xfe])),
        ("text without language", &|r| {
            r.truncate(20);
            r[18..20].copy_from_slice(&[0, 6]);
            r[13] = 20;
            r[9] = 2;
        }),
        ("short head", &|r| r.truncate(10)),
    ];
    for (what, f) in damaged {
        assert!(with(f).is_err(), "{what}");
    }
    for (t, data) in [(0x03u8, &[][..]), (0x03, &[1, 2, 3, 4]), (0x04, &[2]), (0x04, &[2, 0]), (0x05, &[2, 1, 65])] {
        let r = annotation_record(1, &[(0, t, data)]);
        assert!(annotations::parse(&r, 1).is_err(), "type {t:#x} with {data:?}");
    }
}

#[test]
fn annotations_on_no_move_are_damage() {
    for position in [1, 5, 0x7f_ffff] {
        let (_f, db) = db(&format!("past-{position}"), &one_move(), &[(position, 0x02, b"\x00\x2anote")]);
        let err = pgn::classic_game_with(&db, 1, &Options::default()).unwrap_err();
        assert!(err.to_string().contains("past the last"), "{err}");
    }
}

/// Every single-byte change and every truncation of a record is an error or a
/// decoded set, never a panic; a count or size can only claim what the record
/// holds, so nothing large is allocated.
#[test]
fn mutated_records_never_panic() {
    let rec = annotation_record(
        3,
        &[
            (-1, 0x02, b"\x00\x2agame"),
            (0, 0x03, &[1, 14]),
            (0, 0x04, &[2, 29, 4, 36]),
            (1, 0x05, &[3, 7, 22, 4, 1, 64]),
            (2, 0x82, b"\x00\x35vor"),
            (2, 0x21, &[0, 10, 0, 0, 0, 20]),
        ],
    );
    assert!(annotations::parse(&rec, 3).is_ok());
    for i in 0..rec.len() {
        for v in [0u8, 1, 0x7f, 0x80, 0xfe, 0xff] {
            let mut m = rec.clone();
            m[i] = v;
            let _ = annotations::parse(&m, 3);
        }
        let _ = annotations::parse(&rec[..i], 3);
    }
}

/// Through the database too: a record whose size in its head runs past the
/// file, and one over a size limit.
#[test]
fn record_sizes_are_bounded_by_the_file_and_the_limit() {
    let rec = annotation_record(1, &[(0, 0x02, b"\x00\x2atext")]);
    let mut b = Builder::new();
    b.game(&one_move());
    b.annotations(&rec);
    let f = b.write("sizes");
    let db = Database::open(f.base()).unwrap();
    let r = db.record(1).unwrap();
    assert!(db.annotations_of_within(&r, rec.len()).is_ok());
    assert!(db.annotations_of_within(&r, rec.len() - 1).is_err());
    let path = f.dir().join("db.cba");
    let mut cba = std::fs::read(&path).unwrap();
    cba.truncate(cba.len() - 1);
    std::fs::write(&path, &cba).unwrap();
    assert!(db.annotations_of(&r).is_err());
}
