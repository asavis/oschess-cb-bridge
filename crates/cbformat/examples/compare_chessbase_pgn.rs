//! Compares our PGN of a database with ChessBase's own PGN export of the same
//! database, game by game in id order, and prints counts only: how many games
//! agree on their move trees, symbols, comments and graphics, and the first
//! ids that do not, by category. No game content is printed unless `--show ID`
//! asks for that one game, for local debugging.
//!
//! Both movetexts are read into move trees, so a variation that ends earlier or
//! later, or a comment inside instead of after a variation, is a difference;
//! how the two writers space tokens or repeat move numbers is not.
//!
//! `cargo run --release -p cbformat --example compare_chessbase_pgn -- <database> <export.pgn> [--lang de,en] [--show ID]`

use std::collections::BTreeMap;

use cbformat::pgn::{self, Options};
use cbformat::v2::{Database, RecordKind};

/// One move of a tree: its SAN without `!`/`?`, and what is attached to it.
#[derive(Debug, Default)]
struct Node {
    san: String,
    parent: usize,
    /// The moves played after this one: the first continues the line, the
    /// others are alternatives to the first.
    children: Vec<usize>,
    nags: Vec<u16>,
    comments: Vec<String>,
    marks: Vec<String>,
}

/// What is compared, each keyed by the move's index in PGN order (1 for the
/// first move, 0 for the start of the game).
#[derive(Debug, Default, PartialEq, Eq)]
struct Parts {
    /// Each move with the index of the move it follows.
    moves: Vec<(usize, String)>,
    nags: Vec<(usize, Vec<u16>)>,
    comments: Vec<(usize, String)>,
    graphics: Vec<(usize, Vec<String>)>,
}

/// A movetext as a tree. Node 0 is the start of the game; what stands before
/// the first move is attached to it.
fn tree(movetext: &str) -> Vec<Node> {
    let mut nodes = vec![Node::default()];
    // The move the next one follows; `)` returns to the node saved by `(`.
    let mut cur = 0;
    let mut stack: Vec<usize> = Vec::new();
    // Comments and graphics right after `(` belong to the variation's first
    // move, not to the move the variation branches from.
    let mut pending: Option<(Vec<String>, Vec<String>)> = None;
    for tok in tokens(movetext) {
        match tok {
            Tok::Open => {
                stack.push(cur);
                // An alternative to the move just played follows that move's parent.
                cur = nodes[cur].parent;
                pending = Some((Vec::new(), Vec::new()));
            }
            Tok::Close => {
                if let Some(back) = stack.pop() {
                    cur = back;
                }
                pending = None;
            }
            Tok::Comment(body) => {
                let (text, marks) = split_graphics(&body);
                let (comments, all_marks) = match &mut pending {
                    Some((c, m)) => (c, m),
                    None => {
                        let node = &mut nodes[cur];
                        (&mut node.comments, &mut node.marks)
                    }
                };
                comments.extend((!text.is_empty()).then_some(text));
                all_marks.extend(marks);
            }
            Tok::Nag(n) => nodes[cur].nags.push(n),
            Tok::Move(san, suffix) => {
                let id = nodes.len();
                let (comments, marks) = pending.take().unwrap_or_default();
                nodes.push(Node {
                    san,
                    parent: cur,
                    nags: suffix.into_iter().collect(),
                    comments,
                    marks,
                    ..Node::default()
                });
                nodes[cur].children.push(id);
                cur = id;
            }
        }
    }
    nodes
}

/// The nodes in PGN order: a move, then its alternatives each with its whole
/// line, then the move's own continuation.
fn pgn_order(nodes: &[Node]) -> Vec<usize> {
    enum Task {
        /// Write the line that continues after this node.
        Line(usize),
        /// Write this alternative move and the line after it.
        Alternative(usize),
    }
    let mut order = vec![0];
    let mut tasks = vec![Task::Line(0)];
    while let Some(task) = tasks.pop() {
        let n = match task {
            Task::Alternative(n) => {
                order.push(n);
                tasks.push(Task::Line(n));
                continue;
            }
            Task::Line(n) => n,
        };
        let Some((&main, alternatives)) = nodes[n].children.split_first() else { continue };
        order.push(main);
        tasks.push(Task::Line(main));
        tasks.extend(alternatives.iter().rev().map(|&a| Task::Alternative(a)));
    }
    order
}

