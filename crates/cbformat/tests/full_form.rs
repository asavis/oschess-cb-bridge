//! The full PGN form (asavis/oschess-cb-bridge#42): every text in every
//! language as its own comment led by `[%lang]`, every other annotation as a
//! command that keeps its data, and game quotations and medals in the reading
//! form as ChessBase's own export writes them.

use cbformat::fixture::{Builder, TempDb, annotations, arrows, quiet, squares, symbols, text};
use cbformat::fixture_cbh::{self, Tok, annotation_record, encode, move_record};
use cbformat::movetable::{self, Color, Piece};
use cbformat::pgn::{self, AnnotationStatus, Options};
use cbformat::v2::{Database, Date, Quotation, language};
use chesscore::Board;

use Color::{Black as B, White as W};
use Piece::Pawn;

const MOVES: u16 = movetable::MOVES;
const END: u16 = movetable::END_OF_LINE;

/// 1. e4 e5
fn two_moves() -> [u16; 4] {
    [MOVES, quiet(W, Pawn, "e2", "e4"), quiet(B, Pawn, "e7", "e5"), END]
}

fn one_game(name: &str, words: &[u16], content: &[u8]) -> TempDb {
    let mut b = Builder::new();
    let moves = b.moves(1, words);
    let a = b.annotations(content);
    b.annotated_game(moves, a);
    b.write(name)
}

/// The movetext and annotation status of game 1, in the full form or not.
fn render(db: &TempDb, full: bool) -> (String, AnnotationStatus) {
    let db = Database::open(db.base()).unwrap();
    let r = pgn::game_with(&db, 1, &Options { full, ..Options::default() }).unwrap();
    let movetext = r.pgn.split("\n\n").nth(1).unwrap().trim_end().trim_end_matches("1-0").trim_end().to_string();
    (movetext, r.annotations)
}

/// An annotation of type `code` with `data` after its type.
fn other(code: u16, data: &[u8]) -> Vec<u8> {
    let mut v = code.to_le_bytes().to_vec();
    v.extend(data);
    v
}

fn int(n: usize) -> [u8; 4] {
    (n as i32).to_le_bytes()
}

/// The fields a synthetic quotation holds.
struct Spec<'a> {
    white: (&'a str, &'a str, u16),
    black: (&'a str, &'a str, u16),
    event: &'a str,
    site: &'a str,
    date: (i32, i32, i32),
    kind: u8,
    round: u8,
    subround: u8,
    result: u8,
    /// Origin and destination byte of each move.
    moves: &'a [[u8; 2]],
    set_up: bool,
}

impl Default for Spec<'_> {
    fn default() -> Self {
        Spec {
            white: ("Morphy", "Paul", 2690),
            black: ("Anderssen", "Adolf", 2600),
            event: "Casual game",
            site: "Paris",
            date: (1858, 12, 20),
            kind: 4,
            round: 3,
            subround: 0,
            result: 2,
            moves: &[],
            set_up: false,
        }
    }
}

fn date(s: &Spec<'_>) -> i32 {
    s.date.0 << 9 | s.date.1 << 5 | s.date.2
}

