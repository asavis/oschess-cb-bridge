//! Guiding texts (asavis/oschess-cb-bridge#324): the classic format's
//! versions 1 and 2 read into paragraphs with their styles, diagrams and
//! links, its version 3 and 2CBH as HTML. Every record here is built by hand;
//! the names are made up.

use cbformat::codepage::CodePage;
use cbformat::fixture::{self, TempDb};
use cbformat::fixture_cbh;
use cbformat::fixture_cbh::guide::{body, content_v1, counted, diagram, formatting, game_link, style, u32s};
use cbformat::game::guide::{Body, Content, GameLink, GuidingText, Paragraph, Span, Style};
use cbformat::game::language;
use cbformat::{Limits, cbh, v2};

const LIMIT: usize = Limits::DEFAULT.game_bytes;

fn read_classic(db: &TempDb, computer: CodePage, id: u32) -> cbformat::Result<GuidingText> {
    let db = cbh::Database::open_in(db.dir().join("db.cbh"), computer).unwrap();
    db.guiding_text(&db.record(id).unwrap(), LIMIT)
}

fn text(t: &str, style: &Style) -> Span {
    Span::Text { text: t.to_string(), style: style.clone() }
}

fn paragraphs(t: &GuidingText) -> &[Paragraph] {
    match &t.contents[0].body {
        Body::Paragraphs(p) => p,
        Body::Html(h) => panic!("HTML: {h}"),
    }
}

/// A text of version 1 whose formatting data holds every object the reader
/// reads, beside a picture it leaves out.
#[test]
fn a_version_1_text_reads_its_styles_diagrams_and_links() {
    let mut t = b"Title\rWhite plays ".to_vec();
    let diagram_at = t.len();
    t.extend(b"\x04 and wins.\r");
    let label_at = t.len();
    t.extend(b"\x04 See ");
    let game_at = t.len();
    t.extend(b"\x04 and ");
    let link_at = t.len();
    t.push(4);
    let board = diagram(&[("e1", 1), ("e2", 6), ("e8", 9)], 0);
    let objects = [
        (0x17, diagram_at + 1, counted(b"picture.bmp")),
        (0x09, diagram_at + 1, board.clone()),
        (0x11, diagram_at + 1, [&board[..], &[1, 0, 1]].concat()),
        (0x0c, label_at + 1, counted(b"1.")),
        (0x1a, game_at + 1, game_link("Doe,Jane", "Roe,Richard", "Testville op", Some("1.5"))),
        (0x02, game_at + 1, game_link("Doe,Jane", "Roe,Richard", "Testville op", None)),
        (0x19, link_at + 1, [&counted(b"Index")[..], &[0; 8]].concat()),
        (0x05, link_at + 1, [&counted(b"Index")[..], &[0; 6]].concat()),
    ];
    let styles = [(0, style("Arial", 18, false, false)), (1, style("Times", 24, true, true))];
    let f = formatting(&objects, &styles, &[(5, 1), (t.len() - 5, 0)], false);
    let mut b = fixture_cbh::Builder::new();
    b.text_body(&body(1, b"Chapter", 1, &content_v1(language::ANY, &t, &f)));
    let db = b.write("guide-v1");
    let body_style = Style { font: "Arial".into(), size: 18, ..Style::default() };
    let title_style = Style { font: "Times".into(), size: 24, bold: true, italic: true, underline: false };
    let read = read_classic(&db, CodePage::WESTERN, 1).unwrap();
    assert_eq!(read.contents.len(), 1);
    assert_eq!(read.contents[0].language, language::ANY);
    assert_eq!(
        paragraphs(&read),
        [
            Paragraph { spans: vec![text("Title", &title_style)] },
            Paragraph {
                spans: vec![
                    text("White plays ", &body_style),
                    Span::Diagram { board: "4k3/8/8/8/8/8/4P3/4K3".into() },
                    text(" and wins.", &body_style),
                ]
            },
            Paragraph {
                spans: vec![
                    // The list label and the text after it share a style: one span.
                    text("1. See ", &body_style),
                    Span::Game(GameLink {
                        label: "1.5".into(),
                        white: "Doe,Jane".into(),
                        black: "Roe,Richard".into(),
                        event: "Testville op".into(),
                    }),
                    text(" and ", &body_style),
                    Span::TextLink { title: "Index".into() },
                ]
            },
        ]
    );
}

