//! Checks that the full PGN form (asavis/oschess-cb-bridge#42) loses nothing.
//! For every game of each database, of either format, it reads the game's
//! annotations as decoded, recovers them from the game's full form as a reader
//! would, and compares the two, value by value: every text with its language
//! and placement, the three symbol slots, every coloured square and arrow of
//! every colour, the data of every other annotation, and the undecoded rest of
//! a record. A record whose header or annotations cannot be read is counted as
//! a failure, never as a game without annotations, and a full form holding a
//! value that does not decode as a failure, never as an empty value. Prints
//! counts only.
//!
//! `cargo run --release -p cbformat --example full_form_census -- <database>…`

use std::collections::BTreeMap;

use cbformat::Limits;
use cbformat::fixture::{unbase64url, unpercent};
use cbformat::game::{Annotation, Head, RecordKind};
use cbformat::pgn::{self, Options};
use cbformat::view::{Base, Format};

/// One annotation, as the census compares it: its kind and its value.
type Item = (String, Vec<u8>);

#[derive(Debug, Default, PartialEq)]
struct Totals {
    databases: u64,
    /// Databases that could not be opened.
    unopened: u64,
    games: u64,
    annotated: u64,
    /// Records whose header could not be read.
    unreadable_headers: u64,
    /// Games whose annotation record could not be decoded.
    unreadable_annotations: u64,
    /// Annotated games whose full form could not be written.
    unwritten: u64,
    /// Annotated games whose full form holds a value a reader cannot decode.
    /// They count as differing too.
    undecodable: u64,
    /// Annotated games whose recovered annotations differ from the decoded ones.
    differing: u64,
    /// Decoded and recovered annotations, by kind.
    kinds: BTreeMap<String, (u64, u64)>,
}

/// A command's `key=value` fields.
fn fields(body: &str) -> BTreeMap<&str, &str> {
    body.split(';').filter_map(|f| f.split_once('=')).collect()
}

/// The type code a named command stands for.
fn code_of(name: &str) -> Option<&'static str> {
    Some(match name {
        "cbquote" => "13",
        "cbcritical" => "18",
        "cbpawns" => "14",
        "cbpath" => "15",
        "cbcolour" => "23",
        "cblink" => "1c",
        "cbvideo" => "20",
        "cbtraining" => "09",
        "cbtimecontrol" => "24",
        _ => return None,
    })
}

/// The annotations a reader recovers from a full form. A `[%cbtext]` replaces
/// the visible `[%lang]` comment right before it, unless it stands `alone`.
/// `None` when a value does not decode: a malformed value is a failure, never
/// the empty value a well-formed command may hold.
fn recovered(pgn: &str, format: Format) -> Option<Vec<Item>> {
    let movetext = pgn.split_once("\n\n").map_or(pgn, |x| x.1);
    let mut texts: Vec<Item> = Vec::new();
    let mut others: Vec<Item> = Vec::new();
    let mut last_visible = false;
    for body in movetext.split('{').skip(1).map(|c| c.split('}').next().unwrap_or("")) {
        if let Some(rest) = body.strip_prefix("[%lang ") {
            let (code, text) = rest.split_once("] ").unwrap_or((rest, ""));
            texts.push((format!("text {code} after"), text.as_bytes().to_vec()));
            last_visible = true;
            continue;
        }
        if let Some(rest) = body.strip_prefix("[%cbtext ") {
            let f = fields(rest.trim_end_matches(']'));
            let placement = if f.get("before") == Some(&"1") { "before" } else { "after" };
            // A value left out is empty; one that does not decode fails.
            let value = |k: &str| f.get(k).map_or(Some(Vec::new()), |v| unpercent(v));
            let lang = String::from_utf8_lossy(&value("lang")?).into_owned();
            let item = (format!("text {lang} {placement}"), value("value")?);
            match texts.last_mut().filter(|_| last_visible && !f.contains_key("alone")) {
                Some(visible) => *visible = item,
                None => texts.push(item),
            }
            last_visible = false;
            continue;
        }
        last_visible = false;
        for cmd in body.split("[%").skip(1).map(|c| c.split(']').next().unwrap_or("")) {
            let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
            let f = fields(rest);
            // Data left out is empty, as the writer leaves out empty data;
            // data that does not decode fails.
            let data = || f.get("data").map_or(Some(Vec::new()), |d| unbase64url(d));
            let item = match name {
                "cbsymbols" | "cbsquares" | "cbarrows" => (name.trim_start_matches("cb").to_string(), data()?),
                "cbraw" => (f.get("type").unwrap_or(&"?").to_string(), data()?),
                "cbrest" => (format!("rest {}", f.get("type").unwrap_or(&"?")), data()?),
                "mdl" => {
                    let bits: u32 = rest.trim().parse().unwrap_or(u32::MAX);
                    let b = if format == Format::Cbh { bits.to_be_bytes() } else { bits.to_le_bytes() };
                    ("22".into(), b.to_vec())
                }
                n => match code_of(n) {
                    Some(code) => (code.to_string(), data()?),
                    None => continue,
                },
            };
            others.push(item);
        }
    }
    texts.extend(others);
    Some(texts)
}