/// A 2CBH quotation's data, in the layout of `docs/format-notes.md`.
fn quote_2cbh(s: &Spec<'_>) -> Vec<u8> {
    let mut d = vec![1];
    d.extend(if s.moves.is_empty() { 1u16 } else { 2 }.to_le_bytes());
    d.extend([0, 0]);
    d.extend(int(1));
    d.push(0);
    for t in [s.white.0, s.white.1, s.black.0, s.black.1, s.site, s.event] {
        d.push(t.len() as u8 + 1);
        d.extend(t.as_bytes());
        d.push(0);
    }
    let mut fixed = [0u8; 79];
    fixed[0..4].copy_from_slice(&date(s).to_le_bytes());
    fixed[4] = s.kind;
    fixed[28..30].copy_from_slice(&s.white.2.to_le_bytes());
    fixed[30..32].copy_from_slice(&s.black.2.to_le_bytes());
    fixed[32..34].copy_from_slice(&((20 + 1) * 128u16).to_le_bytes()); // A20
    fixed[34] = s.result;
    fixed[43] = s.round;
    fixed[44] = s.subround;
    d.extend(fixed);
    for list in ["FIDE", ""] {
        d.extend([1, 0, 1, 0, 0]);
        d.extend(int(list.len()));
        d.extend(list.as_bytes());
    }
    d.extend([0; 26]);
    if s.set_up {
        d.push(0);
        d.extend([0; 75]);
    } else {
        d.push(1);
    }
    d.extend([0, 0]);
    d.extend(int(s.moves.len()));
    for m in s.moves {
        d.extend([m[0], m[1], 0, 0, 0]);
    }
    d.extend([0; 4]);
    d
}

/// A classic quotation's data: header only.
fn quote_classic(s: &Spec<'_>) -> Vec<u8> {
    let mut d = vec![0, 0, 0, 1, 0, 0];
    let string = |d: &mut Vec<u8>, t: &str| {
        d.push(t.len() as u8);
        d.extend(t.as_bytes());
        d.push(0);
    };
    string(&mut d, &format!("{},{}", s.white.0, s.white.1));
    string(&mut d, &format!("{},{}", s.black.0, s.black.1));
    d.extend(s.white.2.to_be_bytes());
    d.extend(s.black.2.to_be_bytes());
    d.extend(((20 + 1) * 128u16).to_be_bytes());
    string(&mut d, s.event);
    string(&mut d, s.site);
    d.extend(date(s).to_be_bytes());
    d.extend(u16::from(s.kind).to_be_bytes());
    d.extend([0, 0, 0, 0, 0, 9]); // nation, unknown, rounds
    d.extend([s.subround, s.round, s.result, 0, 0, 181, 175, 0, 43]);
    let len = d.len() as u16;
    d[0..2].copy_from_slice(&len.to_be_bytes());
    d
}

/// A square byte, numbered file by file from 0.
fn sq(name: &str) -> u8 {
    let b = name.as_bytes();
    (b[0] - b'a') * 8 + (b[1] - b'1')
}

#[test]
fn every_language_is_a_comment_of_its_own() {
    let content = annotations(&[
        (-1, vec![text(false, language::ANY, "A classic")]),
        (0, vec![text(true, language::ENGLISH, "before"), symbols(1, 0, 0), text(false, language::ENGLISH, "centre")]),
        (0, vec![text(false, language::GERMAN, "Zentrum"), squares(&[(2, "d5")]), arrows(&[(4, "g1", "f3")])]),
        (1, vec![text(false, 9, "other number")]),
    ]);
    let db = one_game("full-languages", &two_moves(), &content);
    let (full, status) = render(&db, true);
    assert_eq!(
        full,
        "{[%lang any] A classic} {[%lang en] before} {[%cbtext lang=en;before=1;value=before]} 1. e4 $1 \
         {[%csl Gd5][%cal Rg1f3]} {[%lang en] centre} {[%lang de] Zentrum} \
         {[%cbsymbols data=AQAA] [%cbsquares data=Ah0] [%cbarrows data=BDEr]} 1... e5 {[%lang cb-l9] other number}"
    );
    assert_eq!(status, AnnotationStatus::Complete);
    let (reading, _) = render(&db, false);
    assert_eq!(reading, "{A classic} {before} 1. e4 $1 {[%csl Gd5][%cal Rg1f3] centre} 1... e5");
}

