//! Engine evaluations and times (asavis/oschess-cb-bridge#42, part 2): the
//! decoders of types 26, 21, 07 and 24, damaged payloads included, and how
//! both PGN forms write them.

use cbformat::fixture::{Builder, TempDb, annotations, quiet};
use cbformat::fixture_cbh::{self, Tok, annotation_record, encode, move_record};
use cbformat::game::timing::{self, Evaluation, Score, Stage};
use cbformat::movetable::{self, Color, Piece};
use cbformat::pgn::{self, Options};
use cbformat::v2::Database;
use chesscore::Board;

/// Type-26 data in 2CBH layout from (value, depth, flag) entries.
fn evals_2cbh(entries: &[(i16, u8, u8)]) -> Vec<u8> {
    let mut d = vec![1];
    d.extend((2 + 4 * entries.len() as i32).to_le_bytes());
    d.extend((entries.len() as u16).to_le_bytes());
    for &(v, depth, flag) in entries {
        d.extend(v.to_le_bytes());
        d.extend([depth, flag]);
    }
    d
}

/// The same entries in the classic layout.
fn evals_classic(entries: &[(i16, u8, u8)]) -> Vec<u8> {
    let mut d = (entries.len() as u16).to_be_bytes().to_vec();
    for &(v, depth, flag) in entries {
        let [lo, hi] = v.to_le_bytes();
        d.extend([flag, depth, hi, lo]);
    }
    d
}

fn engine(value: i16, kind: i16, depth: i16) -> Vec<u8> {
    [value.to_le_bytes(), kind.to_le_bytes(), depth.to_le_bytes()].concat()
}

fn control(stages: &[(i32, i32, u16, u8)]) -> Vec<u8> {
    let mut d = vec![1];
    for k in 0..3 {
        let (initial, increment, moves, kind) = stages.get(k).copied().unwrap_or_default();
        d.extend(initial.to_le_bytes());
        d.extend(increment.to_le_bytes());
        d.extend(moves.to_le_bytes());
        d.push(kind);
    }
    d.extend([0; 4]);
    d
}

fn other(code: u16, data: &[u8]) -> Vec<u8> {
    let mut v = code.to_le_bytes().to_vec();
    v.extend(data);
    v
}

#[test]
fn evaluations_decode_in_both_layouts() {
    let entries = [(20, 18, 0), (-35, 20, 0), (0, 0, 0xff), (3, 1, 1), (-2, 1, 1), (0, 1, 1), (7, 5, 2)];
    let two = timing::evaluations(&evals_2cbh(&entries), false).unwrap();
    assert_eq!(timing::evaluations(&evals_classic(&entries), true).unwrap(), two);
    assert_eq!(two[1], Evaluation { value: -35, depth: 20, flag: 0 });
    let scores: Vec<Option<Score>> = two.iter().map(Evaluation::score).collect();
    // A mate counted in plies becomes moves; mate 0, none and unknown flags give none.
    assert_eq!(
        scores,
        [
            Some(Score::Centipawns(20)),
            Some(Score::Centipawns(-35)),
            None,
            Some(Score::Mate(2)),
            Some(Score::Mate(-1)),
            None,
            None
        ]
    );
    let profile: Vec<i32> = two.iter().map(Evaluation::profile_value).collect();
    assert_eq!(profile, [20, -35, 32767, 29997, -29998, 32767, 32767]);
}

#[test]
fn damaged_evaluations_are_not_decoded() {
    let good = evals_2cbh(&[(20, 18, 0), (5, 18, 0)]);
    let mut bad_len = good.clone();
    bad_len[1] = 99;
    let mut bad_marker = good.clone();
    bad_marker[0] = 2;
    for (what, d) in [
        ("length", bad_len),
        ("marker", bad_marker),
        ("truncated", good[..good.len() - 1].to_vec()),
        ("empty", Vec::new()),
        ("short head", vec![1, 0, 0]),
    ] {
        assert_eq!(timing::evaluations(&d, false), None, "{what}");
    }
    let classic = evals_classic(&[(20, 18, 0)]);
    assert_eq!(timing::evaluations(&classic[..3], true), None);
    assert_eq!(timing::evaluations(&[0, 5, 1, 2, 3, 4], true), None);
}