/// Version 2 places an object past 65,535 bytes with a `u32` position, and
/// splits a long text into runs of fewer than 65,535 bytes, a length that
/// would end the runs.
#[test]
fn a_version_2_text_places_objects_past_a_u16() {
    let mut t = vec![b'a'; 70_000];
    t[69_999] = 4;
    let f = formatting(
        &[(0x09, 70_000, diagram(&[("a1", 1), ("h8", 9)], 0))],
        &[(0, style("Arial", 18, true, false))],
        &[(60_000, 0), (10_000, 0)],
        true,
    );
    let mut content = language::ENGLISH.to_le_bytes().to_vec();
    content.extend(u32s(t.len()));
    content.extend(&t);
    content.extend(u32s(f.len()));
    content.extend(&f);
    let mut b = fixture_cbh::Builder::new();
    b.text_body(&body(2, b"Chapter", 1, &content));
    let db = b.write("guide-v2");
    let read = read_classic(&db, CodePage::WESTERN, 1).unwrap();
    let spans = &paragraphs(&read)[0].spans;
    assert_eq!(spans.len(), 2, "{spans:?}");
    // Both runs are of one style: one span, bold.
    assert!(matches!(&spans[0], Span::Text { text, style } if text.len() == 69_999 && style.bold));
    assert_eq!(spans[1], Span::Diagram { board: "7k/8/8/8/8/8/8/K7".into() });
}

#[test]
fn a_version_3_text_is_html_as_stored() {
    let html = "<html><body><p>Caf\u{e9}</p></body></html>";
    let mut content = language::GERMAN.to_le_bytes().to_vec();
    content.extend(u32s(html.len()));
    content.extend(html.as_bytes());
    content.extend([0; 4]);
    let mut b = fixture_cbh::Builder::new();
    b.text_body(&body(3, b"Chapter", 1, &content));
    let db = b.write("guide-v3");
    let read = read_classic(&db, CodePage::WESTERN, 1).unwrap();
    assert_eq!(read.contents, [Content { language: language::GERMAN, body: Body::Html(html.into()) }]);
}

/// Text in Windows-1251 reads as Russian on either computer, with
/// ChessBase's piece byte as a figurine, as a comment does.
#[test]
fn single_byte_text_reads_as_comments_do() {
    // `Ход ` and a knight to f3, then the end of the paragraph.
    let t = b"\xd5\xee\xe4 \xa4f3\r";
    let f = formatting(&[], &[(0, style("Arial", 18, false, false))], &[(t.len(), 0)], false);
    let mut b = fixture_cbh::Builder::new();
    b.text_body(&body(1, b"Chapter", 1, &content_v1(language::ENGLISH, t, &f)));
    let db = b.write("guide-cyrillic");
    for computer in [CodePage::WESTERN, CodePage::CYRILLIC] {
        let read = read_classic(&db, computer, 1).unwrap();
        let p = paragraphs(&read);
        assert_eq!(p.len(), 1, "{computer:?}: a closing line break makes no empty paragraph");
        assert!(matches!(&p[0].spans[..], [Span::Text { text, .. }] if text == "Ход ♘f3"), "{computer:?}: {p:?}");
    }
}

/// Formatting data that does not read leaves the text unstyled and without
/// objects; runs that end early or name no style leave the rest unstyled.
#[test]
fn damaged_formatting_leaves_plain_text() {
    let t = b"One \x04two";
    let good = formatting(
        &[(0x09, 5, diagram(&[("e1", 1)], 0))],
        &[(0, style("Arial", 18, true, false))],
        &[(t.len(), 0)],
        false,
    );
    let plain = Style::default();
    let bold = Style { font: "Arial".into(), size: 18, bold: true, ..Style::default() };
    let cases: [(&str, Vec<u8>, Vec<Span>); 4] = [
        ("cut in an object", good[..20].to_vec(), vec![text("One two", &plain)]),
        ("cut before the end of the runs", good[..good.len() - 8].to_vec(), vec![text("One two", &plain)]),
        (
            "runs that end early",
            formatting(&[], &[(0, style("Arial", 18, true, false))], &[(2, 0)], false),
            vec![text("On", &bold), text("e two", &plain)],
        ),
        (
            "a run of a style not defined",
            formatting(&[], &[(0, style("Arial", 18, true, false))], &[(t.len(), 4)], false),
            vec![text("One two", &plain)],
        ),
    ];
    for (i, (what, f, spans)) in cases.into_iter().enumerate() {
        let mut b = fixture_cbh::Builder::new();
        b.text_body(&body(1, b"Chapter", 1, &content_v1(language::ENGLISH, t, &f)));
        let db = b.write(&format!("guide-damaged-{i}"));
        let read = read_classic(&db, CodePage::WESTERN, 1).unwrap();
        assert_eq!(paragraphs(&read), [Paragraph { spans }], "{what}");
    }
}