#[test]
fn every_other_type_is_a_command_that_keeps_its_data() {
    let training = [&[1u8, 1, 1, 0, 0, 0][..], &int(30), &5u16.to_le_bytes(), &[0; 8], &[0]].concat();
    let (url, caption, video) = ("https://example.org/a;b", "Caption]}", "Video é");
    let link = [&[1u8][..], &int(url.len()), url.as_bytes(), &int(caption.len()), caption.as_bytes()].concat();
    let video = [&[1u8, 0, 0, 0][..], &int(video.len()), video.as_bytes()].concat();
    let evaluations = [&[1u8][..], &int(6), &1u16.to_le_bytes(), &[20, 0, 12, 0]].concat();
    let content = annotations(&[(
        1,
        vec![
            other(0x22, &[4, 0, 0, 0]),
            other(0x18, &[2]),
            other(0x14, &[5]),
            other(0x15, &[&int(2)[..], &[7, 8]].concat()),
            other(0x23, &[1, 2, 3, 4]),
            other(0x1c, &link),
            other(0x20, &video),
            other(0x09, &training),
            other(0x26, &evaluations),
            other(0x16, &[0x10, 0x27, 0, 0]),
        ],
    )]);
    let db = one_game("full-types", &two_moves(), &content);
    let (full, status) = render(&db, true);
    assert_eq!(status, AnnotationStatus::Complete);
    let want = [
        "[%mdl 4]",
        "[%cbcritical phase=middlegame;value=2;data=Ag]",
        "[%cbpawns value=5;data=BQ]",
        "[%cbpath data=AgAAAAcI]",
        "[%cbcolour data=AQIDBA]",
        "[%cblink url=https%3A%2F%2Fexample.org%2Fa%3Bb;caption=Caption%5D%7D;data=",
        "[%cbvideo language=0;caption=Video%20%C3%A9;data=",
        "[%cbtraining variant=1;seconds=30;points=5;data=",
        "[%cbraw type=26;data=",
        "[%cbraw type=16;data=ECcAAA]",
    ];
    for w in want {
        assert!(full.contains(w), "{w} in {full}");
    }
    assert!(full.starts_with("1. e4 e5 {[%mdl 4] [%cbcritical"), "{full}");
    // The reading form writes the medals as ChessBase does, and none of the rest.
    let (reading, _) = render(&db, false);
    assert_eq!(reading, "1. e4 e5 {[%mdl 4]}");
}

#[test]
fn a_game_quotation_as_command_and_as_chessbase_writes_it() {
    // 1. e4 d5 2. exd5 c6 3. dxc6 Nf6 4. cxb7 Bd7 5. bxa8=N: a capture that
    // promotes to a knight, flagged in the origin and named in the destination.
    let moves = [
        [sq("e2"), sq("e4")],
        [sq("d7"), sq("d5")],
        [sq("e4"), sq("d5")],
        [sq("c7"), sq("c6")],
        [sq("d5"), sq("c6")],
        [sq("g8"), sq("f6")],
        [sq("c6"), sq("b7")],
        [sq("c8"), sq("d7")],
        [sq("b7") | 0x40, sq("a8") | 1 << 6],
    ];
    let spec = Spec { moves: &moves, event: "Café; [a=b] 100%", ..Spec::default() };
    let content = annotations(&[(0, vec![text(false, language::ENGLISH, "see"), other(0x13, &quote_2cbh(&spec))])]);
    let db = one_game("full-quote", &two_moves(), &content);
    let (reading, _) = render(&db, false);
    assert_eq!(reading, "1. e4 {see 1-0 Morphy,P (2690)-Anderssen,A (2600) Café; [a=b] 100% Paris 1858 (3)} 1... e5");
    let (full, _) = render(&db, true);
    let quote = full.split("[%cbquote ").nth(1).unwrap().split(']').next().unwrap();
    let fields: Vec<&str> = quote.split(';').collect();
    assert_eq!(
        fields[..12],
        [
            "result=1-0",
            "white=Morphy%2C%20Paul",
            "whiteElo=2690",
            "black=Anderssen%2C%20Adolf",
            "blackElo=2600",
            "event=Caf%C3%A9%3B%20%5Ba%3Db%5D%20100%25",
            "site=Paris",
            "date=1858.12.20",
            "round=3",
            "eco=A20",
            "moves=1.%20e4%20d5%202.%20exd5%20c6%203.%20dxc6%20Nf6%204.%20cxb7%20Bd7%205.%20bxa8%3DN",
            fields[11],
        ]
    );
    assert!(fields[11].starts_with("data=AQIA"), "{}", fields[11]);
    assert!(full.starts_with("1. e4 {[%lang en] see} {[%cbquote result=1-0;"), "{full}");
}

