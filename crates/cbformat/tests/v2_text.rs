//! 2CBH text that is not UTF-8: Russian in Windows-1251 reads as Russian, and
//! any other text as before (#308).

use cbformat::fixture::{Builder, TempDb, annotations, lid_header, quiet};
use cbformat::game::Annotation;
use cbformat::movetable::{self, Color, Piece};
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
