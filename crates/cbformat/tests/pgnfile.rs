//! PGN files read as databases: the index a build writes, the headers, names
//! and texts read through it, and files that are damaged or changed.

use std::path::{Path, PathBuf};

use cbformat::codepage::CodePage;
use cbformat::fixture::{TempDb, pgn_file};
use cbformat::pgnfile::{self, Database, RECORD_SIZE};
use cbformat::v2::{Eco, GameResult};

const GAMES: &str = "\u{feff}[Event \"Paris\"]\r\n[Site \"Paris FRA\"]\r\n[Date \"1858.??.??\"]\r\n[Round \"5.2\"]\r\n\
[White \"Morphy, Paul\"]\r\n[Black \"Duke Karl / Count Isouard\"]\r\n[Result \"1-0\"]\r\n[ECO \"C41\"]\r\n\
[WhiteElo \"2690\"]\r\n[Annotator \"Steinitz,W\"]\r\n\r\n\
1. e4 e5 2. Nf3 d6 {Philidor} (2... Nc6) 3. d4 Bg4 1-0\r\n\r\n\
[Event \"?\"]\n[White \"?\"]\n[Black \"Tal,Mikhail\"]\n[Result \"*\"]\n[Variant \"Chess960\"]\n\
[FEN \"bbqnnrkr/pppppppp/8/8/8/8/PPPPPPPP/BBQNNRKR w HFhf - 0 1\"]\n\n1. f4 *\n\n\
[Event \"Paris\"]\n[Site \"Paris FRA\"]\n[White \"Morphy, Paul\"]\n[Black \"Anderssen, Adolf\"]\n\
[Result \"garbage\"]\n[Date \"1858.12.20\"]\n\n1. e4 c5 2. d4 0-1\n";

fn index_of(db: &TempDb) -> PathBuf {
    db.dir().join("db.idx")
}

fn built(db: &TempDb, stamp: u64, page: CodePage) -> Database {
    let (pgn, index) = (db.dir().join("db.pgn"), index_of(db));
    pgnfile::build(&pgn, &index, stamp, page, &mut |_| true).unwrap();
    Database::open(&pgn, &index, stamp, page).unwrap()
}

#[test]
fn headers_names_and_texts() {
    let f = pgn_file("headers", GAMES.as_bytes());
    let db = built(&f, 7, CodePage::WESTERN);
    assert_eq!(db.record_count(), 3);

    let r = db.record(1).unwrap();
    assert_eq!(db.player(r.white()).unwrap().unwrap().pgn(), "Morphy, Paul");
    // A name without a comma is all last name.
    assert_eq!(db.player(r.black()).unwrap().unwrap().last, "Duke Karl / Count Isouard");
    let t = db.tournament(r.tournament()).unwrap().unwrap();
    assert_eq!((t.title.as_str(), t.place.as_str()), ("Paris", "Paris FRA"));
    assert_eq!(db.annotator(r.annotator()).unwrap().as_deref(), Some("Steinitz,W"));
    assert_eq!(r.played_date().pgn(), "1858.??.??");
    assert_eq!(r.round(), (5, 2));
    assert_eq!(r.elo(), (2690, 0));
    assert_eq!(r.eco().pgn().as_deref(), Some("C41"));
    assert_eq!(r.result(), GameResult::WhiteWins);
    // Three moves: the variation's move is not counted.
    assert_eq!(r.move_count(), 3);
    let text = db.text(&r, 1 << 20).unwrap();
    assert!(text.starts_with("[Event \"Paris\"]\n[Site"), "{text}");
    assert!(text.ends_with("3. d4 Bg4 1-0\n"), "{text}");
    assert!(!text.contains('\r'));

    // Unknown names are none; the result comes from the termination; a
    // Chess960 game has no ECO code and says so in its field.
    let r = db.record(2).unwrap();
    assert_eq!((r.white(), r.tournament()), (-1, -1));
    assert_eq!(db.player(r.black()).unwrap().unwrap().first, "Mikhail");
    assert_eq!(r.result(), GameResult::Unknown(0xff));
    assert_eq!(r.result().pgn(), "*");
    assert!(matches!(r.eco(), Eco::Chess960(_)));
    assert!(r.is_chess960() && r.has_setup() && !r.is_other_variant());
    assert_eq!(r.move_count(), 1);

    // Names are shared; a result tag that is none gives way to the termination.
    let r3 = db.record(3).unwrap();
    assert_eq!(r3.white(), db.record(1).unwrap().white());
    assert_eq!(r3.tournament(), db.record(1).unwrap().tournament());
    assert_eq!(r3.result(), GameResult::BlackWins);
    assert_eq!(r3.played_date().pgn(), "1858.12.20");
    assert_eq!((db.players(), db.tournaments(), db.annotators()), (4, 1, 1));
}