#[test]
fn quotations_keep_what_is_not_decoded() {
    // Castling is stored as the king's move: 1. e4 e5 2. Nf3 Nc6 3. Bc4 Bc5 4. O-O.
    let castles = [
        [sq("e2"), sq("e4")],
        [sq("e7"), sq("e5")],
        [sq("g1"), sq("f3")],
        [sq("b8"), sq("c6")],
        [sq("f1"), sq("c4")],
        [sq("f8"), sq("c5")],
        [sq("e1"), sq("g1")],
    ];
    let q = Quotation::parse_2cbh(&quote_2cbh(&Spec { moves: &castles, ..Spec::default() })).unwrap();
    assert_eq!(q.moves.len(), 7);
    let db = one_game(
        "full-castles",
        &two_moves(),
        &annotations(&[(0, vec![other(0x13, &quote_2cbh(&Spec { moves: &castles, ..Spec::default() }))])]),
    );
    assert!(render(&db, true).0.contains("4.%20O-O;"));
    // A set-up start: the position is not understood, so the moves stay in the data.
    let set_up = quote_2cbh(&Spec { moves: &castles, set_up: true, ..Spec::default() });
    let q = Quotation::parse_2cbh(&set_up).unwrap();
    assert!(q.set_up && q.moves.is_empty());
    let db = one_game("full-set-up", &two_moves(), &annotations(&[(0, vec![other(0x13, &set_up)])]));
    let (full, _) = render(&db, true);
    assert!(full.contains("[%cbquote result=1-0;") && !full.contains("moves="), "{full}");
    // Moves that do not replay are left out too.
    let illegal = quote_2cbh(&Spec { moves: &[[sq("e2"), sq("e5")]], ..Spec::default() });
    let db = one_game("full-illegal", &two_moves(), &annotations(&[(0, vec![other(0x13, &illegal)])]));
    assert!(!render(&db, true).0.contains("moves="));
    // A layout not understood is kept raw.
    let db = one_game(
        "full-bad-quote",
        &two_moves(),
        &annotations(&[(0, vec![other(0x13, &[1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 3, b'a', b'b'])])]),
    );
    assert!(Database::open(db.base()).is_ok());
}

#[test]
fn the_chessbase_text_follows_chessbase() {
    let text = |s: Spec<'_>| Quotation::parse_2cbh(&quote_2cbh(&s)).unwrap().chessbase_text();
    // The site is left out when the title holds it, and so is the year.
    assert_eq!(
        text(Spec { event: "Paris 1858", ..Spec::default() }),
        "1-0 Morphy,P (2690)-Anderssen,A (2600) Paris 1858 (3)"
    );
    // Speed labels, unless the title names them.
    assert_eq!(
        text(Spec { kind: 0x24, result: 1, round: 0, ..Spec::default() }),
        "1/2 Morphy,P (2690)-Anderssen,A (2600) Casual game Paris blitz 1858"
    );
    assert_eq!(
        text(Spec { kind: 0x44, event: "Rapid Open", result: 0, ..Spec::default() }),
        "0-1 Morphy,P (2690)-Anderssen,A (2600) Rapid Open Paris 1858 (3)"
    );
    // A correspondence board in brackets; a subround stored negative shows as
    // its 16-bit two's complement; no rating, no first name.
    assert_eq!(
        text(Spec { kind: 0x83, round: 0, subround: 4, ..Spec::default() }),
        "1-0 Morphy,P (2690)-Anderssen,A (2600) Casual game Paris 1858 [4]"
    );
    assert_eq!(
        text(Spec { subround: 245, white: ("Morphy", "", 0), ..Spec::default() }),
        "1-0 Morphy-Anderssen,A (2600) Casual game Paris 1858 (3.65525)"
    );
    let q = Quotation::parse_2cbh(&quote_2cbh(&Spec::default())).unwrap();
    assert_eq!((q.date, q.white.elo, q.black.last.as_str()), (Date(1858 << 9 | 12 << 5 | 20), 2690, "Anderssen"));
}