#[test]
fn engine_evaluations_time_spent_and_time_control() {
    assert_eq!(timing::engine_evaluation(&engine(-120, 0, 21), false), Some(Score::Centipawns(-120)));
    assert_eq!(timing::engine_evaluation(&engine(-3, 1, 0), false), Some(Score::Mate(-3)));
    // Mate 0, the unknown kinds 3 and 32, and a truncated record give none.
    for d in [engine(0, 1, 0), engine(40, 3, 0), engine(40, 32, 0), engine(40, 0, 1)[..5].to_vec()] {
        assert_eq!(timing::engine_evaluation(&d, false), None, "{d:?}");
    }
    assert_eq!(timing::engine_evaluation(&engine(40, 0, 1), true), None, "classic is not decoded");

    assert_eq!(timing::time_spent(&[0, 5, 2, 1], false), Some((1, 2, 5)));
    assert_eq!(timing::time_spent(&[0, 60, 2, 1], false), None, "60 seconds");
    assert_eq!(timing::time_spent(&[0, 5, 60, 1], false), None, "60 minutes");
    assert_eq!(timing::time_spent(&[0, 5, 2], false), None);
    assert_eq!(timing::time_spent(&[0, 5, 2, 1], true), None, "classic is not decoded");

    let d = control(&[(540_000, 3000, 40, 1), (180_000, 3000, 1000, 3)]);
    let stages = timing::time_control(&d, false).unwrap();
    assert_eq!(stages[0], Stage { initial: 540_000, increment: 3000, moves: 40, kind: 1 });
    assert_eq!(stages[2], Stage::default());
    let mut tail = d.clone();
    tail[37] = 1;
    for bad in [tail, d[..37].to_vec(), [&[2u8][..], &d[1..]].concat()] {
        assert_eq!(timing::time_control(&bad, false), None);
    }
    assert_eq!(timing::time_control(&d, true), None, "classic is not decoded");
    // A negative time means nothing in a time control: such a record is not
    // decoded, and its data stays raw. The extremes of an `int` are read.
    for negative in [-1, -25, -99, -100, -125, i32::MIN] {
        assert_eq!(timing::time_control(&control(&[(125, negative, 40, 1)]), false), None, "increment {negative}");
        assert_eq!(timing::time_control(&control(&[(negative, 0, 1000, 0)]), false), None, "initial {negative}");
    }
    let max = timing::time_control(&control(&[(i32::MAX, i32::MAX, u16::MAX, 3)]), false).unwrap();
    assert_eq!(max[0], Stage { initial: i32::MAX, increment: i32::MAX, moves: u16::MAX, kind: 3 });
}

#[test]
fn every_time_the_bridge_writes() {
    // A negative increment: the time control stays raw, never a positive time.
    let content = annotations(&[
        (-1, vec![other(0x24, &control(&[(125, -25, 40, 1)]))]),
        (0, vec![other(0x07, &[0, 59, 59, 255])]),
        (1, vec![other(0x24, &control(&[(i32::MAX, 1, u16::MAX, 3)]))]),
    ]);
    let mut b = Builder::new();
    let moves = three_moves(&mut b);
    let a = b.annotations(&content);
    b.annotated_game(moves, a);
    let db = b.write("timing-extremes");
    let full = render(&db, true);
    assert!(full.starts_with("{[%cbraw type=24;data="), "{full}");
    assert!(!full.contains("0.25"), "{full}");
    assert!(full.contains("1. e4 {[%emt 255:59:59]}"), "{full}");
    assert!(full.contains("[%cbtimecontrol kindA=3;initialA=21474836.47;incrementA=0.01;movesA=65535;data="), "{full}");
}

/// 1. e4 e5 2. Nf3
fn three_moves(b: &mut Builder) -> i64 {
    use Color::{Black as B, White as W};
    b.moves(
        1,
        &[
            movetable::MOVES,
            quiet(W, Piece::Pawn, "e2", "e4"),
            quiet(B, Piece::Pawn, "e7", "e5"),
            quiet(W, Piece::Knight, "g1", "f3"),
            movetable::END_OF_LINE,
        ],
    )
}

