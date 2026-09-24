//! Compares a classic database with its 2CBH twin, game by game: the header
//! fields both formats store, the names of players and tournaments, the
//! whole move tree (every move of every line, in stored order, with the
//! branch and resume points), and the PGN of each game with its annotations,
//! byte for byte. Prints counts and game ids only, never game contents.
//!
//! With `--full` the PGN is the full form (asavis/oschess-cb-bridge#42), whose
//! `[%cb…]` commands differ between the formats by design, since the classic
//! layouts are kept as their data: they are left out of the comparison, and
//! everything else must be identical.
//!
//! cargo run --release -p cbformat --example cbh_pairs -- <db.cbh> <db.2cbh> [--lang CODES] [--full]

use chesscore::{Board, Move};

use cbformat::pgn::{self, Options};
use cbformat::replay::{TreeVisitor, walk};
use cbformat::{cbh, v2};

#[derive(Default, PartialEq, Debug)]
struct Events(Vec<String>);

impl TreeVisitor for Events {
    fn play(&mut self, _before: &Board, mv: Option<Move>, main: bool) {
        let m = mv.map_or("--".into(), |m| m.to_string());
        self.0.push(if main { m } else { format!("({m})") });
    }
    fn branch(&mut self) {
        self.0.push("<".into());
    }
    fn resume(&mut self) {
        self.0.push(">".into());
    }
}

/// Whether a single-byte (Windows-1252) text can hold `s`. A name the
/// classic format cannot store differs for that reason alone.
fn single_byte(s: &str) -> bool {
    s.chars().all(|c| {
        (c as u32) < 0x80 || ((c as u32) >= 0xa0 && (c as u32) < 0x100) || "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".contains(c)
    })
}

#[derive(Default)]
struct Diff {
    count: u64,
    ids: Vec<u32>,
}