#[test]
fn an_unknown_layout_keeps_the_rest_of_the_record() {
    let content = annotations(&[(0, vec![text(false, language::ENGLISH, "ok"), vec![0x1a, 0, 9, 8, 7]])]);
    let db = one_game("full-rest", &two_moves(), &content);
    let (full, status) = render(&db, true);
    assert_eq!(status, AnnotationStatus::Incomplete { type_code: 0x1a });
    // The rest: what follows the type code, the end marker included.
    assert_eq!(full, "{[%cbrest type=1a;data=CQgH____fw]} 1. e4 {[%lang en] ok} 1... e5");
}

#[test]
fn annotations_past_the_end_follow_the_last_move_in_the_full_form() {
    let content = annotations(&[
        (1, vec![text(false, language::ENGLISH, "last")]),
        (2, vec![text(true, language::GERMAN, "danach"), other(0x22, &[8, 0, 0, 0])]),
    ]);
    let db = one_game("full-past-end", &two_moves(), &content);
    assert_eq!(
        render(&db, true).0,
        "1. e4 e5 {[%lang en] last} {[%lang de] danach} {[%cbtext lang=de;before=1;value=danach]} {[%mdl 8]}"
    );
    assert_eq!(render(&db, false).0, "1. e4 e5 {[%mdl 8] last}");
}

#[test]
fn a_classic_game_keeps_its_languages_quotations_and_data() {
    let moves = move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let quote = quote_classic(&Spec { round: 7, ..Spec::default() });
    let items: Vec<(i32, u8, &[u8])> = vec![
        (0, 0x02, b"\x00\x2aEnglish"),
        (0, 0x02, b"\x00\x35Deutsch"),
        (0, 0x02, b"\x00\x91Other nation"),
        (0, 0x13, &quote),
        (0, 0x22, &[0, 0, 0, 4]),
        (0, 0x18, &[2]),
    ];
    let mut b = fixture_cbh::Builder::new();
    b.game(&moves);
    b.annotations(&annotation_record(1, &items));
    let f = b.write("full-classic");
    let db = cbformat::cbh::Database::open(f.base()).unwrap();
    let render = |full: bool| {
        let r = pgn::classic_game_with(&db, 1, &Options { full, ..Options::default() }).unwrap();
        r.pgn.split("\n\n").nth(1).unwrap().trim_end().trim_end_matches("1-0").trim_end().to_string()
    };
    assert_eq!(
        render(false),
        "1. e4 {[%mdl 4] English 1-0 Morphy,P (2690)-Anderssen,A (2600) Casual game Paris 1858 (7)}"
    );
    let full = render(true);
    assert!(
        full.starts_with("1. e4 {[%lang en] English} {[%lang de] Deutsch} {[%lang cb-145] Other nation} {[%cbquote "),
        "{full}"
    );
    assert!(full.contains("date=1858.12.20;round=7;eco=A20;data=") && !full.contains("moves="), "{full}");
    // Classic layouts other than the quotation's are kept as their data.
    assert!(full.ends_with("[%mdl 4] [%cbraw type=18;data=Ag]}"), "{full}");
}

