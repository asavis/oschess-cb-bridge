//! One view of both formats: the same game, stored once as 2CBH and once as a
//! classic database, reads the same through `view::Base`.

use std::path::Path;

use cbformat::codepage::CodePage;
use cbformat::fixture::{self, TempDb, quiet, text};
use cbformat::fixture_cbh::{self, Tok, annotation_record, encode, move_record};
use cbformat::game::{
    Annotation, Block, Date, Eco, GameAnnotations, GameResult, Head, PositionOrder, RecordKind, Source, Start, language,
};
use cbformat::movetable::{self, Color, Piece};
use cbformat::pgn::{self, Options};
use cbformat::replay::TreeVisitor;
use cbformat::view::{Base, Format, Header, format_of};
use cbformat::{Limits, cbh, pgnfile, v2};
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

    let (options, limits) = (Options::default(), Limits::default());
    let (a, b) = (new.pgn(1, &options, limits).unwrap(), old.pgn(1, &options, limits).unwrap());
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
        assert_eq!(a.source.position_order(), base.position_order(), "each reader records its format");
        let batch = base.batch(1, 10).unwrap();
        assert_eq!(batch.ids(), 1..=1);
        assert_eq!(batch.pgn(1, &options, limits).unwrap(), base.pgn(1, &options, limits).unwrap());
        assert_eq!(base.headers(1, 5).unwrap().len(), 1);
    }
}