/// A record whose contents do not fill it as the format says is an error, as
/// is a version not known and a record that is not a text.
#[test]
fn a_damaged_record_is_an_error() {
    let content = content_v1(language::ENGLISH, b"text", &[]);
    let mut b = fixture_cbh::Builder::new();
    b.text_body(&body(1, b"Chapter", 1, &content[..content.len() - 3]));
    b.text_body(&body(9, b"Chapter", 1, &content));
    b.text_body(&body(1, b"Chapter", 2, &content));
    b.game(&fixture_cbh::move_record(0, None, None, &[]));
    let db = b.write("guide-errors");
    for id in 1..=4 {
        assert!(read_classic(&db, CodePage::WESTERN, id).is_err(), "record {id}");
    }
}

/// A 2CBH text: one HTML document per language that has one, its nation read
/// as a text annotation's language.
#[test]
fn a_2cbh_text_is_html_per_language() {
    let mut entries = Vec::new();
    for (nation, html) in [(42i32, "<p>English</p>"), (53, ""), (0, "<p>Any</p>")] {
        entries.extend(nation.to_le_bytes());
        entries.extend(u32s(html.len()));
        entries.extend(html.as_bytes());
    }
    let mut content = 5u16.to_le_bytes().to_vec();
    content.extend(u32s(entries.len() + 4));
    content.extend(u32s(3));
    content.extend(entries);
    let mut b = fixture::Builder::new();
    let at = b.move_bytes(v2::TEXT_TAG, &content);
    b.game(at)[0] |= 2;
    let moves = b.moves(1, &[cbformat::movetable::MOVES, cbformat::movetable::END_OF_LINE]);
    b.game(moves);
    let tmp = b.write("guide-2cbh");
    let db = v2::Database::open(tmp.base()).unwrap();
    let read = db.guiding_text(&db.record(1).unwrap(), LIMIT).unwrap();
    assert_eq!(
        read.contents,
        [
            Content { language: language::ENGLISH, body: Body::Html("<p>English</p>".into()) },
            Content { language: language::ANY, body: Body::Html("<p>Any</p>".into()) },
        ]
    );
    assert!(db.guiding_text(&db.record(2).unwrap(), LIMIT).is_err(), "a game is not a text");
}

/// What a run case names: the case, the text, its runs and the spans expected.
type Case<'a> = (&'a str, &'a [u8], &'a [(usize, u16)], Vec<Span>);

/// Runs cut the text only where its styles change: each character reads as it
/// does in the whole text, whichever run it falls in, and neighbouring runs of
/// one style make one span.
#[test]
fn a_run_boundary_does_not_change_how_the_text_reads() {
    let plain = Style { font: "Arial".into(), size: 18, ..Style::default() };
    let bold = Style { bold: true, ..plain.clone() };
    let styles = [
        (0, style("Arial", 18, false, false)),
        (1, style("Arial", 18, true, false)),
        (2, style("Arial", 18, false, false)),
    ];
    let cases: [Case<'_>; 4] = [
        // A knight and its move, in two styles.
        ("a figurine", b"\xa4f3", &[(1, 1), (2, 0)], vec![text("♘", &bold), text("f3", &plain)]),
        // `Božidar`, the 0x9e between letters in a run of its own.
        (
            "a diagram mark's byte",
            b"Bo\x9eidar",
            &[(2, 0), (1, 1), (4, 0)],
            vec![text("Bo", &plain), text("ž", &bold), text("idar", &plain)],
        ),
        // `Ход` in UTF-8, a run ending inside its first character.
        ("a UTF-8 character", "Ход".as_bytes(), &[(1, 1), (5, 0)], vec![text("Х", &bold), text("од", &plain)]),
        // Two runs of styles that read the same are one span.
        ("one style twice", b"one two", &[(4, 0), (3, 2)], vec![text("one two", &plain)]),
    ];
    for (i, (what, t, runs, spans)) in cases.into_iter().enumerate() {
        let f = formatting(&[], &styles, runs, false);
        let mut b = fixture_cbh::Builder::new();
        b.text_body(&body(1, b"Chapter", 1, &content_v1(language::ENGLISH, t, &f)));
        let db = b.write(&format!("guide-runs-{i}"));
        let read = read_classic(&db, CodePage::WESTERN, 1).unwrap();
        assert_eq!(paragraphs(&read), [Paragraph { spans }], "{what}");
    }
}