/// Percent-decoding, as a reader of the full form does it.
fn unpercent(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// The texts a reader recovers from a full form, in order: a `[%cbtext]`
/// replaces the visible comment right before it, or stands alone.
fn recovered_texts(full: &str) -> Vec<(String, bool, String)> {
    let mut out: Vec<(String, bool, String)> = Vec::new();
    let mut last_visible = false;
    for body in full.split('{').skip(1).map(|c| c.split('}').next().unwrap()) {
        if let Some(rest) = body.strip_prefix("[%lang ") {
            let (code, text) = rest.split_once("] ").unwrap();
            out.push((code.to_string(), false, text.to_string()));
            last_visible = true;
            continue;
        }
        if let Some(rest) = body.strip_prefix("[%cbtext ") {
            let fields: Vec<(&str, &str)> =
                rest.trim_end_matches(']').split(';').map(|f| f.split_once('=').unwrap()).collect();
            let get = |k: &str| fields.iter().find(|f| f.0 == k).map(|f| unpercent(f.1));
            let text = (get("lang").unwrap(), get("before").as_deref() == Some("1"), get("value").unwrap());
            match out.last_mut().filter(|_| last_visible && get("alone").is_none()) {
                Some(visible) => *visible = text,
                None => out.push(text),
            }
        }
        last_visible = false;
    }
    out
}

#[test]
fn a_text_is_recovered_byte_for_byte() {
    let originals = ["A{B}\nC", "A(B) C", "", "  \t ", "Київ — \u{e024} ok", "  padded  ", "see [%csl Ge4]", "plain"];
    let mut blocks: Vec<(i32, Vec<Vec<u8>>)> =
        originals.iter().map(|t| (0, vec![text(false, language::ENGLISH, t)])).collect();
    blocks.push((1, vec![text(true, language::GERMAN, "vor e5"), text(false, language::GERMAN, "")]));
    let db = one_game("full-text-values", &two_moves(), &annotations(&blocks));
    let (full, _) = render(&db, true);
    let mut want: Vec<(String, bool, String)> =
        originals.iter().map(|t| ("en".to_string(), false, t.to_string())).collect();
    want.push(("de".into(), true, "vor e5".into()));
    want.push(("de".into(), false, String::new()));
    assert_eq!(recovered_texts(&full), want, "{full}");
    // Two texts that clean to the same comment stay apart.
    assert!(
        full.contains("{[%lang en] A(B) C} {[%cbtext lang=en;value=A%7BB%7D%0AC]} {[%lang en] A(B) C} {[%cbtext lang=en;alone=1;value=]}"),
        "{full}"
    );
    // A text that needs nothing has no command.
    assert!(full.contains("{[%lang en] plain} 1... e5") || full.contains("{[%lang en] plain} {[%lang"), "{full}");
    // The reading form is unchanged.
    let (reading, _) = render(&db, false);
    assert!(reading.starts_with("1. e4 {A(B) C A(B) C Київ"), "{reading}");
}

#[test]
fn every_colour_and_game_symbols_are_kept() {
    let content = annotations(&[
        (-1, vec![symbols(1, 10, 140), squares(&[(7, "a1")])]),
        (0, vec![squares(&[(2, "e4"), (7, "d5")]), arrows(&[(8, "b1", "c3")]), symbols(0, 14, 0)]),
    ]);
    let db = one_game("full-colours", &two_moves(), &content);
    let (full, _) = render(&db, true);
    // The game comment: no NAG can stand before the first move, and colour 7
    // has no [%csl] letter; both are commands.
    assert!(full.starts_with("{[%cbsymbols data=AQqM] [%cbsquares data=BwE]} 1. e4 $14"), "{full}");
    // The move: green e4 shown, every colour kept, the position symbol in its slot.
    // e4 is 36 (5th file, 4th rank: 4 · 8 + 3 + 1), d5 29, b1 9, c3 19.
    assert!(
        full.contains("{[%csl Ge4]} {[%cbsquares data=AiQHHQ] [%cbarrows data=CAkT] [%cbsymbols data=AA4A]}"),
        "{full}"
    );
    let (reading, _) = render(&db, false);
    assert_eq!(reading, "1. e4 $14 {[%csl Ge4]} 1... e5");
}

/// Every `[%name body]` command in a full form.
fn commands_in(full: &str) -> Vec<(String, String)> {
    full.split("[%")
        .skip(1)
        .map(|c| {
            let c = c.split(']').next().unwrap();
            let (name, body) = c.split_once(' ').unwrap_or((c, ""));
            (name.to_string(), body.to_string())
        })
        .collect()
}

/// Whether a value keeps to the escaping rule: unreserved characters, and
/// every other byte as `%` and two upper-case hex digits.
fn escaped(v: &str) -> bool {
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = |x: Option<&u8>| x.is_some_and(|c| c.is_ascii_digit() || (b'A'..=b'F').contains(c));
                if !(hex(b.get(i + 1)) && hex(b.get(i + 2))) {
                    return false;
                }
                i += 3;
            }
            c if c.is_ascii_alphanumeric() || b"-._~".contains(&c) => i += 1,
            _ => return false,
        }
    }
    true
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// Whether a command keeps to its grammar: `cb` commands `key=value;…` with
/// keys of letters only and escaped values, the others their own forms.
fn conforms(name: &str, body: &str) -> bool {
    let signed = |s: &str| digits(s.strip_prefix('-').unwrap_or(s));
    match name {
        n if n.starts_with("cb") => body.split(';').all(|f| match f.split_once('=') {
            Some((k, v)) => !k.is_empty() && k.bytes().all(|c| c.is_ascii_alphabetic()) && escaped(v),
            None => false,
        }),
        "lang" => {
            body == "any"
                || (body.len() == 2 && body.bytes().all(|c| c.is_ascii_lowercase()))
                || body.strip_prefix("cb-").is_some_and(|n| digits(n.strip_prefix('l').unwrap_or(n)))
        }
        "mdl" => digits(body),
        "evp" => body.split(',').all(signed) && body.split(',').count() >= 3,
        "eval" => match body.strip_prefix('#') {
            Some(m) => signed(m),
            None => body.split_once('.').is_some_and(|(i, f)| signed(i) && f.len() == 2 && digits(f)),
        },
        "emt" => {
            let p: Vec<&str> = body.split(':').collect();
            p.len() == 3 && digits(p[0]) && p[1..].iter().all(|x| x.len() == 2 && digits(x))
        }
        "csl" | "cal" => body.split(',').all(|m| m.len() >= 3 && b"GYR".contains(&m.as_bytes()[0])),
        _ => false,
    }
}

