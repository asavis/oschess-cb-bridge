//! Compares our PGN of a database with ChessBase's own PGN export of the same
//! database, game by game in id order, and prints counts only: how many games
//! agree on their move trees, symbols, comments and graphics, how many differ
//! in a known way, by kind, and the first ids of any other difference. No game
//! content is printed unless `--show ID` asks for that one game, for local
//! debugging.
//!
//! Both movetexts are read into move trees, so a variation that ends earlier or
//! later, or a comment inside instead of after a variation, is a difference;
//! how the two writers space tokens or repeat move numbers is not. The known
//! differences:
//!
//! - **Disambiguation.** ChessBase names the origin file or rank of a move
//!   another piece of its kind could reach only illegally, being pinned; the
//!   PGN standard, and our SAN, count legal moves only.
//! - **Languages.** ChessBase writes a comment in every language it has; the
//!   reading form writes one. The comments must equal our full form's texts,
//!   every language joined, with the game quotations as ChessBase writes them.
//! - **Evaluations** (type `26`), which ChessBase writes as `[%evp …]`.
//!
//! ChessBase starts its export with a UTF-8 byte-order mark, which is skipped.
//!
//! `cargo run --release -p cbformat --example compare_chessbase_pgn -- <database> <export.pgn> [--lang de,en] [--show ID]`

use std::collections::BTreeMap;