/// Whether a reader recovers exactly the annotations decoded, in any order.
/// A full form that does not decode (`None`) recovers nothing.
fn same(want: &[Item], got: Option<&[Item]>) -> bool {
    let Some(got) = got else { return false };
    let (mut want, mut got) = (want.to_vec(), got.to_vec());
    want.sort();
    got.sort();
    want == got
}

/// The annotations as decoded, in the census's terms.
fn decoded(a: &cbformat::game::GameAnnotations) -> Vec<Item> {
    let cb = |sq: u8| (sq % 8) * 8 + sq / 8 + 1;
    let mut out: Vec<Item> = Vec::new();
    for an in a.blocks.iter().flat_map(|b| &b.annotations) {
        out.push(match an {
            Annotation::Text { before, language, text } => {
                let placement = if *before { "before" } else { "after" };
                (format!("text {} {placement}", pgn::language_code(*language)), text.as_bytes().to_vec())
            }
            Annotation::Symbols { on_move, on_position, prefix } => {
                ("symbols".into(), vec![*on_move, *on_position, *prefix])
            }
            Annotation::Squares(v) => ("squares".into(), v.iter().flat_map(|s| [s.colour, cb(s.square)]).collect()),
            Annotation::Arrows(v) => {
                ("arrows".into(), v.iter().flat_map(|a| [a.colour, cb(a.from), cb(a.to)]).collect())
            }
            Annotation::Other { code, data } => (format!("{code:02x}"), data.clone()),
        });
    }
    if let Some(u) = a.stopped_at {
        out.push((format!("rest {:02x}", u.type_code), a.undecoded.clone()));
    }
    out
}

/// The kind a census row counts an item under: every text is "text".
fn kind(item: &Item) -> String {
    if item.0.starts_with("text ") { "text".into() } else { item.0.split(' ').next().unwrap_or("").to_string() }
}

