//! Standard algebraic notation.

use chesscore::{Board, Move, Piece, Square, attacks, squares};

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
pub(super) fn write_san_body(s: &mut String, board: &Board, mv: Move) {
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

pub(super) fn write_check_suffix(s: &mut String, after: &Board) {
    if after.in_check() {
        // In check with no legal move is mate.
        s.push(if after.has_legal_move() { '+' } else { '#' });
    }
}

pub(super) fn file_char(sq: Square) -> char {
    (b'a' + sq.file()) as char
}

pub(super) fn rank_char(sq: Square) -> char {
    (b'1' + sq.rank()) as char
}
