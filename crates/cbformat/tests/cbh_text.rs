//! A classic database's single-byte text, read in the page its words show,
//! with ChessBase's piece bytes as figurines (#293).

use cbformat::cbh::Database;
use cbformat::codepage::CodePage;
use cbformat::fixture::TempDb;
use cbformat::fixture_cbh::{Builder, Tok, annotation_record, encode, move_record};
use cbformat::game::Annotation;
use chesscore::Board;

const COMPUTERS: [CodePage; 2] = [CodePage::WESTERN, CodePage::CYRILLIC];

fn e4() -> Vec<u8> {
    move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false))
}

/// A text annotation of any language holding `bytes`.
fn text(bytes: &[u8]) -> Vec<u8> {
    [&[0, 0][..], bytes].concat()
}

/// A database of two games whose players are `names`, each game with the
/// texts given.
fn database(name: &str, names: [&[u8]; 2], games: [&[&[u8]]; 2]) -> TempDb {
    let mut b = Builder::new();
    b.player_name(0, names[0]).player_name(1, names[1]);
    for (i, texts) in games.iter().enumerate() {
        b.game(&e4());
        let items: Vec<Vec<u8>> = texts.iter().map(|t| text(t)).collect();
        let items: Vec<(i32, u8, &[u8])> = items.iter().map(|t| (0, 0x02, &t[..])).collect();
        b.annotations(&annotation_record(i as u32 + 1, &items));
    }
    b.write(name)
}

/// The last names of players 0 and 1 and the texts of each game.
fn read(f: &TempDb, computer: CodePage) -> (Vec<String>, Vec<Vec<String>>) {
    let db = Database::open_in(f.dir().join("db.cbh"), computer).unwrap();
    let e = db.entities();
    let names = (0..2).map(|id| e.player(id).unwrap().unwrap().last).collect();
    let texts = (1..=2)
        .map(|id| {
            let r = db.record(id).unwrap();
            let a = db.annotations_of(&r).unwrap().unwrap();
            a.blocks
                .iter()
                .flat_map(|b| &b.annotations)
                .filter_map(|a| match a {
                    Annotation::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        })
        .collect();
    (names, texts)
}

/// A Russian book reads alike on a Cyrillic and on a Western computer. A text
/// whose own words show no page (`1.49а`, `и т.д.`, `и`) is read as the
/// game's other texts show, else as the database's names show.
#[test]
fn a_russian_database_reads_alike_on_either_computer() {
    let f = database(
        "text-russian",
        // `Петров`, and `1.49а`, which shows nothing alone.
        [b"\xcf\xe5\xf2\xf0\xee\xe2", b"1.49\xe0"],
        [
            // `Лучше 3...¤d7 и т.д.`, then `и т.д.` alone.
            &[b"\xcb\xf3\xf7\xf8\xe5 3...\xa4d7 \xe8 \xf2.\xe4.", b"\xe8 \xf2.\xe4."],
            // `и`, alone in its game.
            &[b"\xe8"],
        ],
    );
    for computer in COMPUTERS {
        let (names, texts) = read(&f, computer);
        assert_eq!(names, ["Петров", "1.49а"], "{computer:?}");
        assert_eq!(texts, [vec!["Лучше 3...♘d7 и т.д.", "и т.д."], vec!["и"]], "{computer:?}");
    }
}

/// A Western database reads alike on either computer too.
#[test]
fn a_western_database_reads_alike_on_either_computer() {
    let f = database(
        "text-western",
        [b"M\xfcller", b"1.49\xe0"],
        [&[b"Diese Partie ist ein Beispiel f\xfcr die Schw\xe4che, \xa5xf3", b"\xe0"], &[b"\xe0"]],
    );
    for computer in COMPUTERS {
        let (names, texts) = read(&f, computer);
        assert_eq!(names, ["Müller", "1.49à"], "{computer:?}");
        assert_eq!(texts, [vec!["Diese Partie ist ein Beispiel für die Schwäche, ♗xf3", "à"], vec!["à"]]);
    }
}

/// Where neither the text nor the database shows a page, the computer's
/// decides. On a computer of another page the text is read in it as it
/// stands, without figurines: Windows-1250 has Ł at 0xa3.
#[test]
fn the_computer_decides_what_nothing_shows() {
    let f = database("text-computer", [b"Anand", b"Kasparov"], [&[b"\xe8"], &[b"\xa3\xf3d\x9f"]]);
    // `ód` shows Western, whatever the computer, and `£` before a letter
    // is no piece.
    assert_eq!(read(&f, CodePage::WESTERN).1, [vec!["è"], vec!["£ód\u{178}"]]);
    assert_eq!(read(&f, CodePage::CYRILLIC).1, [vec!["и"], vec!["£ód\u{178}"]]);
    assert_eq!(read(&f, CodePage::new(1250)).1, [vec!["č"], vec!["Łódź"]]);
    // `open` reads as a Western computer.
    let db = Database::open(f.dir().join("db.cbh")).unwrap();
    let r = db.record(1).unwrap();
    let a = db.annotations_of(&r).unwrap().unwrap();
    assert!(matches!(&a.blocks[0].annotations[0], Annotation::Text { text, .. } if text == "è"));
}

/// Where the names show nothing, a short text is read as its game's other
/// texts show, and a game without them leaves it to the computer.
#[test]
fn a_short_text_is_read_as_its_game_shows() {
    let f = database(
        "text-game",
        [b"Anand", b"Kasparov"],
        // `Лучше 3...¤d7`, then `и т.д.`; and `и т.д.` in a game of its own.
        [&[b"\xcb\xf3\xf7\xf8\xe5 3...\xa4d7", b"\xe8 \xf2.\xe4."], &[b"\xe8 \xf2.\xe4."]],
    );
    assert_eq!(read(&f, CodePage::WESTERN).1, [vec!["Лучше 3...♘d7", "и т.д."], vec!["è ò.ä."]]);
    assert_eq!(read(&f, CodePage::CYRILLIC).1, [vec!["Лучше 3...♘d7", "и т.д."], vec!["и т.д."]]);
}

/// Words that could be either page leave the decision to their context: a
/// Western name and a French comment stay Western on a Western computer, and
/// a lone Russian move with a Latin K stays Cyrillic on a Cyrillic one.
#[test]
fn short_words_keep_their_context() {
    let f = database(
        "text-short",
        [b"S\xfc\xdf", b"Anderssen"],
        // `Cet été` (French), and `1.Kрg1` with a Latin K.
        [&[b"Cet \xe9t\xe9"], &[b"1.K\xf0g1"]],
    );
    let (names, texts) = read(&f, CodePage::WESTERN);
    assert_eq!(names, ["Süß", "Anderssen"]);
    assert_eq!(texts[0], ["Cet été"]);
    assert_eq!(read(&f, CodePage::CYRILLIC).1[1], ["1.Kрg1"]);
}

/// Ukrainian names keep their capital Ґ, which is ChessBase's bishop byte, on
/// either computer.
#[test]
fn ukrainian_names_keep_their_capital_ghe() {
    // `Ґалаґан` and `Ґаєвський` in Windows-1251; є is a letter below 0xc0.
    let names: [&[u8]; 2] = [b"\xa5\xe0\xeb\xe0\xb4\xe0\xed", b"\xa5\xe0\xba\xe2\xf1\xfc\xea\xe8\xe9"];
    let f = database("text-ghe", names, [&[], &[]]);
    for computer in COMPUTERS {
        assert_eq!(read(&f, computer).0, ["Ґалаґан", "Ґаєвський"], "{computer:?}");
    }
}