/// A font name longer than any real one is cut, so that a style's spans do
/// not each carry a copy of a stored name of any length.
#[test]
fn a_long_font_name_is_cut() {
    let long = "F".repeat(1_000);
    let t = b"one\rtwo";
    let f = formatting(&[], &[(0, style(&long, 18, false, false))], &[(t.len(), 0)], false);
    let mut b = fixture_cbh::Builder::new();
    b.text_body(&body(1, b"Chapter", 1, &content_v1(language::ENGLISH, t, &f)));
    let db = b.write("guide-long-font");
    let read = read_classic(&db, CodePage::WESTERN, 1).unwrap();
    for p in paragraphs(&read) {
        assert!(matches!(&p.spans[0], Span::Text { style, .. } if style.font.chars().count() == cbh::MAX_FONT_CHARS));
    }
}

/// Contents that do not fill the record exactly are an error: a count of none
/// over a body, and bytes after the last content.
#[test]
fn contents_must_fill_the_record() {
    let content = content_v1(language::ENGLISH, b"text", &[]);
    let mut b = fixture_cbh::Builder::new();
    b.text_body(&[&body(1, b"Chapter", 0, &[])[..], &content].concat());
    b.text_body(&[&body(1, b"Chapter", 1, &content)[..], &[0]].concat());
    let db = b.write("guide-extent");
    for id in 1..=2 {
        assert!(read_classic(&db, CodePage::WESTERN, id).is_err(), "record {id}");
    }
}

/// A 2CBH text whose size field is not its content's, or whose entries do not
/// fill it, is an error.
#[test]
fn a_2cbh_text_must_fill_its_record() {
    let entry = [&42i32.to_le_bytes()[..], &u32s(9), b"<p>x</p>!"].concat();
    let content = |size: usize, count: usize| [&5u16.to_le_bytes()[..], &u32s(size), &u32s(count), &entry].concat();
    let fits = 4 + entry.len();
    let mut b = fixture::Builder::new();
    for c in [content(fits, 1), content(0, 1), content(0xffff_ffff, 1), content(fits, 0)] {
        let at = b.move_bytes(v2::TEXT_TAG, &c);
        b.game(at)[0] |= 2;
    }
    let tmp = b.write("guide-2cbh-extent");
    let db = v2::Database::open(tmp.base()).unwrap();
    assert!(db.guiding_text(&db.record(1).unwrap(), LIMIT).is_ok());
    for id in 2..=4 {
        assert!(db.guiding_text(&db.record(id).unwrap(), LIMIT).is_err(), "record {id}");
    }
}

/// A game link's pair gives its label whichever of the two comes first.
#[test]
fn a_game_link_keeps_its_label_in_either_order() {
    let t = b"See \x04.";
    let labelled = game_link("Doe,Jane", "Roe,Richard", "Testville op", Some("1.5"));
    let plain = game_link("Doe,Jane", "Roe,Richard", "Testville op", None);
    for (i, objects) in
        [[(0x1a, 5, labelled.clone()), (0x02, 5, plain.clone())], [(0x02, 5, plain), (0x1a, 5, labelled)]]
            .into_iter()
            .enumerate()
    {
        let f = formatting(&objects, &[(0, style("Arial", 18, false, false))], &[(t.len(), 0)], false);
        let mut b = fixture_cbh::Builder::new();
        b.text_body(&body(1, b"Chapter", 1, &content_v1(language::ENGLISH, t, &f)));
        let db = b.write(&format!("guide-pair-{i}"));
        let read = read_classic(&db, CodePage::WESTERN, 1).unwrap();
        let games: Vec<&GameLink> = paragraphs(&read)[0]
            .spans
            .iter()
            .filter_map(|span| match span {
                Span::Game(link) => Some(link),
                _ => None,
            })
            .collect();
        assert_eq!(games.len(), 1, "order {i}");
        assert_eq!(games[0].label, "1.5", "order {i}");
    }
}