fn census<'a>(paths: impl IntoIterator<Item = &'a str>) -> Totals {
    let mut t = Totals::default();
    let full = Options { full: true, ..Options::default() };
    for path in paths {
        t.databases += 1;
        let Ok(base) = Base::open(path) else {
            t.unopened += 1;
            continue;
        };
        for id in 1..=base.record_count() {
            let h = match base.header(id) {
                Ok(h) => h,
                Err(_) => {
                    t.unreadable_headers += 1;
                    continue;
                }
            };
            if h.kind() != RecordKind::Game {
                continue;
            }
            t.games += 1;
            let a = match base.annotations_of(&h) {
                Ok(Some(a)) if !a.is_empty() => a,
                Ok(_) => continue,
                Err(_) => {
                    t.unreadable_annotations += 1;
                    continue;
                }
            };
            t.annotated += 1;
            let want = decoded(&a);
            let got = match base.pgn(id, &full, Limits::format_max()) {
                Ok(r) => recovered(&r.pgn, base.format()),
                Err(_) => {
                    t.unwritten += 1;
                    Some(Vec::new())
                }
            };
            if got.is_none() {
                t.undecodable += 1;
            }
            if !same(&want, got.as_deref()) {
                t.differing += 1;
            }
            for item in &want {
                t.kinds.entry(kind(item)).or_default().0 += 1;
            }
            for item in got.iter().flatten() {
                t.kinds.entry(kind(item)).or_default().1 += 1;
            }
        }
    }
    t
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let t = census(args.iter().map(String::as_str));
    println!(
        "databases {} (not opened {}), games {}, annotated {}; unreadable headers {}, unreadable annotations {}, \
         full form not written {}, not decoded {}; games whose recovered annotations differ {}",
        t.databases,
        t.unopened,
        t.games,
        t.annotated,
        t.unreadable_headers,
        t.unreadable_annotations,
        t.unwritten,
        t.undecodable,
        t.differing
    );
    println!("kind: decoded / recovered");
    for (k, (d, r)) in &t.kinds {
        println!("  {k:<8} {d:>9} / {r:>9}{}", if d == r { "" } else { "   DIFFERS" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbformat::fixture::{Builder, annotations, arrows, quiet, squares, symbols, text};
    use cbformat::game::{Block, GameAnnotations, language};
    use cbformat::movetable::{self, Color, Piece};

    fn database(name: &str, contents: &[Vec<u8>]) -> cbformat::fixture::TempDb {
        let mut b = Builder::new();
        let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
        for c in contents {
            let a = b.annotations(c);
            b.annotated_game(e4, a);
        }
        b.write(name)
    }

    #[test]
    fn every_value_is_recovered() {
        let games = [
            annotations(&[(-1, vec![symbols(1, 10, 140), squares(&[(7, "a1")])])]),
            annotations(&[(0, vec![text(false, 0, "A{B}\nC"), text(false, 0, "A(B) C"), text(false, 0, " ")])]),
            annotations(&[(0, vec![text(true, 1, "vor"), arrows(&[(8, "b1", "c3")]), vec![0x22, 0, 4, 0, 0, 0]])]),
            annotations(&[(0, vec![text(false, 0, "ok"), vec![0x1a, 0, 9, 8, 7]])]),
        ];
        let db = database("census-values", &games);
        let t = census([db.base().to_str().unwrap()]);
        assert_eq!(
            (t.annotated, t.differing, t.unreadable_annotations, t.unwritten, t.undecodable),
            (4, 0, 0, 0, 0),
            "{t:?}"
        );
        for k in ["symbols", "squares", "arrows", "text", "22", "rest"] {
            let (d, r) = t.kinds[k];
            assert!(d > 0 && d == r, "{k}: {t:?}");
        }
    }

    #[test]
    fn an_unreadable_record_is_counted() {
        // A web link that claims a 2,147,483,647-byte URL it does not hold.
        let mut link = vec![0x1c, 0, 1];
        link.extend(i32::MAX.to_le_bytes());
        let games = [annotations(&[(0, vec![link])]), annotations(&[(0, vec![text(false, 0, "ok")])])];
        let db = database("census-damaged", &games);
        let t = census([db.base().to_str().unwrap()]);
        assert_eq!((t.games, t.annotated, t.unreadable_annotations, t.differing), (2, 1, 1, 0), "{t:?}");
    }

    /// An empty text and an empty list of squares are recovered as written;
    /// a value that does not decode is a failure, never taken for them.
    #[test]
    fn a_value_that_does_not_decode_is_not_an_empty_one() {
        let db = database("census-empty", &[annotations(&[(0, vec![text(false, 0, ""), squares(&[])])])]);
        let t = census([db.base().to_str().unwrap()]);
        assert_eq!((t.annotated, t.differing, t.undecodable), (1, 0, 0), "{t:?}");
        let one = |a: Annotation| GameAnnotations {
            blocks: vec![Block { position: 0, annotations: vec![a] }],
            ..GameAnnotations::default()
        };
        let empty_text = one(Annotation::Text { before: false, language: language::ENGLISH, text: String::new() });
        let no_squares = one(Annotation::Squares(Vec::new()));
        let cases = [
            (
                &empty_text,
                "1. e4 {[%cbtext lang=en;alone=1;value=]}",
                ["1. e4 {[%cbtext lang=en;alone=1;value=%]}", "1. e4 {[%cbtext lang=e%zz;alone=1;value=]}"],
            ),
            (
                &no_squares,
                "1. e4 {[%cbsquares data=]}",
                ["1. e4 {[%cbsquares data=!!!!]}", "1. e4 {[%cbsquares data=A]}"],
            ),
        ];
        for (a, good, bad) in cases {
            let want = decoded(a);
            assert!(same(&want, recovered(good, Format::TwoCbh).as_deref()), "{good}");
            for bad in bad {
                assert!(!same(&want, recovered(bad, Format::TwoCbh).as_deref()), "{bad}");
            }
        }
    }
}
