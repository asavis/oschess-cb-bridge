//! One view of both formats: the same game, stored once as 2CBH and once as a
//! classic database, reads the same through `view::Base`.

use cbformat::fixture::{self, TempDb, quiet, text};
use cbformat::fixture_cbh::{self, Tok, annotation_record, encode, move_record};
use cbformat::game::{Date, Eco, GameResult, Head, RecordKind, Start, language};
use cbformat::movetable::{self, Color, Piece};
use cbformat::pgn::{self, Options};
use cbformat::replay::TreeVisitor;
use cbformat::view::{Base, Format, Header, PositionOrder, format_of};
use chesscore::{Board, Move};

use Color::{Black as B, White as W};
use Piece::{Knight, Pawn};

/// `1.e4 c5 (1...c6 2.d4) 2.Nf3` as 2CBH, with a comment on every move in
/// PGN order: e4 0, c5 1, c6 2, d4 3, Nf3 4.
fn two_cbh(name: &str) -> TempDb {
    let words = [
        movetable::MOVES,
        quiet(W, Pawn, "e2", "e4"),
        quiet(B, Pawn, "c7", "c5"),
        movetable::ALTERNATIVE,
        quiet(W, Knight, "g1", "f3"),
        movetable::END_OF_LINE,
        quiet(B, Pawn, "c7", "c6"),
        quiet(W, Pawn, "d2", "d4"),
        movetable::END_OF_LINE,
    ];
    let names = ["e4", "c5", "c6", "d4", "Nf3"];
    let blocks: Vec<_> =
        names.iter().enumerate().map(|(p, n)| (p as i32, vec![text(false, language::ENGLISH, n)])).collect();
    let mut b = fixture::Builder::new();
    let moves = b.moves(1, &words);
    let a = b.annotations(&fixture::annotations(&blocks));
    b.annotated_game(moves, a);
    b.write(&format!("view-2cbh-{name}"))
}

/// The same game as a classic database, its comments in stored order:
/// e4 0, c5 1, Nf3 2, c6 3, d4 4.
fn classic(name: &str) -> TempDb {
    use Tok::{End as E, Mv as M, Var as V};
    let toks = [M("e2e4"), V, M("c7c5"), M("g1f3"), E, M("c7c6"), M("d2d4"), E];
    let mut b = fixture_cbh::Builder::new();
    b.game(&move_record(0, None, None, &encode(&Board::startpos(), &toks, 0, false)));
    let data: Vec<Vec<u8>> = ["e4", "c5", "Nf3", "c6", "d4"]
        .iter()
        .map(|n| {
            let mut d = vec![0, 42];
            d.extend(n.as_bytes());
            d
        })
        .collect();
    let items: Vec<(i32, u8, &[u8])> = data.iter().enumerate().map(|(p, d)| (p as i32, 0x02, d.as_slice())).collect();
    b.annotations(&annotation_record(1, &items));
    b.write(&format!("view-cbh-{name}"))
}

fn movetext(pgn: &str) -> &str {
    pgn.split("\n\n").nth(1).unwrap().trim_end()
}

#[derive(Default)]
struct Sans(Vec<String>);

impl TreeVisitor for Sans {
    fn play(&mut self, _: &Board, mv: Option<Move>, _: bool) {
        self.0.push(mv.map_or("--".into(), |m| m.to_string()));
    }
}

#[test]
fn both_formats_read_the_same() {
    let (f2, f1) = (two_cbh("same"), classic("same"));
    let new = Base::open(f2.base()).unwrap();
    let old = Base::open(f1.base()).unwrap();
    assert_eq!((new.format(), old.format()), (Format::TwoCbh, Format::Cbh));
    assert_eq!((new.position_order(), old.position_order()), (PositionOrder::Pgn, PositionOrder::Stored));
    assert_eq!((new.record_count(), old.record_count()), (1, 1));
    assert!(new.has_annotations() && old.has_annotations());

    let options = Options::default();
    let (a, b) = (new.pgn(1, &options).unwrap(), old.pgn(1, &options).unwrap());
    // The comments land on the same moves although the formats number them
    // differently; only the result after the movetext may differ.
    let want = "1. e4 {e4} 1... c5 {c5} (1... c6 {c6} 2. d4 {d4}) 2. Nf3 {Nf3} ";
    assert!(movetext(&a.pgn).starts_with(want), "{}", a.pgn);
    assert!(movetext(&b.pgn).starts_with(want), "{}", b.pgn);
    assert_eq!(a.annotations, b.annotations);

    for base in [&new, &old] {
        let h = base.header(1).unwrap();
        assert_eq!(h.id(), 1);
        assert_eq!(h.kind(), RecordKind::Game);
        let moves = base.moves_of(&h).unwrap();
        let mut sans = Sans::default();
        let stats = moves.walk(&mut sans).unwrap();
        assert_eq!(sans.0, ["e2e4", "c7c5", "g1f3", "c7c6", "d2d4"], "stored order in both formats");
        assert_eq!(stats.total_plies, 5);
        assert_eq!(moves.start().unwrap(), Start::Standard);
        assert!(!moves.is_chess960().unwrap());
        let a = base.annotations_of(&h).unwrap().unwrap();
        assert_eq!(a.blocks.len(), 5);
        let batch = base.batch(1, 10).unwrap();
        assert_eq!(batch.ids(), 1..=1);
        assert_eq!(batch.pgn(1, &options).unwrap(), base.pgn(1, &options).unwrap());
        assert_eq!(base.headers(1, 5).unwrap().len(), 1);
    }
}

