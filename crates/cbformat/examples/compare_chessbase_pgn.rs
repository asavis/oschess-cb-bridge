//! Compares our PGN of a database with ChessBase's own PGN export of the same
//! database, game by game in id order, and prints counts only: how many games
//! agree on their moves, symbols, comments and graphics, and the first ids
//! that do not, by category. No game content is printed unless `--show ID`
//! asks for that one game, for local debugging.
//!
//! `cargo run --release -p cbformat --example compare_chessbase_pgn -- <database> <export.pgn> [--lang de,en] [--show ID]`

use std::collections::BTreeMap;

use cbformat::pgn::{self, Options};
use cbformat::v2::{Database, RecordKind};

/// The parts of one game's movetext that are compared.
#[derive(Debug, Default, PartialEq, Eq)]
struct Parts {
    moves: Vec<String>,
    /// NAG numbers after each move, by move index.
    nags: Vec<(usize, u16)>,
    /// Comment text after each move index (0 = before the first move), with
    /// whitespace collapsed and graphics taken out.
    comments: Vec<(usize, String)>,
    /// `[%csl]` / `[%cal]` items by move index, each sorted.
    graphics: Vec<(usize, Vec<String>)>,
}

/// Splits a PGN file into games' movetexts: the text after each header block.
fn movetexts(pgn: &str) -> Vec<String> {
    let mut games = Vec::new();
    let mut cur = String::new();
    let mut in_moves = false;
    for line in pgn.lines() {
        let l = line.trim_end_matches('\r');
        if l.starts_with('[') && !in_moves {
            continue;
        }
        if l.starts_with("[Event ") && in_moves {
            games.push(std::mem::take(&mut cur));
            in_moves = false;
            continue;
        }
        if !l.trim().is_empty() {
            in_moves = true;
            cur.push_str(l);
            cur.push('\n');
        }
    }
    if in_moves {
        games.push(cur);
    }
    games
}

fn parts(movetext: &str) -> Parts {
    let mut p = Parts::default();
    let chars: Vec<char> = movetext.chars().collect();
    let mut i = 0;
    let mut comment_texts: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            let end = chars[i..].iter().position(|&x| x == '}').map_or(chars.len(), |e| i + e);
            let body: String = chars[i + 1..end].iter().collect();
            let (text, marks) = split_graphics(&body);
            if !text.is_empty() {
                comment_texts.entry(p.moves.len()).or_default().push(text);
            }
            if !marks.is_empty() {
                p.graphics.push((p.moves.len(), marks));
            }
            i = end + 1;
        } else if c.is_whitespace() || c == '(' || c == ')' {
            i += 1;
        } else {
            let end =
                chars[i..].iter().position(|x| x.is_whitespace() || "(){".contains(*x)).map_or(chars.len(), |e| i + e);
            let tok: String = chars[i..end].iter().collect();
            i = end;
            if let Some(n) = tok.strip_prefix('$') {
                if let Ok(n) = n.parse() {
                    p.nags.push((p.moves.len(), n));
                }
            } else if tok.trim_start_matches(|c: char| c.is_ascii_digit()).starts_with('.')
                || ["1-0", "0-1", "1/2-1/2", "*"].contains(&tok.as_str())
            {
                // A move number, or the result.
            } else {
                let (san, suffix) = split_suffix(&tok);
                p.moves.push(san.to_string());
                if let Some(n) = suffix {
                    p.nags.push((p.moves.len(), n));
                }
            }
        }
    }
    p.comments = comment_texts.into_iter().map(|(k, v)| (k, v.join(" "))).collect();
    p.nags.sort();
    p
}

/// `e4!?` as `e4` and NAG 5.
fn split_suffix(tok: &str) -> (&str, Option<u16>) {
    for (s, n) in [("!!", 3), ("??", 4), ("!?", 5), ("?!", 6), ("!", 1), ("?", 2)] {
        if let Some(san) = tok.strip_suffix(s) {
            return (san, Some(n));
        }
    }
    (tok, None)
}

fn split_graphics(body: &str) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut marks = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[%c") {
        text.push_str(&rest[..start]);
        let end = rest[start..].find(']').map_or(rest.len(), |e| start + e + 1);
        let cmd = &rest[start..end];
        if let Some(list) = cmd.strip_prefix("[%csl ").or_else(|| cmd.strip_prefix("[%cal ")) {
            marks.extend(list.trim_end_matches(']').split(',').map(|s| s.trim().to_string()));
        }
        rest = &rest[end..];
    }
    text.push_str(rest);
    marks.sort();
    (text.split_whitespace().collect::<Vec<_>>().join(" "), marks)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = Database::open(&args[1]).expect("open database");
    let theirs = std::fs::read(&args[2]).expect("read export");
    let theirs = String::from_utf8(theirs.clone()).unwrap_or_else(|_| theirs.iter().map(|&b| b as char).collect());
    let mut options = Options::default();
    let mut show = None;
    let mut it = args[3..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--lang" => options = Options::with_languages(it.next().expect("--lang CODES").split(',')),
            "--show" => show = it.next().and_then(|s| s.parse::<u32>().ok()),
            _ => panic!("unknown argument {a}"),
        }
    }
    let theirs = movetexts(&theirs);
    let ids: Vec<u32> = (1..=db.record_count())
        .filter(|&id| db.record(id).is_ok_and(|r| r.kind() == RecordKind::Game && !r.is_deleted()))
        .collect();
    println!("games in the database {}, in the export {}", ids.len(), theirs.len());
    let mut mismatches: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    let mut agree = 0;
    for (&id, their_text) in ids.iter().zip(&theirs) {
        let ours = match pgn::game_with(&db, id, &options) {
            Ok(r) => r.pgn.split_once("\n\n").map(|(_, m)| m.to_string()).unwrap_or_default(),
            Err(_) => {
                mismatches.entry("our error").or_default().push(id);
                continue;
            }
        };
        let (a, b) = (parts(&ours), parts(their_text));
        if show == Some(id) {
            println!("--- ours\n{ours}\n--- theirs\n{their_text}\n--- ours {a:?}\n--- theirs {b:?}");
        }
        let mut ok = true;
        for (name, same) in [
            ("moves", a.moves == b.moves),
            ("symbols", a.nags == b.nags),
            ("comments", a.comments == b.comments),
            ("graphics", a.graphics == b.graphics),
        ] {
            if !same {
                mismatches.entry(name).or_default().push(id);
                ok = false;
            }
        }
        agree += ok as u32;
    }
    println!("games agreeing on everything compared {agree}");
    for (name, v) in &mismatches {
        let first: Vec<_> = v.iter().take(10).collect();
        println!("differ in {name:<9} {:>7}   first ids {first:?}", v.len());
    }
}