/// The writer places annotations and reads their data as their own
/// [`Source`] says, not as the database or the entry point they come
/// through: the same medal on position 2, over the same 2CBH moves, is on
/// `Nf3` and big-endian when classic, and on `c6` and little-endian when 2CBH.
#[test]
fn annotations_are_written_as_their_source_says() {
    let f = two_cbh("source");
    let db = v2::Database::open(f.base()).unwrap();
    let data = db.moves_of(&db.record(1).unwrap()).unwrap();
    let moves = data.moves().unwrap();
    let medal = |source| GameAnnotations {
        blocks: vec![Block {
            position: 2,
            annotations: vec![Annotation::Other { code: 0x22, data: vec![0, 0, 0, 4] }],
        }],
        source,
        ..GameAnnotations::default()
    };
    let text = |source| pgn::movetext_annotated(&moves, &medal(source), &Options::default()).unwrap();
    assert_eq!(text(Source::Classic), "1. e4 c5 (1... c6 2. d4) 2. Nf3 {[%mdl 4]}");
    assert_eq!(text(Source::TwoCbh), "1. e4 c5 (1... c6 {[%mdl 67108864]} 2. d4) 2. Nf3");
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
    assert_eq!(pgn::classic_game(&db, 1).unwrap(), old.pgn(1, &Options::default(), Limits::default()).unwrap().pgn);

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

/// Each format's files, the main one first, with whether its reader cannot
/// open the database without it; a path is taken with or without its main
/// extension, as `Base::open` takes it.
#[test]
fn the_files_of_each_format() {
    let names = |format: Format, path: &str| -> Vec<(String, bool)> {
        format
            .files(Path::new(path))
            .into_iter()
            .map(|(p, required)| {
                assert_eq!(p.parent(), Some(Path::new("/d")), "{}", p.display());
                (p.file_name().unwrap().to_string_lossy().into_owned(), required)
            })
            .collect()
    };
    let with = |stem: &str, files: &[(&str, bool)]| -> Vec<(String, bool)> {
        files.iter().map(|&(ext, required)| (format!("{stem}{ext}"), required)).collect()
    };
    let two_cbh =
        [(".2cbh", true), (".2cbg", true), (".2cba", false), (".2lid", true), (".2lgd", false), (".2lcd", false)];
    assert_eq!(names(Format::TwoCbh, "/d/Big Base.2cbh"), with("Big Base", &two_cbh));
    assert_eq!(names(Format::TwoCbh, "/d/Big Base.2CBH"), with("Big Base", &two_cbh));
    assert_eq!(names(Format::TwoCbh, "/d/Big Base"), with("Big Base", &two_cbh));
    // Only the format's own extension is taken off: a stem may hold a dot.
    assert_eq!(names(Format::TwoCbh, "/d/v1.5"), with("v1.5", &two_cbh));
    let classic = [
        (".cbh", true),
        (".cbg", true),
        (".cba", false),
        (".cbp", true),
        (".cbt", true),
        (".cbc", true),
        (".cbs", true),
        (".cbj", false),
    ];
    assert_eq!(names(Format::Cbh, "/d/Old.cbh"), with("Old", &classic));
    assert_eq!(names(Format::Cbh, "/d/Old"), with("Old", &classic));
    assert_eq!(names(Format::Pgn, "/d/games.PGN"), [("games.PGN".to_string(), true)]);
}

/// Every file `Format::files` calls required is one the reader cannot open
/// the database without, and every other one it can.
#[test]
fn a_database_opens_without_its_optional_files_only() {
    for (format, f) in [(Format::TwoCbh, two_cbh("files")), (Format::Cbh, classic("files"))] {
        let main = f.dir().join(if format == Format::TwoCbh { "db.2cbh" } else { "db.cbh" });
        let files = format.files(&main);
        assert_eq!(files[0].0, main);
        for (file, required) in files {
            let moved = file.with_extension("moved");
            let existed = std::fs::rename(&file, &moved).is_ok();
            assert!(existed || !required, "{} is required but was not built", file.display());
            let opened = Base::open(&main);
            assert_eq!(opened.is_err(), required, "{}", file.display());
            if existed {
                std::fs::rename(&moved, &file).unwrap();
            }
        }
        assert!(Base::open(&main).is_ok());
    }
}

/// A classic database's `.cbj` is required once its `.cbg` or its `.cba` is
/// over 4 GiB, as the reader cannot open it without `.cbj` then, and optional
/// up to 4 GiB. Each file is made sparse at the boundary, which needs a Unix
/// file system.
#[cfg(unix)]
#[test]
fn a_classic_database_requires_its_cbj_past_4_gib() {
    let f = classic("wide");
    let main = f.dir().join("db.cbh");
    let required = || -> Vec<String> {
        Format::Cbh
            .files(&main)
            .into_iter()
            .filter(|(_, required)| *required)
            .map(|(p, _)| p.extension().unwrap().to_string_lossy().into_owned())
            .collect()
    };
    let small = ["cbh", "cbg", "cbp", "cbt", "cbc", "cbs"];
    let large = ["cbh", "cbg", "cbp", "cbt", "cbc", "cbs", "cbj"];
    assert_eq!(required(), small);
    let max = u64::from(u32::MAX);
    for ext in ["cbg", "cba"] {
        let file = main.with_extension(ext);
        let len = std::fs::metadata(&file).unwrap().len();
        let set_len = |len: u64| std::fs::OpenOptions::new().write(true).open(&file).unwrap().set_len(len).unwrap();
        set_len(max);
        assert_eq!(required(), small, ".{ext} of 4 GiB less a byte");
        assert!(Base::open(&main).is_ok(), ".{ext} of 4 GiB less a byte");
        set_len(max + 1);
        assert_eq!(required(), large, ".{ext} of 4 GiB");
        let err = Base::open(&main).err().unwrap_or_else(|| panic!(".{ext} of 4 GiB opened without .cbj"));
        assert!(err.to_string().contains(".cbj"), "{err}");
        set_len(len);
    }
    assert_eq!(required(), small);
}

/// Limits of `n` bytes for a game's move and annotation records, or its PGN
/// text.
fn within(n: usize) -> Limits {
    Limits { game_bytes: n, ..Limits::default() }
}

/// Game 1 of `base` as PGN within `limits`, or the error's text: read alone,
/// and from a batch whose buffers hold its records, which must agree.
fn render_within(base: &Base, limits: Limits) -> Result<String, String> {
    let options = Options::default();
    let alone = base.pgn(1, &options, limits).map(|r| r.pgn).map_err(|e| e.to_string());
    let batch = base.batch(1, 2).unwrap().pgn(1, &options, limits).map(|r| r.pgn).map_err(|e| e.to_string());
    assert_eq!(batch, alone, "{limits:?}");
    alone
}

/// Each format renders a game within the limits it is given (#168), from
/// its own reads and from a batch's buffers alike: a record one byte over
/// them is refused before it is rendered, and one within them renders as
/// [`Limits::format_max`] renders it.
#[test]
fn a_game_is_rendered_within_the_limits_given() {
    let comment = "x".repeat(100);
    // 2CBH: 6 bytes of moves, and a longer annotation record. The second game
    // makes the batch's buffers hold the first one's records.
    let words = [movetable::MOVES, quiet(W, Pawn, "e2", "e4"), movetable::END_OF_LINE];
    let notes = fixture::annotations(&[(0, vec![text(false, language::ENGLISH, &comment)])]);
    let mut b = fixture::Builder::new();
    let moves = b.moves(1, &words);
    let a = b.annotations(&notes);
    b.annotated_game(moves, a);
    b.game(moves);
    let f2 = b.write("view-limits-2cbh");
    // Classic: the same, its sizes counting the whole records.
    use Tok::{End as E, Mv as M};
    let record = move_record(0, None, None, &encode(&Board::startpos(), &[M("e2e4"), E], 0, false));
    let data = [b"\x00\x2a".as_slice(), comment.as_bytes()].concat();
    let classic_notes = annotation_record(1, &[(0, 0x02, &data)]);
    let mut b = fixture_cbh::Builder::new();
    b.game(&record);
    b.annotations(&classic_notes);
    b.game(&record);
    let f1 = b.write("view-limits-cbh");

    for (f, moves, notes) in [(&f2, 6, notes.len()), (&f1, record.len(), classic_notes.len())] {
        let base = Base::open(f.base()).unwrap();
        let whole = render_within(&base, Limits::format_max()).unwrap();
        assert!(whole.contains(&comment), "{whole}");
        assert_eq!(render_within(&base, within(notes)), Ok(whole));
        for (n, what) in [(notes - 1, "annotation record"), (moves - 1, "move record")] {
            let err = render_within(&base, within(n)).unwrap_err();
            assert!(err.contains(what) && err.contains(&format!("over the {n}-byte limit")), "{err}");
        }
    }

    // A PGN file: its game's text.
    let game = format!("[Event \"Limits\"]\n\n1. e4 {{{comment}}} *\n");
    let f = fixture::pgn_file("view-limits", format!("{game}\n{game}").as_bytes());
    let (path, index) = (f.dir().join("db.pgn"), f.dir().join("db.idx"));
    pgnfile::build(&path, &index, 0, CodePage::WESTERN, &mut |_| true).unwrap();
    let db = pgnfile::Database::open(&path, &index, 0, CodePage::WESTERN).unwrap();
    let len = db.record(1).unwrap().len() as usize;
    let base = Base::Pgn(db);
    let whole = render_within(&base, Limits::format_max()).unwrap();
    assert_eq!(whole, game);
    assert_eq!(render_within(&base, within(len)), Ok(whole));
    let err = render_within(&base, within(len - 1)).unwrap_err();
    assert!(err.contains(&format!("over the {}-byte limit", len - 1)), "{err}");
}

/// The entry points that take no limits render within the default ones,
/// which refuse a record no server should render; [`Limits::format_max`]
/// renders it.
#[test]
fn a_game_rendered_without_limits_is_bounded_by_the_default_ones() {
    let big = Limits::DEFAULT.game_bytes;
    let over = |err: cbformat::Error| {
        let err = err.to_string();
        assert!(err.contains(&format!("over the {big}-byte limit")), "{err}");
        err
    };
    let e4 = [movetable::MOVES, quiet(W, Pawn, "e2", "e4"), movetable::END_OF_LINE];
    let notes = fixture::annotations(&[(0, vec![text(false, language::ENGLISH, &"x".repeat(big))])]);
    let f = fixture::one_game("view-limits-default", &e4, Some(&notes));
    let db = v2::Database::open(f.base()).unwrap();
    let err = over(pgn::game(&db, 1).unwrap_err());
    let base = Base::open(f.base()).unwrap();
    assert_eq!(base.pgn(1, &Options::default(), Limits::default()).unwrap_err().to_string(), err);
    assert!(base.pgn(1, &Options::default(), Limits::format_max()).unwrap().pgn.len() > big);

    let mut nulls = vec![movetable::MOVES];
    nulls.extend(std::iter::repeat_n(movetable::NULL_MOVE, big / 2 + 1));
    nulls.push(movetable::END_OF_LINE);
    let f = fixture::one_game("view-limits-default-moves", &nulls, None);
    let db = v2::Database::open(f.base()).unwrap();
    over(pgn::movetext(&db, &db.record(1).unwrap()).unwrap_err());

    let mut b = fixture_cbh::Builder::new();
    b.game(&move_record(0, None, None, &vec![0; big]));
    let f = b.write("view-limits-default-cbh");
    over(pgn::classic_game(&cbh::Database::open(f.base()).unwrap(), 1).unwrap_err());
}