#[test]
fn full_moves_count_the_move_numbers() {
    // Set up with black to move: `35... Kf8 36. Ke4` covers two move numbers.
    let text = "[SetUp \"1\"]\n[FEN \"3r4/4R1kp/8/2p5/2R5/P3PK2/1r3P1P/8 b - - 0 35\"]\n\n35... Kf8 36. Ke4 1-0\n\n\
                [Event \"w\"]\n\n1. e4 e5 2. Nf3 1-0\n";
    let f = pgn_file("full-moves", text.as_bytes());
    let db = built(&f, 1, CodePage::WESTERN);
    assert_eq!(db.record(1).unwrap().move_count(), 2);
    assert_eq!(db.record(2).unwrap().move_count(), 2);
}

#[test]
fn batches_and_bounds() {
    let f = pgn_file("batches", GAMES.as_bytes());
    let db = built(&f, 1, CodePage::WESTERN);
    let ids: Vec<u32> = db.records(0, 99).unwrap().iter().map(|r| r.id()).collect();
    assert_eq!(ids, [1, 2, 3]);
    let mut buf = vec![0u8; RECORD_SIZE * 2];
    assert_eq!(db.read_records(2, &mut buf).unwrap(), 2);
    assert_eq!(db.read_records(4, &mut buf).unwrap(), 0);
    assert_eq!(db.read_records(0, &mut buf).unwrap(), 0);
    assert!(db.record(0).is_err() && db.record(4).is_err());
    assert_eq!(db.player(99).unwrap(), None);
    assert_eq!(db.player(-1).unwrap(), None);
    assert_eq!(db.tournament(1).unwrap(), None);
    // A game longer than the limit is refused before it is read.
    let r = db.record(1).unwrap();
    assert!(db.text(&r, 10).is_err());
}

#[test]
fn text_in_the_code_page() {
    // «Спасский» and a comment in Windows-1251, as older programs wrote them.
    let mut bytes = b"[White \"".to_vec();
    bytes.extend([0xD1, 0xEF, 0xE0, 0xF1, 0xF1, 0xEA, 0xE8, 0xE9]);
    bytes.extend(b"\"]\n\n1. e4 {");
    bytes.extend([0xF5, 0xEE, 0xE4]);
    bytes.extend(b"} *\n");
    let f = pgn_file("cp1251", &bytes);
    let db = built(&f, 1, CodePage::new(1251));
    let r = db.record(1).unwrap();
    assert_eq!(db.player(r.white()).unwrap().unwrap().last, "Спасский");
    assert_eq!(db.text(&r, 1 << 20).unwrap(), "[White \"Спасский\"]\n\n1. e4 {ход} *\n");
    // A game is read in one encoding throughout: a name that happens to be
    // valid UTF-8 in a game that is not reads as the served text shows it.
    let f2 = pgn_file("mixed", b"[White \"\xc3\xa9\"]\n\n1. e4 {\xff} *\n");
    let db2 = built(&f2, 1, CodePage::WESTERN);
    let r2 = db2.record(1).unwrap();
    let name = db2.player(r2.white()).unwrap().unwrap().last;
    assert_eq!(name, "\u{c3}\u{a9}");
    assert!(db2.text(&r2, 1 << 20).unwrap().contains(&format!("[White \"{name}\"]")));
    // The index belongs to the code page it was built with.
    let (pgn, index) = (f.dir().join("db.pgn"), index_of(&f));
    assert!(Database::open(&pgn, &index, 1, CodePage::WESTERN).is_err());
}