fn render(db: &TempDb, full: bool) -> String {
    let db = Database::open(db.base()).unwrap();
    let r = pgn::game_with(&db, 1, &Options { full, ..Options::default() }).unwrap();
    r.pgn.split("\n\n").nth(1).unwrap().trim_end().trim_end_matches("1-0").trim_end().to_string()
}

#[test]
fn both_forms_write_the_evaluations_and_the_full_form_the_moves_scores() {
    // The start, 1. e4, 1... e5 (Black mates in 3 plies), 2. Nf3 (none).
    let entries = [(15, 20, 0), (32, 20, 0), (-3, 1, 1), (0, 0, 0xff)];
    let content = annotations(&[
        (-1, vec![other(0x26, &evals_2cbh(&entries)), other(0x24, &control(&[(720_000, 3000, 1000, 3)]))]),
        (0, vec![other(0x07, &[0, 12, 0, 0])]),
        (2, vec![other(0x21, &engine(-250, 0, 24)), other(0x07, &[0, 5, 1, 0])]),
    ]);
    let mut b = Builder::new();
    let moves = three_moves(&mut b);
    let a = b.annotations(&content);
    b.annotated_game(moves, a);
    let db = b.write("timing-forms");
    assert_eq!(render(&db, false), "{[%evp 0,3,15,32,-29997,32767]} 1. e4 e5 2. Nf3");
    let full = render(&db, true);
    assert!(full.starts_with("{[%evp 0,3,15,32,-29997,32767]} {[%cbraw type=26;data="), "the data is kept: {full}");
    assert!(full.contains("[%cbtimecontrol kindA=3;initialA=7200;incrementA=30;movesA=1000;data="), "{full}");
    // Each main-line move takes its entry; a move's own evaluation (type 21)
    // wins over the main line's; the time spent follows.
    assert!(full.contains("1. e4 {[%eval 0.32] [%emt 0:00:12]} {[%cbraw type=07;data=AAwAAA]}"), "{full}");
    assert!(full.contains("1... e5 {[%eval #-2]}"), "{full}");
    assert!(full.contains("2. Nf3 {[%eval -2.50] [%emt 0:01:05]} {[%cbraw type=21;data="), "{full}");
}

#[test]
fn damaged_payloads_are_kept_raw() {
    // The record's own length holds, so the record decodes, but the count
    // claims more entries than there are.
    let mut bad = evals_2cbh(&[(15, 20, 0), (32, 20, 0)]);
    bad[5] = 3;
    let content = annotations(&[(-1, vec![other(0x26, &bad)]), (0, vec![other(0x07, &[0, 75, 0, 0])])]);
    let mut b = Builder::new();
    let moves = three_moves(&mut b);
    let a = b.annotations(&content);
    b.annotated_game(moves, a);
    let db = b.write("timing-damaged");
    assert_eq!(render(&db, false), "1. e4 e5 2. Nf3");
    let full = render(&db, true);
    assert!(!full.contains("[%evp") && !full.contains("[%eval") && !full.contains("[%emt"), "{full}");
    assert!(full.contains("[%cbraw type=26;data=") && full.contains("[%cbraw type=07;data=AEsAAA]"), "{full}");
}

#[test]
fn a_classic_game_writes_the_same_evaluations() {
    let entries = [(15, 20, 0), (32, 20, 0), (-3, 1, 1)];
    let moves = move_record(
        0,
        None,
        None,
        &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::Mv("e7e5"), Tok::End], 0, false),
    );
    let data = evals_classic(&entries);
    let items: Vec<(i32, u8, &[u8])> = vec![(-1, 0x26, &data)];
    let mut b = fixture_cbh::Builder::new();
    b.game(&moves);
    b.annotations(&annotation_record(1, &items));
    let f = b.write("timing-classic");
    let db = cbformat::cbh::Database::open(f.base()).unwrap();
    let render = |full: bool| {
        let r = pgn::classic_game_with(&db, 1, &Options { full, ..Options::default() }).unwrap();
        r.pgn.split("\n\n").nth(1).unwrap().trim_end().trim_end_matches("1-0").trim_end().to_string()
    };
    assert_eq!(render(false), "{[%evp 0,2,15,32,-29997]} 1. e4 e5");
    let full = render(true);
    assert!(full.contains("1. e4 {[%eval 0.32]} 1... e5 {[%eval #-2]}"), "{full}");
}