fn parts(movetext: &str) -> Parts {
    let nodes = tree(movetext);
    let order = pgn_order(&nodes);
    let mut index = vec![0usize; nodes.len()];
    for (i, &n) in order.iter().enumerate() {
        index[n] = i;
    }
    let mut p = Parts::default();
    for (i, &n) in order.iter().enumerate() {
        let node = &nodes[n];
        if n != 0 {
            p.moves.push((index[node.parent], node.san.clone()));
        }
        if !node.nags.is_empty() {
            let mut nags = node.nags.clone();
            nags.sort_unstable();
            p.nags.push((i, nags));
        }
        if !node.comments.is_empty() {
            p.comments.push((i, node.comments.join(" ")));
        }
        if !node.marks.is_empty() {
            let mut marks = node.marks.clone();
            marks.sort();
            p.graphics.push((i, marks));
        }
    }
    p
}

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    Comment(String),
    Nag(u16),
    /// A move's SAN and the NAG its `!`/`?` suffix stands for.
    Move(String, Option<u16>),
}

fn tokens(movetext: &str) -> Vec<Tok> {
    let chars: Vec<char> = movetext.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' => {
                let end = chars[i..].iter().position(|&x| x == '}').map_or(chars.len(), |e| i + e);
                out.push(Tok::Comment(chars[i + 1..end].iter().collect()));
                i = end + 1;
            }
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            '$' => {
                // A NAG ends at its last digit, so `$1$14` and `e4$1` hold two
                // tokens. One that is not a number stays in the comparison as
                // `u16::MAX` instead of vanishing.
                let digits = chars[i + 1..].iter().take_while(|x| x.is_ascii_digit()).count();
                let n: String = chars[i + 1..i + 1 + digits].iter().collect();
                out.push(Tok::Nag(n.parse().unwrap_or(u16::MAX)));
                i += 1 + digits;
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                let end = chars[i..]
                    .iter()
                    .position(|x| x.is_whitespace() || "(){$".contains(*x))
                    .map_or(chars.len(), |e| i + e);
                let word: String = chars[i..end].iter().collect();
                i = end;
                push_word(&word, &mut out);
            }
        }
    }
    out
}

/// One word between spaces: a result, a move number, or a move with or
/// without a move number glued in front (`1.e4`, `1...e5`).
fn push_word(word: &str, out: &mut Vec<Tok>) {
    if ["1-0", "0-1", "1/2-1/2", "*"].contains(&word) {
        return;
    }
    let digits = word.len() - word.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let rest = match word[digits..].strip_prefix('.') {
        Some(after) if digits > 0 => after.trim_start_matches('.'),
        _ => word,
    };
    if !rest.is_empty() {
        let (san, suffix) = split_suffix(rest);
        out.push(Tok::Move(san.to_string(), suffix));
    }
}