#[test]
fn an_index_of_another_state_is_refused() {
    let f = pgn_file("stale", GAMES.as_bytes());
    let (pgn, index) = (f.dir().join("db.pgn"), index_of(&f));
    pgnfile::build(&pgn, &index, 5, CodePage::WESTERN, &mut |_| true).unwrap();
    assert!(Database::open(&pgn, &index, 6, CodePage::WESTERN).is_err(), "another stamp");
    std::fs::write(&pgn, format!("{GAMES}\n[Event \"more\"]\n*\n")).unwrap();
    assert!(Database::open(&pgn, &index, 5, CodePage::WESTERN).is_err(), "another length");
}

#[test]
fn damaged_indexes_are_errors_not_panics() {
    let f = pgn_file("damaged", GAMES.as_bytes());
    let (pgn, index) = (f.dir().join("db.pgn"), index_of(&f));
    pgnfile::build(&pgn, &index, 1, CodePage::WESTERN, &mut |_| true).unwrap();
    let good = std::fs::read(&index).unwrap();
    let open = |bytes: &[u8]| {
        std::fs::write(&index, bytes).unwrap();
        Database::open(&pgn, &index, 1, CodePage::WESTERN)
    };
    assert!(open(&good[..10]).is_err());
    assert!(open(&good[..good.len() - 1]).is_ok(), "a short name text reads as empty");
    for at in [0, 8, 12, 32, 36, 48] {
        let mut bad = good.clone();
        bad[at] ^= 0xff;
        assert!(open(&bad).is_err(), "byte {at}");
    }
    // Name entries that point anywhere read as empty names.
    let mut bad = good.clone();
    let names_at = u64::from_le_bytes(good[48..56].try_into().unwrap()) as usize;
    bad[names_at..names_at + 12].fill(0xff);
    let db = open(&bad).unwrap();
    assert_eq!(db.player(0).unwrap().unwrap().pgn(), "");
    // Record fields read whatever they hold.
    let mut bad = good;
    bad[64..64 + RECORD_SIZE].fill(0xff);
    let db = open(&bad).unwrap();
    let r = db.record(1).unwrap();
    assert!(db.player(r.white()).unwrap().is_none());
    assert!(db.text(&r, 1 << 20).is_err());
}

#[test]
fn a_build_can_be_stopped_and_leaves_nothing() {
    let f = pgn_file("stopped", GAMES.as_bytes());
    let (pgn, index) = (f.dir().join("db.pgn"), index_of(&f));
    assert!(pgnfile::build(&pgn, &index, 1, CodePage::WESTERN, &mut |_| false).is_err());
    assert!(!index.exists());
    assert!(!pgnfile::partial_path(&index).exists());
    assert!(pgnfile::build(Path::new("/no/such/file.pgn"), &index, 1, CodePage::WESTERN, &mut |_| true).is_err());
}

#[test]
fn malformed_text_never_panics() {
    let cases: [&[u8]; 9] = [
        b"",
        b"[",
        b"[Event \"x",
        b"{ never closed",
        b"((((((((e4",
        b")))) e4 1-0",
        b"\xff\xfe\x00[White \"\xff\"]\n1. e4 *",
        b"[Event \"a\"]\n[Event \"b\"]\n[Event \"c\"]",
        b"1-0 0-1 * 1/2-1/2",
    ];
    for (i, bytes) in cases.iter().enumerate() {
        let f = pgn_file(&format!("malformed-{i}"), bytes);
        let db = built(&f, 1, CodePage::WESTERN);
        for id in 1..=db.record_count() {
            let r = db.record(id).unwrap();
            let _ = db.player(r.white()).unwrap();
            let _ = db.text(&r, 1 << 20).unwrap();
        }
    }
    // Garbage of every byte value, repeated.
    let garbage: Vec<u8> = (0..=255u8).cycle().take(1 << 16).collect();
    let f = pgn_file("malformed-garbage", &garbage);
    let db = built(&f, 1, CodePage::WESTERN);
    for id in 1..=db.record_count() {
        let r = db.record(id).unwrap();
        db.text(&r, 1 << 20).unwrap();
    }
}

