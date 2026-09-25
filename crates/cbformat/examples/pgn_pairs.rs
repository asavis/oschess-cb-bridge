//! Compares a PGN file with the 2CBH database it was exported from, game by
//! game in number order: the fields of the game list and the main line.
//! The 2CBH side is read by its own reader, its main line replayed from the
//! move record; the PGN side is read as the bridge reads a PGN file, through
//! its index, its main line played from its text. Prints counts, and the
//! numbers of the first games that differ in each field; never a game's
//! content.
//!
//! `cargo run --release -p cbformat --example pgn_pairs -- <database.2cbh> <export.pgn> [--code-page N]`

use std::collections::BTreeMap;
use std::path::Path;

use cbformat::codepage::CodePage;
use cbformat::game::RecordKind;
use cbformat::pgnfile::lex::Lexer;
use cbformat::pgnfile::line::main_line;
use cbformat::pgnfile::{self, MAX_TEXT};
use cbformat::replay::{self, start_board};
use cbformat::v2;
use chesscore::Move;

/// The main line of a 2CBH game, replayed from its move record.
fn replayed(db: &v2::Database, r: &v2::Record) -> Option<Vec<Move>> {
    let data = db.moves_of(r).ok()?;
    let moves = data.moves().ok()?;
    let mut board = moves.start().and_then(|s| start_board(&s)).ok()?;
    let mut out = Vec::new();
    for word in moves.main_line() {
        match replay::play(&mut board, word) {
            Ok(Some(mv)) => out.push(mv),
            _ => break,
        }
    }
    Some(out)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [base, pgn, rest @ ..] = args.as_slice() else {
        return Err("usage: pgn_pairs <database.2cbh> <export.pgn> [--code-page N]".into());
    };
    let page = match rest {
        [flag, n] if flag == "--code-page" => CodePage::new(n.parse()?),
        _ => CodePage::WESTERN,
    };
    let db = v2::Database::open(base)?;
    let index = std::env::temp_dir().join(format!("pgn-pairs-{}.head", std::process::id()));
    let started = std::time::Instant::now();
    pgnfile::build(Path::new(pgn), &index, 0, page, &mut |_| true)?;
    let built = started.elapsed().as_secs_f64();
    let pdb = pgnfile::Database::open(Path::new(pgn), &index, 0, page)?;
    println!("2cbh records {}, pgn games {} (index built in {built:.1} s)", db.record_count(), pdb.record_count());

    // The PGN has the games only: the n-th game of the PGN is the n-th game
    // record of the database, texts and analyses passed over.
    let mut equal: BTreeMap<&str, u64> = BTreeMap::new();
    let mut differ: BTreeMap<&str, (u64, Vec<u32>)> = BTreeMap::new();
    let mut lexer = Lexer::new();
    let mut n = 0u32;
    let e = db.entities();
    for id in 1..=db.record_count() {
        let r = db.record(id)?;
        if r.kind() != RecordKind::Game {
            continue;
        }
        n += 1;
        if n > pdb.record_count() {
            break;
        }
        let p = pdb.record(n)?;
        let player = |p: Option<cbformat::game::Player>| p.map(|p| p.pgn()).unwrap_or_default();
        let tournament = e.tournament(r.tournament())?;
        let ptournament = pdb.tournament(p.tournament())?;
        let text = |t: &Option<cbformat::game::Tournament>, place: bool| {
            t.as_ref().map(|t| if place { t.place.clone() } else { t.title.clone() }).unwrap_or_default()
        };
        let blank = |s: String| if matches!(s.trim(), "?" | "-") { String::new() } else { s.trim().to_string() };
        let mut fields: Vec<(&str, bool)> = vec![
            ("white", blank(player(e.player(r.white())?)) == player(pdb.player(p.white())?)),
            ("black", blank(player(e.player(r.black())?)) == player(pdb.player(p.black())?)),
            ("event", blank(text(&tournament, false)) == text(&ptournament, false)),
            ("site", blank(text(&tournament, true)) == text(&ptournament, true)),
            ("elo", (r.white_elo(), r.black_elo()) == p.elo()),
            ("result", r.result().pgn() == p.result().pgn()),
            ("eco", r.eco().pgn() == p.eco().pgn()),
            ("date", r.played_date().pgn() == p.played_date().pgn()),
            ("round", (r.round(), r.subround()) == p.round()),
            ("moves", r.move_count() == p.move_count()),
        ];
        let mut bytes = vec![0u8; (p.len() as usize).min(MAX_TEXT)];
        pdb.read_span(p.offset(), &mut bytes)?;
        let mut moves = Vec::new();
        main_line(&bytes, &mut lexer, &mut |_, mv| {
            moves.extend(mv);
            true
        });
        fields.push(("main line", replayed(&db, &r).is_some_and(|m| m == moves)));
        for (field, same) in fields {
            if same {
                *equal.entry(field).or_default() += 1;
            } else {
                let d = differ.entry(field).or_default();
                d.0 += 1;
                if d.1.len() < 5 {
                    d.1.push(n);
                }
            }
        }
    }
    let _ = std::fs::remove_file(&index);
    println!("games compared {n}");
    for (field, count) in &equal {
        let (d, first) = differ.get(field).cloned().unwrap_or_default();
        println!("{field:<10} equal {count:>10}  differ {d:>8}  first {first:?}");
    }
    for (field, (d, first)) in &differ {
        if !equal.contains_key(field) {
            println!("{field:<10} equal {:>10}  differ {d:>8}  first {first:?}", 0);
        }
    }
    Ok(())
}
