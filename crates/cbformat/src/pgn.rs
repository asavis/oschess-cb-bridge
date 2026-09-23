//! PGN output: standard algebraic notation and a game's full move tree.

use cozy_chess::{Board, Color as CColor, GameStatus, Move, Piece, Square};

use crate::replay::{self, TreeVisitor, start_board};
use crate::v2::{Database, GameMoves, Record, RecordKind, Start};
use crate::{Error, Result};

fn piece_letter(p: Piece) -> &'static str {
    match p {
        Piece::King => "K",
        Piece::Queen => "Q",
        Piece::Rook => "R",
        Piece::Bishop => "B",
        Piece::Knight => "N",
        Piece::Pawn => "",
    }
}

/// The SAN of a legal move `mv` in `board`. Castling is written `O-O` /
/// `O-O-O` (cozy-chess encodes it as the king taking its own rook).
pub fn san(board: &Board, mv: Move) -> String {
    let us = board.side_to_move();
    let piece = board.piece_on(mv.from).expect("legal move has a piece on its origin");
    let mut s = String::new();
    if piece == Piece::King && board.color_on(mv.to) == Some(us) {
        s.push_str(if mv.to.file() > mv.from.file() { "O-O" } else { "O-O-O" });
    } else {
        let capture = board.color_on(mv.to) == Some(!us) || (piece == Piece::Pawn && mv.from.file() != mv.to.file());
        if piece == Piece::Pawn {
            if capture {
                s.push(file_char(mv.from));
            }
        } else {
            s.push_str(piece_letter(piece));
            // Other pieces of the same kind that can also reach the destination.
            let mut same_file = false;
            let mut same_rank = false;
            let mut ambiguous = false;
            board.generate_moves_for(board.colored_pieces(us, piece), |pm| {
                for m in pm {
                    if m.to == mv.to && m.from != mv.from {
                        ambiguous = true;
                        same_file |= m.from.file() == mv.from.file();
                        same_rank |= m.from.rank() == mv.from.rank();
                    }
                }
                false
            });
            if ambiguous {
                if !same_file {
                    s.push(file_char(mv.from));
                } else if !same_rank {
                    s.push(rank_char(mv.from));
                } else {
                    s.push(file_char(mv.from));
                    s.push(rank_char(mv.from));
                }
            }
        }
        if capture {
            s.push('x');
        }
        s.push(file_char(mv.to));
        s.push(rank_char(mv.to));
        if let Some(p) = mv.promotion {
            s.push('=');
            s.push_str(piece_letter(p));
        }
    }
    let mut after = board.clone();
    after.play_unchecked(mv);
    if !after.checkers().is_empty() {
        s.push(if after.status() == GameStatus::Won { '#' } else { '+' });
    }
    s
}

fn file_char(sq: Square) -> char {
    (b'a' + sq.file() as u8) as char
}

fn rank_char(sq: Square) -> char {
    (b'1' + sq.rank() as u8) as char
}

struct Node {
    san: String,
    fullmove: u16,
    white: bool,
    children: Vec<usize>,
}

/// The move tree of a game as PGN movetext, without the result.
pub fn movetext(db: &Database, record: &Record<'_>) -> Result<String> {
    movetext_of(&db.moves_of(record)?)
}

/// The move tree of a parsed move record as PGN movetext.
pub fn movetext_of(moves: &GameMoves<'_>) -> Result<String> {
    let mut tree = TreeBuilder {
        nodes: vec![Node { san: String::new(), fullmove: 0, white: true, children: vec![] }],
        cur: 0,
        parent_of_last: 0,
        branches: Vec::new(),
    };
    // walk() checks every move and the tree's shape, so a damaged record is an
    // error here exactly as it is in `cbtool verify`.
    replay::walk(moves, &mut tree)?;
    let mut out = String::new();
    emit(&tree.nodes, &mut out);
    Ok(out.trim_end().to_string())
}

struct TreeBuilder {
    nodes: Vec<Node>,
    cur: usize,
    parent_of_last: usize,
    branches: Vec<usize>,
}

impl TreeVisitor for TreeBuilder {
    fn play(&mut self, before: &Board, mv: Option<Move>, _main_line: bool) {
        let san = match mv {
            Some(mv) => san(before, mv),
            None => "--".to_string(),
        };
        let white = before.side_to_move() == CColor::White;
        self.nodes.push(Node { san, fullmove: before.fullmove_number(), white, children: vec![] });
        let id = self.nodes.len() - 1;
        self.nodes[self.cur].children.push(id);
        self.parent_of_last = self.cur;
        self.cur = id;
    }
    fn branch(&mut self) {
        self.branches.push(self.parent_of_last);
    }
    fn resume(&mut self) {
        // walk() calls resume only with a branch outstanding.
        self.cur = self.branches.pop().unwrap_or(0);
    }
}