#[test]
fn names_and_header_fields() {
    let f1 = classic("names");
    let old = Base::open(f1.dir().join("db.cbh")).unwrap();
    let h = old.header(1).unwrap();
    let names = old.names(&h).unwrap();
    assert_eq!(names.white.unwrap().last, "Morphy");
    assert_eq!(names.black.unwrap().last, "Anderssen");
    assert_eq!(names.tournament.unwrap().title, "Paris");
    assert_eq!(h.result().pgn(), "1-0");
    assert_eq!((h.round(), h.elo()), ((0, 0), (0, 0)));
    // The direct classic writer gives the same PGN as the view.
    let db = cbformat::cbh::Database::open(f1.base()).unwrap();
    assert_eq!(pgn::classic_game(&db, 1).unwrap(), old.pgn(1, &Options::default()).unwrap().pgn);

    // A header of one format given to a database of the other is an error.
    let f2 = two_cbh("names");
    let new = Base::open(f2.base()).unwrap();
    let other: Header = new.header(1).unwrap();
    assert!(old.names(&other).is_err());
    assert!(old.moves_of(&other).is_err());
    assert!(old.annotations_of(&other).is_err());
}

/// One mapping per format (#66): the same header fields, stored once in a
/// 2CBH record and once in a classic one, read the same through [`Head`].
#[test]
fn every_header_field_reads_the_same_in_both_formats() {
    let date: u32 = (2019 << 9) | (7 << 5) | 21;
    // ECO B42/3, round 7(2), ratings 2710 and 2695, 41 moves, a draw.
    let (eco, round, sub, elo, moves, result) = (43u16 * 128 + 3, 7u8, 2u8, (2710u16, 2695u16), 41u8, 1u8);

    let mut b = fixture::Builder::new();
    let m = b.moves(1, &[movetable::MOVES, quiet(W, Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    let r = b.game(m);
    r[0x58] = result;
    r[0x5a..0x5c].copy_from_slice(&i16::from(round).to_le_bytes());
    r[0x5c..0x5e].copy_from_slice(&i16::from(sub).to_le_bytes());
    r[0x60..0x62].copy_from_slice(&elo.0.to_le_bytes());
    r[0x70..0x72].copy_from_slice(&elo.1.to_le_bytes());
    r[0x80..0x82].copy_from_slice(&eco.to_le_bytes());
    r[0x8a..0x8c].copy_from_slice(&i16::from(moves).to_le_bytes());
    r[0xbc..0xc0].copy_from_slice(&date.to_le_bytes());
    let f2 = b.write("view-2cbh-fields");

    use Tok::{End as E, Mv as M};
    let mut b = fixture_cbh::Builder::new();
    let r = b.game(&move_record(0, None, None, &encode(&Board::startpos(), &[M("e2e4"), E], 0, false)));
    r[0x1b] = result;
    r[0x1d] = round;
    r[0x1e] = sub;
    r[0x1f..0x21].copy_from_slice(&elo.0.to_be_bytes());
    r[0x21..0x23].copy_from_slice(&elo.1.to_be_bytes());
    r[0x23..0x25].copy_from_slice(&eco.to_be_bytes());
    r[0x2d] = moves;
    r[0x18..0x1b].copy_from_slice(&date.to_be_bytes()[1..]);
    let f1 = b.write("view-cbh-fields");

    let fields = |h: Header| {
        (
            h.id(),
            h.kind(),
            h.is_deleted(),
            h.other(),
            h.result(),
            h.eco(),
            h.played_date(),
            h.round(),
            h.elo(),
            h.move_count(),
        )
    };
    let new = Base::open(f2.base()).unwrap();
    let old = Base::open(f1.dir().join("db.cbh")).unwrap();
    let two_cbh = fields(new.header(1).unwrap());
    assert_eq!(two_cbh, fields(old.header(1).unwrap()));
    assert_eq!(
        two_cbh,
        (
            1,
            RecordKind::Game,
            false,
            None,
            GameResult::Draw,
            Eco::Code { code: 42, sub: 3 },
            Date(date as i32),
            (7, 2),
            (2710, 2695),
            41
        )
    );
}

#[test]
fn formats_by_extension_and_by_stem() {
    let (f2, f1) = (two_cbh("formats"), classic("formats"));
    assert_eq!(format_of(&f2.base()), Format::TwoCbh);
    assert_eq!(format_of(&f1.base()), Format::Cbh);
    assert_eq!(format_of(&f1.dir().join("db.cbh")), Format::Cbh);
    assert_eq!(format_of(&f1.dir().join("db.2cbh")), Format::TwoCbh);
    // A stem with neither file is read as 2CBH, whose error names what is missing.
    assert_eq!(format_of(&f1.dir().join("none")), Format::TwoCbh);
    assert!(Base::open(f1.dir().join("none")).is_err());
}