/// `e4!?` as `e4` and NAG 5.
fn split_suffix(word: &str) -> (&str, Option<u16>) {
    for (s, n) in [("!!", 3), ("??", 4), ("!?", 5), ("?!", 6), ("!", 1), ("?", 2)] {
        if let Some(san) = word.strip_suffix(s) {
            return (san, Some(n));
        }
    }
    (word, None)
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
    (text.split_whitespace().collect::<Vec<_>>().join(" "), marks)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn moves(p: &Parts) -> Vec<(usize, &str)> {
        p.moves.iter().map(|(i, s)| (*i, s.as_str())).collect()
    }

    #[test]
    fn where_a_variation_ends_matters() {
        let a = parts("1. e4 e5 (1... c5 2. Nf3 Nc6) 2. Nc3");
        let b = parts("1. e4 e5 (1... c5 2. Nf3 Nc6 3. Nc3)");
        assert_ne!(a, b);
        // PGN order: e4 1, e5 2, c5 3, Nf3 4, Nc6 5, Nc3 6.
        assert_eq!(moves(&a), [(0, "e4"), (1, "e5"), (1, "c5"), (3, "Nf3"), (4, "Nc6"), (2, "Nc3")]);
        assert_eq!(moves(&b), [(0, "e4"), (1, "e5"), (1, "c5"), (3, "Nf3"), (4, "Nc6"), (5, "Nc3")]);
    }

    #[test]
    fn the_order_is_the_annotation_order() {
        // Morphy's example: e4 0, c5 1, c6 2, d4 3, Nf3 4, here one higher.
        let p = parts("1. e4 c5 (1... c6 2. d4) 2. Nf3");
        assert_eq!(moves(&p), [(0, "e4"), (1, "c5"), (1, "c6"), (3, "d4"), (2, "Nf3")]);
    }

    #[test]
    fn a_comment_inside_or_after_a_variation() {
        let inside = parts("1. e4 (1. d4 {note}) e5");
        let after = parts("1. e4 (1. d4) {note} e5");
        assert_ne!(inside, after);
        assert_eq!(inside.comments, [(2, "note".to_string())]);
        // After the variation closes, the comment is on the move it replaced.
        assert_eq!(after.comments, [(1, "note".to_string())]);
        assert_eq!(after, parts("1. e4 {note} (1. d4) 1... e5"));
    }

    #[test]
    fn a_comment_opening_a_variation_is_on_its_first_move() {
        let p = parts("1. e4 ({a} 1. d4 {b}) 1... e5");
        assert_eq!(p.comments, [(2, "a b".to_string())]);
    }

    #[test]
    fn compact_move_numbers() {
        let want = parts("1. e4 e5 2. Nf3 *");
        assert_eq!(moves(&want), [(0, "e4"), (1, "e5"), (2, "Nf3")]);
        assert_eq!(parts("1.e4 e5 2.Nf3 *"), want);
        assert_eq!(parts("1.e4 1...e5 2.Nf3 *"), want);
        assert_eq!(parts("1. e4 1... e5 2. Nf3 1-0"), want);
        assert_eq!(moves(&parts("12.Nf3 12...Nc6")), [(0, "Nf3"), (1, "Nc6")]);
    }

    #[test]
    fn suffixes_nags_and_graphics() {
        let p = parts("1. e4! $14 {[%cal Gg1f3][%csl Re5] text} e5?!");
        assert_eq!(p.nags, [(1, vec![1, 14]), (2, vec![6])]);
        assert_eq!(p.comments, [(1, "text".to_string())]);
        assert_eq!(p.graphics, [(1, vec!["Gg1f3".to_string(), "Re5".to_string()])]);
        assert_eq!(p, parts("1. e4 $1 $14 {text [%csl Re5] [%cal Gg1f3]} 1... e5 $6"));
    }

    #[test]
    fn adjacent_nags_are_kept() {
        let spaced = parts("1. e4 $1 $14 1-0");
        assert_eq!(spaced.nags, [(1, vec![1, 14])]);
        assert_eq!(parts("1. e4 $1$14 1-0"), spaced);
        assert_eq!(parts("1. e4$1$14 1-0"), spaced);
        assert_ne!(parts("1. e4 1-0"), spaced);
    }

    #[test]
    fn a_malformed_nag_is_not_dropped() {
        assert_eq!(parts("1. e4 $ 1-0").nags, [(1, vec![u16::MAX])]);
        assert_eq!(parts("1. e4 $99999 1-0").nags, [(1, vec![u16::MAX])]);
    }

    #[test]
    fn a_comment_before_the_first_move() {
        assert_eq!(parts("{game} 1. e4").comments, [(0, "game".to_string())]);
    }
}