/// Writes the tree below the root as movetext. Iterative, so that however
/// deeply the variations nest, the depth costs heap and not stack.
fn emit(nodes: &[Node], out: &mut String) {
    enum Step {
        /// Continue the line whose last written move is `node`.
        Line {
            node: usize,
            force_number: bool,
        },
        /// Write the alternatives of `branch` from its `next`-th child.
        Alternatives {
            branch: usize,
            next: usize,
        },
        Close,
    }
    let mut steps = vec![Step::Line { node: 0, force_number: true }];
    while let Some(step) = steps.pop() {
        match step {
            Step::Line { node, force_number } => {
                let children = &nodes[node].children;
                let Some(&main) = children.first() else { continue };
                write_move(&nodes[main], force_number, out);
                if children.len() > 1 {
                    steps.push(Step::Alternatives { branch: node, next: 1 });
                } else {
                    steps.push(Step::Line { node: main, force_number: false });
                }
            }
            Step::Alternatives { branch, next } => {
                let children = &nodes[branch].children;
                if let Some(&alt) = children.get(next) {
                    steps.push(Step::Alternatives { branch, next: next + 1 });
                    steps.push(Step::Close);
                    out.push('(');
                    write_move(&nodes[alt], true, out);
                    steps.push(Step::Line { node: alt, force_number: false });
                } else {
                    // Back on the main line after its alternatives: repeat the number.
                    steps.push(Step::Line { node: children[0], force_number: true });
                }
            }
            Step::Close => {
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push_str(") ");
            }
        }
    }
}

fn write_move(n: &Node, force_number: bool, out: &mut String) {
    if n.white {
        out.push_str(&format!("{}. ", n.fullmove));
    } else if force_number {
        out.push_str(&format!("{}... ", n.fullmove));
    }
    out.push_str(&n.san);
    out.push(' ');
}

fn tag(out: &mut String, name: &str, value: &str) {
    let v = value.replace('\\', "\\\\").replace('"', "\\\"");
    out.push_str(&format!("[{name} \"{v}\"]\n"));
}

/// A game as a complete PGN record with the seven-tag roster, Elo tags and,
/// for games not from the standard position, `SetUp`/`FEN`.
pub fn game(db: &Database, id: u32) -> Result<String> {
    let r = db.record(id)?;
    if r.kind() != RecordKind::Game {
        return Err(Error::Format(format!("record {id} is not a game")));
    }
    let e = db.entities();
    let t = e.tournament(r.tournament());
    let name = |pid| e.player(pid).map(|p| p.pgn()).filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into());
    let mut out = String::new();
    tag(&mut out, "Event", t.as_ref().map(|t| t.title.as_str()).filter(|s| !s.is_empty()).unwrap_or("?"));
    tag(&mut out, "Site", t.as_ref().map(|t| t.place.as_str()).filter(|s| !s.is_empty()).unwrap_or("?"));
    tag(&mut out, "Date", &r.played_date().pgn());
    let round = match (r.round(), r.subround()) {
        (0, _) => "?".to_string(),
        (n, 0) => n.to_string(),
        (n, s) => format!("{n}({s})"),
    };
    tag(&mut out, "Round", &round);
    tag(&mut out, "White", &name(r.white()));
    tag(&mut out, "Black", &name(r.black()));
    tag(&mut out, "Result", r.result().pgn());
    if r.white_elo() > 0 {
        tag(&mut out, "WhiteElo", &r.white_elo().to_string());
    }
    if r.black_elo() > 0 {
        tag(&mut out, "BlackElo", &r.black_elo().to_string());
    }
    if let Some(eco) = r.eco().pgn() {
        tag(&mut out, "ECO", &eco);
    }
    let moves = db.moves_of(&r)?;
    let start = moves.start()?;
    if start != Start::Standard {
        let board = start_board(&start)?;
        if moves.is_chess960() {
            tag(&mut out, "Variant", "Chess960");
        }
        tag(&mut out, "SetUp", "1");
        tag(&mut out, "FEN", &format!("{board}"));
    }
    out.push('\n');
    let text = movetext(db, &r)?;
    if !text.is_empty() {
        out.push_str(&text);
        out.push(' ');
    }
    out.push_str(r.result().pgn());
    out.push('\n');
    Ok(out)
}
