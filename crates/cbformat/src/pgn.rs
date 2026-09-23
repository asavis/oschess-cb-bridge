//! PGN output: standard algebraic notation and a game's full move tree.

use std::fmt::Write;

use chesscore::{Board, Color as CColor, Move, Piece, Square, attacks, squares};

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
/// `O-O-O` (chesscore encodes it as the king taking its own rook).
pub fn san(board: &Board, mv: Move) -> String {
    let mut s = String::with_capacity(8);
    write_san_body(&mut s, board, mv);
    let mut after = board.clone();
    after.play_unchecked(mv);
    write_check_suffix(&mut s, &after);
    s
}

/// Everything of a SAN but the check or mate suffix, which needs the position
/// after the move.
fn write_san_body(s: &mut String, board: &Board, mv: Move) {
    let us = board.side_to_move();
    let Some((piece, _)) = board.piece_at(mv.from) else { return };
    if piece == Piece::King && board.colors(us) & mv.to.bit() != 0 {
        s.push_str(if mv.to.file() > mv.from.file() { "O-O" } else { "O-O-O" });
        return;
    }
    let capture = board.colors(!us) & mv.to.bit() != 0 || (piece == Piece::Pawn && mv.from.file() != mv.to.file());
    if piece == Piece::Pawn {
        if capture {
            s.push(file_char(mv.from));
        }
    } else {
        s.push_str(piece_letter(piece));
        disambiguate(s, board, piece, mv);
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

/// Adds the origin file, rank or both when another piece of the same kind can
/// also move legally to the destination. The candidates are the pieces that
/// attack the destination, so the common unambiguous move costs one lookup and
/// no move generation.
fn disambiguate(s: &mut String, board: &Board, piece: Piece, mv: Move) {
    let occupied = board.occupied();
    let reach = match piece {
        Piece::Knight => attacks::knight(mv.to),
        Piece::Bishop => attacks::bishop(mv.to, occupied),
        Piece::Rook => attacks::rook(mv.to, occupied),
        Piece::Queen => attacks::queen(mv.to, occupied),
        Piece::King | Piece::Pawn => return,
    };
    let others = board.colored(piece, board.side_to_move()) & reach & !mv.from.bit();
    let (mut ambiguous, mut same_file, mut same_rank) = (false, false, false);
    for from in squares(others) {
        if board.is_legal(Move::new(from, mv.to, None)) {
            ambiguous = true;
            same_file |= from.file() == mv.from.file();
            same_rank |= from.rank() == mv.from.rank();
        }
    }
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

fn write_check_suffix(s: &mut String, after: &Board) {
    if after.in_check() {
        // In check with no legal move is mate.
        s.push(if after.has_legal_move() { '+' } else { '#' });
    }
}

fn file_char(sq: Square) -> char {
    (b'a' + sq.file()) as char
}

fn rank_char(sq: Square) -> char {
    (b'1' + sq.rank()) as char
}

const NONE: u32 = u32::MAX;

/// A move of the tree. Children form a linked list, and every SAN lives in one
/// shared buffer, so building the tree allocates nothing per move.
struct Node {
    san: (u32, u32),
    fullmove: u16,
    white: bool,
    first_child: u32,
    last_child: u32,
    next_sibling: u32,
}

impl Node {
    fn new(san: (u32, u32), fullmove: u16, white: bool) -> Node {
        Node { san, fullmove, white, first_child: NONE, last_child: NONE, next_sibling: NONE }
    }
}

/// The move tree of a game as PGN movetext, without the result.
pub fn movetext(db: &Database, record: &Record) -> Result<String> {
    movetext_of(&db.moves_of(record)?.moves()?)
}

/// The move tree of a parsed move record as PGN movetext.
pub fn movetext_of(moves: &GameMoves<'_>) -> Result<String> {
    let mut tree = TreeBuilder {
        nodes: vec![Node::new((0, 0), 0, true)],
        sans: String::new(),
        cur: 0,
        parent_of_last: 0,
        branches: Vec::new(),
    };
    // walk() checks every move and the tree's shape, so a damaged record is an
    // error here exactly as it is in `cbtool verify`.
    replay::walk(moves, &mut tree)?;
    let mut out = String::with_capacity(tree.sans.len() + 4 * tree.nodes.len());
    emit(&tree.nodes, &tree.sans, &mut out);
    let len = out.trim_end().len();
    out.truncate(len);
    Ok(out)
}

struct TreeBuilder {
    nodes: Vec<Node>,
    sans: String,
    cur: u32,
    parent_of_last: u32,
    branches: Vec<u32>,
}

impl TreeVisitor for TreeBuilder {
    fn play(&mut self, before: &Board, mv: Option<Move>, _main_line: bool) {
        let start = self.sans.len() as u32;
        match mv {
            Some(mv) => write_san_body(&mut self.sans, before, mv),
            None => self.sans.push_str("--"),
        }
        let white = before.side_to_move() == CColor::White;
        let id = self.nodes.len() as u32;
        self.nodes.push(Node::new((start, self.sans.len() as u32), before.fullmove_number(), white));
        let cur = self.cur as usize;
        match self.nodes[cur].last_child {
            NONE => self.nodes[cur].first_child = id,
            last => self.nodes[last as usize].next_sibling = id,
        }
        self.nodes[cur].last_child = id;
        self.parent_of_last = self.cur;
        self.cur = id;
    }
    fn played(&mut self, after: &Board) {
        // The suffix extends the SAN just written, which ends the buffer.
        write_check_suffix(&mut self.sans, after);
        self.nodes[self.cur as usize].san.1 = self.sans.len() as u32;
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
fn emit(nodes: &[Node], sans: &str, out: &mut String) {
    enum Step {
        /// Continue the line whose last written move is `node`.
        Line {
            node: u32,
            force_number: bool,
        },
        /// Write the alternative `alt` to the main move `main`, and those after it.
        Alternatives {
            main: u32,
            alt: u32,
        },
        Close,
    }
    let write = |out: &mut String, n: u32, force_number: bool| {
        let n = &nodes[n as usize];
        // Writing to a String cannot fail.
        let _ = if n.white {
            write!(out, "{}. ", n.fullmove)
        } else if force_number {
            write!(out, "{}... ", n.fullmove)
        } else {
            Ok(())
        };
        out.push_str(&sans[n.san.0 as usize..n.san.1 as usize]);
        out.push(' ');
    };
    let mut steps = vec![Step::Line { node: 0, force_number: true }];
    while let Some(step) = steps.pop() {
        match step {
            Step::Line { node, force_number } => {
                let main = nodes[node as usize].first_child;
                if main == NONE {
                    continue;
                }
                write(out, main, force_number);
                match nodes[main as usize].next_sibling {
                    NONE => steps.push(Step::Line { node: main, force_number: false }),
                    alt => steps.push(Step::Alternatives { main, alt }),
                }
            }
            Step::Alternatives { main, alt } => {
                if alt == NONE {
                    // Back on the main line after its alternatives: repeat the number.
                    steps.push(Step::Line { node: main, force_number: true });
                } else {
                    steps.push(Step::Alternatives { main, alt: nodes[alt as usize].next_sibling });
                    steps.push(Step::Close);
                    out.push('(');
                    write(out, alt, true);
                    steps.push(Step::Line { node: alt, force_number: false });
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

fn tag(out: &mut String, name: &str, value: &str) {
    out.push('[');
    out.push_str(name);
    out.push_str(" \"");
    for c in value.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push_str("\"]\n");
}

/// A game as a complete PGN record with the seven-tag roster, Elo tags and,
/// for games not from the standard position, `SetUp`/`FEN`.
pub fn game(db: &Database, id: u32) -> Result<String> {
    let r = db.record(id)?;
    if r.kind() != RecordKind::Game {
        return Err(Error::Format(format!("record {id} is not a game")));
    }
    let data = db.moves_of(&r)?;
    game_from(db, &r, &data.moves()?)
}

/// [`game`] for a record and move record already read, as from a
/// [`crate::v2::Batch`]; entities are read from `db`.
pub fn game_from(db: &Database, r: &Record, moves: &GameMoves<'_>) -> Result<String> {
    if r.kind() != RecordKind::Game {
        return Err(Error::Format(format!("record {} is not a game", r.id())));
    }
    let e = db.entities();
    let t = e.tournament(r.tournament())?;
    let name = |pid| -> Result<String> {
        Ok(e.player(pid)?.map(|p| p.pgn()).filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into()))
    };
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
    tag(&mut out, "White", &name(r.white())?);
    tag(&mut out, "Black", &name(r.black())?);
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
    let text = movetext_of(moves)?;
    if !text.is_empty() {
        out.push_str(&text);
        out.push(' ');
    }
    out.push_str(r.result().pgn());
    out.push('\n');
    Ok(out)
}