impl Diff {
    fn add(&mut self, id: u32) {
        self.count += 1;
        if self.ids.len() < 5 {
            self.ids.push(id);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: cbh_pairs <db.cbh> <db.2cbh> [--lang CODES] [--full]";
    let [old, new, flags @ ..] = args.as_slice() else { return Err(usage.into()) };
    let mut options = Options::default();
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--lang" => options.languages = Options::with_languages(it.next().ok_or(usage)?.split(',')).languages,
            "--full" => options.full = true,
            _ => return Err(usage.into()),
        }
    }
    let a = cbh::Database::open(old)?;
    let b = v2::Database::open(new)?;
    println!("records            {} / {}", a.record_count(), b.record_count());
    let n = a.record_count().min(b.record_count());
    let names = ["kind", "deleted", "result", "eco", "date", "elo", "round", "white", "black", "event", "site"];
    let mut header: Vec<Diff> = names.iter().map(|_| Diff::default()).collect();
    let (mut unrepresentable, mut truncated, mut texts, mut analyses) = (0u64, 0u64, 0u64, 0u64);
    let (mut trees, mut starts, mut errors_a, mut errors_b, mut compared) =
        (Diff::default(), Diff::default(), Diff::default(), Diff::default(), 0u64);
    let (ea, eb) = (a.entities(), b.entities());
    let mut pgn = PgnDiffs::default();
    for id in 1..=n {
        let (ra, rb) = (a.record(id)?, b.record(id)?);
        // A guiding text has no players, result or date to compare, and a
        // 2CBH analysis is stored as a game by the classic format.
        match (ra.kind(), rb.kind()) {
            (v2::RecordKind::Text, v2::RecordKind::Text) => {
                texts += 1;
                continue;
            }
            (v2::RecordKind::Game, v2::RecordKind::Analysis) => analyses += 1,
            _ => {}
        }
        let analysis = rb.kind() == v2::RecordKind::Analysis;
        let player_a = |pid| ea.player(pid).ok().flatten().map(|p| p.pgn());
        let player_b = |pid| eb.player(pid).ok().flatten().map(|p| p.pgn());
        let ta = ea.tournament(ra.tournament()).ok().flatten();
        let tb = eb.tournament(rb.tournament()).ok().flatten();
        let checks = [
            ra.kind() == rb.kind() || analysis,
            ra.is_deleted() == rb.is_deleted(),
            ra.result() == rb.result() || analysis,
            ra.eco() == rb.eco(),
            ra.played_date() == rb.played_date() || analysis,
            (i32::from(ra.white_elo()), i32::from(ra.black_elo()))
                == (i32::from(rb.white_elo()), i32::from(rb.black_elo())),
            (i32::from(ra.round()), i32::from(ra.subround())) == (i32::from(rb.round()), i32::from(rb.subround())),
            player_a(ra.white()) == player_b(rb.white()),
            player_a(ra.black()) == player_b(rb.black()),
            ta.as_ref().map(|t| &t.title) == tb.as_ref().map(|t| &t.title),
            ta.as_ref().map(|t| &t.place) == tb.as_ref().map(|t| &t.place),
        ];
        let texts_a = [
            player_a(ra.white()).unwrap_or_default(),
            player_a(ra.black()).unwrap_or_default(),
            ta.as_ref().map(|t| t.title.clone()).unwrap_or_default(),
            ta.as_ref().map(|t| t.place.clone()).unwrap_or_default(),
        ];
        let texts_b = [
            player_b(rb.white()).unwrap_or_default(),
            player_b(rb.black()).unwrap_or_default(),
            tb.as_ref().map(|t| t.title.clone()).unwrap_or_default(),
            tb.as_ref().map(|t| t.place.clone()).unwrap_or_default(),
        ];
        let mut name_exception = false;
        for (i, (d, ok)) in header.iter_mut().zip(checks).enumerate() {
            // An analysis record's entity fields are not a game's.
            if !ok && !(analysis && i >= 7) {
                if i >= 7 && !single_byte(&texts_b[i - 7]) {
                    unrepresentable += 1;
                    name_exception = true;
                } else if i >= 7 && !texts_a[i - 7].is_empty() && texts_b[i - 7].starts_with(&texts_a[i - 7]) {
                    truncated += 1;
                    name_exception = true;
                } else {
                    d.add(id);
                }
            }
        }
        if ra.kind() == v2::RecordKind::Game && rb.kind() == v2::RecordKind::Game {
            pgn.compare(id, pgn::classic_game_with(&a, id, &options), pgn::game_with(&b, id, &options), name_exception);
        }
        if ra.kind() != v2::RecordKind::Game {
            continue;
        }
        let mut ev_a = Events::default();
        let res_a = a.moves_of(&ra).and_then(|m| {
            let g = m.moves()?;
            let s = g.start()?;
            cbh::walk(&g, &mut ev_a).map(|_| s)
        });
        let mut ev_b = Events::default();
        let res_b = b.moves_of(&rb).and_then(|m| {
            let g = m.moves()?;
            let s = g.start()?;
            walk(&g, &mut ev_b).map(|_| s)
        });
        match (res_a, res_b) {
            (Ok(sa), Ok(sb)) => {
                compared += 1;
                if sa != sb {
                    starts.add(id);
                }
                if ev_a != ev_b {
                    trees.add(id);
                }
            }
            (Err(_), Ok(_)) => errors_a.add(id),
            (Ok(_), Err(_)) => errors_b.add(id),
            (Err(_), Err(_)) => {
                errors_a.add(id);
                errors_b.add(id);
            }
        }
    }
    println!("trees compared     {compared}");
    println!("guiding texts, trees not compared: {texts}");
    println!("2CBH analyses stored as classic games: {analyses}");
    println!("names beyond a single-byte code page, not compared: {unrepresentable}");
    println!("names cut short by the classic fixed-size field: {truncated}");
    let show = |name: &str, d: &Diff| println!("{name:<18} {} {:?}", d.count, d.ids);
    show("tree differs", &trees);
    show("start differs", &starts);
    show("cbh error", &errors_a);
    show("2cbh error", &errors_b);
    for (name, d) in names.iter().zip(&header) {
        show(&format!("{name} differs"), d);
    }
    println!("PGN compared       {}", pgn.compared);
    println!("PGN identical      {}", pgn.identical);
    println!("PGN tags differ by a known name exception only: {}", pgn.names_only);
    show("PGN tags differ", &pgn.tags);
    show("PGN moves differ", &pgn.movetext);
    show("PGN in comments", &pgn.comments);
    show("PGN in NAGs", &pgn.nags);
    show("annotation status", &pgn.status);
    show("PGN cbh error", &pgn.errors_a);
    show("PGN 2cbh error", &pgn.errors_b);
    Ok(())
}

