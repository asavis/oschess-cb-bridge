//! PGN output: standard algebraic notation and a game's full move tree.

use cozy_chess::{Board, Color as CColor, GameStatus, Move, Piece, Square};

use crate::replay::{self, start_board};
use crate::v2::{Database, GameMoves, Record, RecordKind, Start, Token};
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
    let mut board = start_board(&moves.start()?)?;
    let mut nodes = vec![Node { san: String::new(), fullmove: 0, white: true, children: vec![] }];
    let mut cur = 0usize;
    let mut stack: Vec<(usize, Board)> = Vec::new();
    let mut before_last: Option<(usize, Board)> = None;
    for (ply, token) in moves.tokens().enumerate() {
        match token {
            Token::Move(w) => {
                let before = board.clone();
                let text =
                    match replay::play(&mut board, w).map_err(|reason| Error::Move { ply: ply as u32 + 1, reason })? {
                        Some(mv) => san(&before, mv),
                        None => "--".to_string(),
                    };
                nodes.push(Node {
                    san: text,
                    fullmove: before.fullmove_number(),
                    white: before.side_to_move() == CColor::White,
                    children: vec![],
                });
                let id = nodes.len() - 1;
                nodes[cur].children.push(id);
                before_last = Some((cur, before));
                cur = id;
            }
            Token::Alternative => {
                stack.push(before_last.clone().ok_or_else(|| Error::Format("alternative before any move".into()))?);
            }
            Token::EndOfLine => match stack.pop() {
                Some((node, b)) => {
                    cur = node;
                    board = b;
                }
                None => break,
            },
        }
    }
    let mut out = String::new();
    emit(&nodes, 0, true, &mut out);
    Ok(out.trim_end().to_string())
}

fn emit(nodes: &[Node], parent: usize, mut force_number: bool, out: &mut String) {
    let mut parent = parent;
    loop {
        let children = &nodes[parent].children;
        let Some(&main) = children.first() else { return };
        write_move(&nodes[main], force_number, out);
        force_number = false;
        for &alt in &children[1..] {
            out.push('(');
            write_move(&nodes[alt], true, out);
            emit(nodes, alt, false, out);
            if out.ends_with(' ') {
                out.pop();
            }
            out.push_str(") ");
            force_number = true;
        }
        parent = main;
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
    if let Some((eco, _)) = r.eco() {
        tag(&mut out, "ECO", &format!("{}{:02}", (b'A' + (eco / 100) as u8) as char, eco % 100));
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