#[test]
fn every_command_keeps_to_its_grammar() {
    let evaluations = [&[1u8][..], &int(10), &2u16.to_le_bytes(), &[20, 0, 18, 0], &[253, 255, 1, 1]].concat();
    let mut stages = vec![1u8];
    for (initial, increment, moves, kind) in [(540_000i32, 3000i32, 40u16, 1u8), (180_025, 3000, 1000, 3), (0, 0, 0, 0)]
    {
        stages.extend([&initial.to_le_bytes()[..], &increment.to_le_bytes(), &moves.to_le_bytes(), &[kind]].concat());
    }
    stages.extend([0; 4]);
    let (url, caption) = ("https://example.org/a;b=c", "Caption ]} ; = % é");
    let link = [&[1u8][..], &int(url.len()), url.as_bytes(), &int(caption.len()), caption.as_bytes()].concat();
    let caption_v = "Vidé]";
    let video = [&[1u8, 0, 0, 0][..], &int(caption_v.len()), caption_v.as_bytes()].concat();
    let training = [&[1u8, 1, 1, 0, 0, 0][..], &int(30), &5u16.to_le_bytes(), &[0; 8], &[0]].concat();
    let quote = quote_2cbh(&Spec {
        moves: &[[sq("e2"), sq("e4")], [sq("e7"), sq("e5")]],
        event: "Café; [a=b] 100%",
        ..Spec::default()
    });
    let content = annotations(&[
        (
            -1,
            vec![
                text(false, language::ANY, "A classic"),
                symbols(1, 10, 140),
                squares(&[(7, "a1")]),
                other(0x26, &evaluations),
                other(0x24, &stages),
                other(0x16, &[0x10, 0x27, 0, 0]),
            ],
        ),
        (
            0,
            vec![
                text(true, language::GERMAN, "vor"),
                text(false, 9, "A{B}\nC"),
                symbols(1, 14, 0),
                squares(&[(2, "e4")]),
                arrows(&[(8, "b1", "c3"), (4, "g1", "f3")]),
                other(0x13, &quote),
                other(0x22, &[4, 0, 0, 0]),
                other(0x18, &[2]),
                other(0x14, &[5]),
                other(0x15, &[&int(2)[..], &[7, 8]].concat()),
                other(0x23, &[1, 2, 3, 4]),
                other(0x1c, &link),
                other(0x20, &video),
                other(0x09, &training),
                other(0x07, &[0, 12, 1, 0]),
                other(0x21, &[0x06, 0xff, 0, 0, 0x18, 0]),
            ],
        ),
    ]);
    let all = one_game("grammar-all", &two_moves(), &content);
    let (full_all, status) = render(&all, true);
    assert_eq!(status, AnnotationStatus::Complete, "every synthetic annotation decodes: {full_all}");
    let rest = one_game("grammar-rest", &two_moves(), &annotations(&[(0, vec![vec![0x1a, 0, 9, 8, 7]])]));
    let moves = move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let quote_classic = quote_classic(&Spec::default());
    let items: Vec<(i32, u8, &[u8])> = vec![(0, 0x13, &quote_classic), (0, 0x18, &[2]), (0, 0x02, b"\x00\x91Nation")];
    let mut b = fixture_cbh::Builder::new();
    b.game(&moves);
    b.annotations(&annotation_record(1, &items));
    let f = b.write("grammar-classic");
    let classic = cbformat::cbh::Database::open(f.base()).unwrap();
    let classic_full = pgn::classic_game_with(&classic, 1, &Options { full: true, ..Options::default() }).unwrap().pgn;

    let outputs = [full_all, render(&rest, true).0, classic_full];
    let mut seen = std::collections::BTreeSet::new();
    for full in &outputs {
        for (name, body) in commands_in(full) {
            assert!(conforms(&name, &body), "[%{name} {body}] in {full}");
            seen.insert(name);
        }
    }
    let every = [
        "cbarrows",
        "cbcolour",
        "cbcritical",
        "cblink",
        "cbpath",
        "cbpawns",
        "cbquote",
        "cbraw",
        "cbrest",
        "cbsquares",
        "cbsymbols",
        "cbtext",
        "cbtimecontrol",
        "cbtraining",
        "cbvideo",
        "cal",
        "csl",
        "emt",
        "eval",
        "evp",
        "lang",
        "mdl",
    ];
    let missing: Vec<&&str> = every.iter().filter(|n| !seen.contains(**n)).collect();
    assert!(missing.is_empty(), "not rendered: {missing:?}");
    // The grammar check itself rejects what it must.
    for (name, body) in [
        ("cbtimecontrol", "kind1=3"),
        ("cbtext", "lang=en;value=a b"),
        ("cbraw", "type=24;data=a;b"),
        ("cbraw", "type"),
        ("eval", "0.2"),
        ("emt", "0:0:05"),
        ("evp", "0,1"),
        ("other", "x"),
    ] {
        assert!(!conforms(name, body), "[%{name} {body}]");
    }
}