#[test]
fn many_games_are_read_in_parts() {
    // More than one part of the file, with games across the parts' ends.
    let game = "[Event \"E\"]\n[White \"W\"]\n[Black \"B\"]\n\n1. e4 e5 2. Nf3 Nc6 1/2-1/2\n\n";
    let count = (3 << 20) / game.len() + 7;
    let f = pgn_file("many", game.repeat(count).as_bytes());
    let db = built(&f, 1, CodePage::WESTERN);
    assert_eq!(db.record_count() as usize, count);
    let last = db.record(count as u32).unwrap();
    assert_eq!(last.move_count(), 2);
    assert_eq!(db.text(&last, 1 << 20).unwrap(), game.trim_end().to_string() + "\n");
    assert_eq!((db.players(), db.tournaments()), (2, 1));
}

#[test]
fn comments_open_to_the_end_and_closed_comments() {
    // A comment left open ends before the next game's header, and the games
    // after it are read; the text is read again from there.
    let text = "[Event \"A\"]\n\n1. e4 {never closed\n\n[Event \"B\"]\n[White \"W\"]\n\n1. d4 d5 *\n\n\
                [Event \"C\"]\n\n1. c4 {again\n[Event \"D\"]\n\n1. Nf3 *\n";
    let f = pgn_file("open-comments", text.as_bytes());
    let db = built(&f, 1, CodePage::WESTERN);
    assert_eq!(db.record_count(), 4);
    let b = db.record(2).unwrap();
    assert_eq!(db.player(b.white()).unwrap().unwrap().last, "W");
    assert_eq!(b.move_count(), 1);
    assert!(db.text(&db.record(1).unwrap(), 1 << 20).unwrap().trim_end().ends_with("{never closed"));
    assert_eq!(db.record(4).unwrap().move_count(), 1);

    // A closed comment keeps the header lines it quotes: one game.
    let text =
        "[Event \"Actual\"]\n\n1. e4 {quoted header:\n[Event \"Example\"]\n[Site \"Somewhere\"]\n} e5 2. Nf3 *\n";
    let f = pgn_file("closed-comment", text.as_bytes());
    let db = built(&f, 1, CodePage::WESTERN);
    assert_eq!(db.record_count(), 1);
    assert_eq!(db.record(1).unwrap().move_count(), 2);
}

#[test]
fn an_escape_in_a_tag_counts_in_the_games_encoding() {
    // `\xc3 \ \xa9` is not UTF-8: the game is read in the code page, its
    // names as its served text.
    let f = pgn_file("escape", b"[White \"\xc3\xa9\"]\n[Event \"\xc3\\\xa9\"]\n\n1. e4 *");
    let db = built(&f, 1, CodePage::WESTERN);
    let r = db.record(1).unwrap();
    let name = db.player(r.white()).unwrap().unwrap().last;
    assert_eq!(name, "\u{c3}\u{a9}");
    assert!(db.text(&r, 1 << 20).unwrap().contains(&format!("[White \"{name}\"]")));
}

#[test]
fn names_follow_the_served_text_at_the_edges() {
    // A tag given up, and a tag the file ends in with a comment after its
    // value: the game's names are read as its text is served.
    for (i, bytes) in
        [&b"[White \"\xc3\xa9\"]\n[Event {\xff} broken]\n\n1. e4 *"[..], &b"[White \"\xc3\xa9\" {\xff}"[..]]
            .into_iter()
            .enumerate()
    {
        let f = pgn_file(&format!("edges-{i}"), bytes);
        let db = built(&f, 1, CodePage::WESTERN);
        let r = db.record(1).unwrap();
        let name = db.player(r.white()).unwrap().unwrap().last;
        let text = db.text(&r, 1 << 20).unwrap();
        assert!(text.contains(&format!("[White \"{name}\"")), "{i}: {name} in {text:?}");
    }
}
