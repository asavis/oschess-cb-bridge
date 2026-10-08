//! 2CBH text that is not UTF-8: Russian in Windows-1251 reads as Russian, and
//! any other text as before (#308); ChessBase's signs read as Unicode (#309);
//! UTF-8 mixed into single-byte text reads as UTF-8 (#311).

use cbformat::fixture::{Builder, TempDb, annotations, lid_header, quiet, rendered};
use cbformat::game::Annotation;
use cbformat::movetable::{self, Color, Piece};
use cbformat::pgn::Options;
use cbformat::view::Base;

/// A text annotation of any language holding `bytes` as they are.
fn raw_text(bytes: &[u8]) -> Vec<u8> {
    let mut v = 0x02u16.to_le_bytes().to_vec();
    v.extend([0, 0]);
    v.extend(7u16.to_le_bytes());
    v.extend((bytes.len() as i32).to_le_bytes());
    v.extend(bytes);
    v
}

/// One game, 1.e4, with the comments `texts`, its players both the one
/// player whose last name is `last`.
fn database(name: &str, texts: &[&[u8]], last: &[u8]) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    let comment = b.annotations(&annotations(&[(0, texts.iter().map(|t| raw_text(t)).collect())]));
    b.annotated_game(e4, comment);
    let mut player = Vec::new();
    player.extend((last.len() as i32).to_le_bytes());
    player.extend(last);
    player.extend(0i32.to_le_bytes());
    let mut lid = lid_header((4 + player.len()).max(1024) as i32, 1);
    lid.extend((player.len() as i32).to_le_bytes());
    lid.extend(&player);
    b.lid(lid);
    b.write(&format!("v2-text-{name}"))
}

/// The game's texts and its white player's last name.
fn read(f: &TempDb) -> (Vec<String>, String) {
    let db = Base::open(f.dir().join("db.2cbh")).unwrap();
    let h = db.header(1).unwrap();
    let texts = db
        .annotations_of(&h)
        .unwrap()
        .unwrap()
        .blocks
        .iter()
        .flat_map(|b| &b.annotations)
        .filter_map(|a| match a {
            Annotation::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    (texts, db.names(&h).unwrap().white.unwrap().last)
}

#[test]
fn russian_in_windows_1251_reads_as_russian() {
    // `Опять то же самое` and `Петров`, in Windows-1251.
    let f = database(
        "russian",
        &[b"\xce\xef\xff\xf2\xfc \xf2\xee \xe6\xe5 \xf1\xe0\xec\xee\xe5", "Ход белых".as_bytes()],
        b"\xcf\xe5\xf2\xf0\xee\xe2",
    );
    let (texts, white) = read(&f);
    assert_eq!(texts, ["Опять то же самое", "Ход белых"]);
    assert_eq!(white, "Петров");
}

/// Western text and names read as before: Windows-1252 whatever their words
/// show, a lone `à` included, and ChessBase's piece bytes as they stand.
#[test]
fn other_text_reads_as_before() {
    let f = database("western", &[b"f\xfcr die Schw\xe4che", b"\xe0", b"\xa5xf3"], b"M\xfcller");
    let (texts, white) = read(&f);
    assert_eq!(texts, ["für die Schwäche", "à", "¥xf3"]);
    assert_eq!(white, "Müller", "no longer M�ller");
}

/// ChessBase's signs: a comment keeps them as the record holds them, PGN
/// shows them as Unicode and its full form keeps the original, and a name's
/// letters that ChessBase took for signs read as the letters (#309). UTF-8
/// mixed into a single-byte comment reads as UTF-8 (#311). Made-up text.
#[test]
fn chessbase_signs_and_utf8_inside_single_bytes() {
    let f = database(
        "signs",
        &["the \u{e028}d4 is strong /\u{e00a}".as_bytes(), b"8\xe2\x80\xa6d6 9.\xa4e2", b"\xc4\x8cern\xfd tah"],
        "Кл\u{e008}\u{e009}ко".as_bytes(),
    );
    let (texts, white) = read(&f);
    assert_eq!(texts, ["the \u{e028}d4 is strong /\u{e00a}", "8…d6 9.¤e2", "Černý tah"]);
    assert_eq!(white, "Ключко");
    let (reading, _) = rendered(&f, &Options::default());
    assert!(reading.contains("{the ♘d4 is strong /± 8…d6 9.¤e2 Černý tah}"), "{reading}");
    let (full, _) = rendered(&f, &Options { full: true, ..Options::default() });
    assert!(
        full.contains(
            "{[%lang any] the ♘d4 is strong /±} {[%cbtext lang=any;value=the%20%EE%80%A8d4%20is%20strong%20%2F%EE%80%8A]}"
        ),
        "{full}"
    );
    assert!(
        full.contains("{[%lang any] 8…d6 9.¤e2} {[%lang any] Černý tah}"),
        "a text without signs needs no original"
    );
}

/// A Ukrainian name in Windows-1251 whose first two bytes UTF-8 would read as
/// a Latin letter stays Cyrillic (#311). A made-up name.
#[test]
fn a_ukrainian_name_in_windows_1251_is_no_utf8() {
    // `Діденко`: Д and і are 0xc4 0xb3, UTF-8 for ĳ.
    let f = database("ukrainian", &[b"\xc4\xb3\xe4\xe5\xed\xea\xee \xa4d4"], b"\xc4\xb3\xe4\xe5\xed\xea\xee");
    let (texts, white) = read(&f);
    assert_eq!(texts, ["Діденко ¤d4"]);
    assert_eq!(white, "Діденко");
}