/// The PGN comparison. A movetext that differs is also counted by where the
/// first difference lies: inside a comment, or at a NAG.
/// The PGN without its `[%cb…]` commands, which the classic format keeps as
/// its own data; a comment left empty goes too.
fn without_cb_commands(mut r: pgn::Rendered) -> pgn::Rendered {
    let mut out = String::with_capacity(r.pgn.len());
    let mut rest = r.pgn.as_str();
    while let Some(at) = rest.find("[%cb") {
        out.push_str(&rest[..at]);
        rest = rest[at..].split_once(']').map_or("", |x| x.1);
    }
    out.push_str(rest);
    r.pgn = out.replace("{ }", "{}").replace(" {}", "").replace("{} ", "");
    r
}

#[derive(Default)]
struct PgnDiffs {
    compared: u64,
    identical: u64,
    names_only: u64,
    tags: Diff,
    movetext: Diff,
    comments: Diff,
    nags: Diff,
    status: Diff,
    errors_a: Diff,
    errors_b: Diff,
}

impl PgnDiffs {
    fn compare(
        &mut self,
        id: u32,
        a: cbformat::Result<pgn::Rendered>,
        b: cbformat::Result<pgn::Rendered>,
        name_exception: bool,
    ) {
        let (a, b) = match (a, b) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(_), Ok(_)) => return self.errors_a.add(id),
            (Ok(_), Err(_)) => return self.errors_b.add(id),
            (Err(_), Err(_)) => {
                self.errors_a.add(id);
                return self.errors_b.add(id);
            }
        };
        self.compared += 1;
        if a.annotations != b.annotations {
            self.status.add(id);
        }
        let (a, b) = (without_cb_commands(a), without_cb_commands(b));
        if a.pgn == b.pgn {
            self.identical += 1;
            return;
        }
        let split = |s: &str| s.split_once("\n\n").map(|(t, m)| (t.to_string(), m.to_string())).unwrap_or_default();
        let ((tags_a, moves_a), (tags_b, moves_b)) = (split(&a.pgn), split(&b.pgn));
        if moves_a != moves_b {
            self.movetext.add(id);
            match first_difference(&moves_a, &moves_b) {
                Where::Comment => self.comments.add(id),
                Where::Nag => self.nags.add(id),
                Where::Moves => {}
            }
        } else if tags_a != tags_b {
            if name_exception { self.names_only += 1 } else { self.tags.add(id) }
        }
    }
}

/// Where two movetexts first differ.
#[derive(Debug, PartialEq, Eq)]
enum Where {
    Comment,
    Nag,
    Moves,
}

/// Where `a` and `b` first differ: inside a comment, at a NAG, or elsewhere.
/// Works on bytes, so a difference inside a multi-byte character is found at
/// that character.
fn first_difference(a: &str, b: &str) -> Where {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let at = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let before = &a[..at];
    let open = before.iter().filter(|&&c| c == b'{').count();
    let close = before.iter().filter(|&&c| c == b'}').count();
    if open > close {
        Where::Comment
    } else if before.ends_with(b"$") || a[at..].starts_with(b"$") || b[at..].starts_with(b"$") {
        Where::Nag
    } else {
        Where::Moves
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differences_are_placed() {
        assert_eq!(first_difference("1. e4 {a} e5", "1. e4 {b} e5"), Where::Comment);
        assert_eq!(first_difference("1. e4 $1 e5", "1. e4 $2 e5"), Where::Nag);
        assert_eq!(first_difference("1. e4 $1 e5", "1. e4 e5"), Where::Nag);
        assert_eq!(first_difference("1. e4 e5", "1. e4 $1 e5"), Where::Nag);
        assert_eq!(first_difference("1. e4 e5", "1. d4 e5"), Where::Moves);
        assert_eq!(first_difference("1. e4", "1. e4 e5"), Where::Moves);
        assert_eq!(first_difference("1. e4 e5", "1. e4"), Where::Moves);
    }

    #[test]
    fn a_difference_inside_a_multi_byte_character() {
        // é and ê share their first UTF-8 byte.
        assert_eq!(first_difference("1. e4 {caf\u{e9}} e5", "1. e4 {caf\u{ea}} e5"), Where::Comment);
        assert_eq!(first_difference("1. e4 {\u{e9}} e5", "1. e4 {\u{1f600}} e5"), Where::Comment);
        assert_eq!(first_difference("\u{e9}", "\u{ea}"), Where::Moves);
    }
}