use cbformat::pgn::{self, Options};
use cbformat::v2::{Database, Quotation, RecordKind};
use chesscore::{Board, Piece, attacks, squares};

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
    /// The comments as written, commands included.
    raw: Vec<String>,
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
    let mut pending: Option<(Vec<String>, Vec<String>, Vec<String>)> = None;
    for tok in tokens(movetext) {
        match tok {
            Tok::Open => {
                stack.push(cur);
                // An alternative to the move just played follows that move's parent.
                cur = nodes[cur].parent;
                pending = Some((Vec::new(), Vec::new(), Vec::new()));
            }
            Tok::Close => {
                if let Some(back) = stack.pop() {
                    cur = back;
                }
                pending = None;
            }
            Tok::Comment(body) => {
                let (text, marks) = split_graphics(&body);
                let (comments, all_marks, raw) = match &mut pending {
                    Some((c, m, r)) => (c, m, r),
                    None => {
                        let node = &mut nodes[cur];
                        (&mut node.comments, &mut node.marks, &mut node.raw)
                    }
                };
                comments.extend((!text.is_empty()).then_some(text));
                all_marks.extend(marks);
                raw.push(body);
            }
            Tok::Nag(n) => nodes[cur].nags.push(n),
            Tok::Move(san, suffix) => {
                let id = nodes.len();
                let (comments, marks, raw) = pending.take().unwrap_or_default();
                nodes.push(Node {
                    san,
                    parent: cur,
                    nags: suffix.into_iter().collect(),
                    comments,
                    marks,
                    raw,
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
    let pgn = pgn.strip_prefix('\u{feff}').unwrap_or(pgn);
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

/// Our full form's texts per node, every language joined in stored order,
/// with each game quotation as ChessBase writes it: what ChessBase's export
/// holds.
fn all_languages(full: &str) -> BTreeMap<usize, String> {
    texts_of(full, |_| true)
}

/// The language of the reading form: the first of `preferred` the game has,
/// else English, else the first stored.
fn reading_language(full: &str, preferred: &[String]) -> String {
    let codes: Vec<String> = full
        .split("{[%lang ")
        .skip(1)
        .filter_map(|x| x.split(']').next())
        .filter(|c| *c != "any")
        .map(str::to_string)
        .collect();
    preferred
        .iter()
        .chain(std::iter::once(&"en".to_string()))
        .find(|p| codes.contains(p))
        .cloned()
        .or(codes.first().cloned())
        .unwrap_or_default()
}

/// Our full form's texts per node, as [`all_languages`], keeping only the
/// texts whose `[%lang]` code `keep` accepts.
fn texts_of(full: &str, keep: impl Fn(&str) -> bool) -> BTreeMap<usize, String> {
    let nodes = tree(full);
    let order = pgn_order(&nodes);
    let mut out = BTreeMap::new();
    for (i, &n) in order.iter().enumerate() {
        let mut texts = Vec::new();
        // ChessBase writes medals first.
        for body in &nodes[n].raw {
            for m in body.split("[%mdl ").skip(1) {
                texts.push(format!("[%mdl {}]", m.split(']').next().unwrap_or("")));
            }
        }
        for body in &nodes[n].raw {
            if let Some(rest) = body.strip_prefix("[%lang ") {
                if let Some((code, text)) = rest.split_once("] ")
                    && keep(code)
                {
                    texts.push(text.to_string());
                }
                continue;
            }
            for cmd in body.split("[%cbquote ").skip(1) {
                let data = cmd.split(']').next().unwrap_or("").split(';').find_map(|f| f.strip_prefix("data="));
                let q = data.and_then(unbase64url).and_then(|d| Quotation::parse_2cbh(&d));
                texts.extend(q.map(|q| q.chessbase_text()));
            }
        }
        let joined = texts.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        if !joined.is_empty() {
            out.insert(i, joined);
        }
    }
    out
}

fn unbase64url(s: &str) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    for chunk in s.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= u32::from(value(c)?) << (18 - 6 * i);
        }
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

/// A comment of ChessBase's without its `[%evp …]` evaluations.
fn without_evaluations(t: &str) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(at) = rest.find("[%evp") {
        out.push_str(&rest[..at]);
        rest = rest[at..].split_once(']').map_or("", |x| x.1);
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether every move that differs is ChessBase naming the origin of a move
/// another piece of its kind reaches only illegally.
fn pinned_disambiguation(fen: Option<&str>, ours: &str, a: &Parts, b: &Parts) -> bool {
    if a.moves.len() != b.moves.len() {
        return false;
    }
    let nodes = tree(ours);
    let order = pgn_order(&nodes);
    let Some(start) = (match fen {
        Some(f) => Board::from_fen(f).ok(),
        None => Some(Board::startpos()),
    }) else {
        return false;
    };
    let mut boards: Vec<Option<Board>> = vec![None; nodes.len()];
    boards[0] = Some(start);
    let mut played = vec![None; nodes.len()];
    for n in 1..nodes.len() {
        let Some(before) = boards[nodes[n].parent].clone() else { return false };
        let Some(mv) = before.legal_moves().into_iter().find(|&m| pgn::san(&before, m) == nodes[n].san) else {
            return false;
        };
        let mut after = before.clone();
        after.play_unchecked(mv);
        boards[n] = Some(after);
        played[n] = Some(mv);
    }
    for (k, (x, y)) in a.moves.iter().zip(&b.moves).enumerate() {
        if x == y {
            continue;
        }
        let (xs, ys) = (x.1.as_bytes(), y.1.as_bytes());
        let extra = x.0 == y.0 && ys.len() == xs.len() + 1 && xs[0] == ys[0] && xs[1..] == ys[2..];
        let n = order[k + 1];
        let (Some(before), Some(mv)) = (boards[nodes[n].parent].clone(), played[n]) else { return false };
        let Some((piece, _)) = before.piece_at(mv.from) else { return false };
        let reach = match piece {
            Piece::Knight => attacks::knight(mv.to),
            Piece::Bishop => attacks::bishop(mv.to, before.occupied()),
            Piece::Rook => attacks::rook(mv.to, before.occupied()),
            Piece::Queen => attacks::queen(mv.to, before.occupied()),
            _ => return false,
        };
        let reaching = squares(before.colored(piece, before.side_to_move()) & reach).count();
        let legal = before
            .legal_moves()
            .into_iter()
            .filter(|m| m.to == mv.to && before.piece_at(m.from).map(|p| p.0) == Some(piece))
            .count();
        if !(extra && reaching >= 2 && legal == 1) {
            return false;
        }
    }
    true
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = Database::open(&args[1]).expect("open database");
    let theirs = std::fs::read(&args[2]).expect("read export");
    let theirs = String::from_utf8(theirs.clone()).unwrap_or_else(|_| theirs.iter().map(|&b| b as char).collect());
    let mut options = Options::default();
    let mut preferred: Vec<String> = Vec::new();
    let mut show = None;
    let mut it = args[3..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--lang" => {
                let codes = it.next().expect("--lang CODES");
                preferred = codes.split(',').map(str::to_string).collect();
                options = Options::with_languages(codes.split(','));
            }
            "--show" => show = it.next().and_then(|s| s.parse::<u32>().ok()),
            _ => panic!("unknown argument {a}"),
        }
    }
    let theirs = movetexts(&theirs);
    let ids: Vec<u32> = (1..=db.record_count())
        .filter(|&id| db.record(id).is_ok_and(|r| r.kind() == RecordKind::Game && !r.is_deleted()))
        .collect();
    println!("games in the database {}, in the export {}", ids.len(), theirs.len());
    let mut known: BTreeMap<&str, u32> = BTreeMap::new();
    let mut unexplained: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    let mut agree = 0;
    let (mut stripped_equal, mut stripped_differ) = (0u32, Vec::new());
    for (&id, their_text) in ids.iter().zip(&theirs) {
        let render = |full: bool| {
            let o = Options { full, ..options.clone() };
            pgn::game_with(&db, id, &o).map(|r| {
                let (head, moves) = r.pgn.split_once("\n\n").unwrap_or_default();
                (head.to_string(), moves.to_string())
            })
        };
        let (Ok((head, ours)), Ok((_, full))) = (render(false), render(true)) else {
            unexplained.entry("our error").or_default().push(id);
            continue;
        };
        let (a, b) = (parts(&ours), parts(their_text));
        // The full form kept to the reading form's language, without its
        // `[%cb…]` commands, gives the reading form.
        {
            let f = parts(&full);
            let chosen = reading_language(&full, &preferred);
            let kept = texts_of(&full, |code| code == chosen || code == "any");
            let reading: BTreeMap<usize, String> = a.comments.iter().cloned().collect();
            if f.moves == a.moves && f.nags == a.nags && f.graphics == a.graphics && kept == reading {
                stripped_equal += 1;
            } else {
                stripped_differ.push(id);
            }
        }
        if show == Some(id) {
            println!("--- ours\n{ours}\n--- full\n{full}\n--- theirs\n{their_text}");
        }
        let mut same = true;
        if a.moves != b.moves {
            same = false;
            let fen = head.lines().find_map(|l| l.strip_prefix("[FEN \"").and_then(|x| x.strip_suffix("\"]")));
            if pinned_disambiguation(fen, &ours, &a, &b) {
                *known.entry("moves: pinned-piece disambiguation").or_default() += 1;
            } else {
                unexplained.entry("moves").or_default().push(id);
            }
        }
        for (name, equal) in [("symbols", a.nags == b.nags), ("graphics", a.graphics == b.graphics)] {
            if !equal {
                same = false;
                unexplained.entry(name).or_default().push(id);
            }
        }
        if a.comments != b.comments {
            same = false;
            let all = all_languages(&full);
            let theirs_c: BTreeMap<usize, String> = b.comments.iter().cloned().collect();
            let keys: std::collections::BTreeSet<usize> = all.keys().chain(theirs_c.keys()).copied().collect();
            let mut kind = Some("comments: ChessBase's other languages");
            for k in keys {
                let (o, t) = (all.get(&k).cloned().unwrap_or_default(), theirs_c.get(&k).cloned().unwrap_or_default());
                if o == t {
                    continue;
                }
                if t.contains("[%evp") && without_evaluations(&t) == o {
                    kind = kind.map(|_| "comments: evaluations as [%evp] (part 2)");
                    continue;
                }
                kind = None;
                if std::env::var_os("CB_DIAG").is_some() {
                    let mut ws_o: Vec<&str> = o.split_whitespace().collect();
                    let mut ws_t: Vec<&str> = t.split_whitespace().collect();
                    let same_words = {
                        ws_o.sort();
                        ws_t.sort();
                        ws_o == ws_t
                    };
                    let shape = |x: &str| {
                        let mut sh = String::new();
                        for ch in x.chars() {
                            let c = if ch.is_alphabetic() {
                                'a'
                            } else if ch.is_ascii_digit() {
                                '9'
                            } else {
                                ch
                            };
                            if !((c == 'a' || c == '9') && sh.ends_with(c)) {
                                sh.push(c);
                            }
                        }
                        sh
                    };
                    if let Some(at) = t.find("[%") {
                        let name: String = t[at + 2..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                        let num: String =
                            t[at + 2 + name.len()..].trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
                        eprintln!("CMD {name} value-digits {}", num.len());
                    }
                    let extra = if t.starts_with(o.as_str()) {
                        format!("theirs = ours + [{}]", shape(&t[o.len()..]))
                    } else if t.ends_with(o.as_str()) {
                        format!("theirs = [{}] + ours", shape(&t[..t.len() - o.len()]))
                    } else if o.is_empty() {
                        format!("ours empty, theirs [{}]", shape(&t).chars().take(40).collect::<String>())
                    } else {
                        format!("same words {same_words}, lens {} {}", o.len(), t.len())
                    };
                    eprintln!("DIAG {extra}");
                }
            }
            match kind {
                Some(k) => *known.entry(k).or_default() += 1,
                None => unexplained.entry("comments").or_default().push(id),
            }
        }
        agree += same as u32;
    }
    println!(
        "full form kept to one language equals the reading form: {stripped_equal} of {}, first others {:?}",
        stripped_equal as usize + stripped_differ.len(),
        &stripped_differ[..stripped_differ.len().min(10)]
    );
    println!("games agreeing on everything compared {agree}");
    for (name, n) in &known {
        println!("known     {name:<45} {n:>7}");
    }
    let total: usize = unexplained.values().map(Vec::len).sum();
    println!("unexplained {total}");
    for (name, v) in &unexplained {
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
