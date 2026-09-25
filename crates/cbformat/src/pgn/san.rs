//! Standard algebraic notation: written, and read as PGN files in the wild
//! write it.

use chesscore::{Board, CastleSide, Move, Piece, Square, attacks, squares};

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

/// The legal move `text` names in `board`, read leniently: `0-0` for `O-O`,
/// `e8Q` or `e8=Q`, long algebraic `Ng1-f3`, a missing or extra `+` or `#`,
/// and `!`/`?` suffixes. `None` for a null move, text that names no legal
/// move, or one that names more than one.
pub fn parse(board: &Board, text: &[u8]) -> Option<Move> {
    let end = text.iter().position(|b| matches!(b, b'+' | b'#' | b'!' | b'?')).unwrap_or(text.len());
    let text = &text[..end];
    match text {
        b"O-O" | b"0-0" | b"o-o" => return castle(board, CastleSide::Short),
        b"O-O-O" | b"0-0-0" | b"o-o-o" => return castle(board, CastleSide::Long),
        _ => {}
    }
    let (piece, rest) = match text.first()? {
        b'K' => (Piece::King, &text[1..]),
        b'Q' => (Piece::Queen, &text[1..]),
        b'R' => (Piece::Rook, &text[1..]),
        b'B' => (Piece::Bishop, &text[1..]),
        b'N' => (Piece::Knight, &text[1..]),
        b'P' => (Piece::Pawn, &text[1..]),
        _ => (Piece::Pawn, text),
    };
    // The promotion, after the destination: `=Q`, `Q` or `q`.
    let (rest, promotion) = match rest {
        [head @ .., b'=', p] | [head @ .., p] if piece == Piece::Pawn && promotion_piece(*p).is_some() => {
            (head, promotion_piece(*p))
        }
        _ => (rest, None),
    };
    // The destination is the last square named; files and ranks before it
    // narrow the origin. Captures and long algebraic's dash are passed over.
    let squares: Vec<u8> = rest.iter().copied().filter(|b| !matches!(b, b'x' | b'X' | b'-' | b':')).collect();
    let [origin @ .., f, r] = squares.as_slice() else { return None };
    let to = square(*f, *r)?;
    let (mut file, mut rank) = (None, None);
    for &b in origin {
        match b {
            b'a'..=b'h' if file.is_none() => file = Some(b - b'a'),
            b'1'..=b'8' if rank.is_none() => rank = Some(b - b'1'),
            _ => return None,
        }
    }
    let us = board.side_to_move();
    let mut found = None;
    let mut many = false;
    board.any_legal_move(|mv| {
        let fits = mv.to == to
            && board.piece_at(mv.from) == Some((piece, us))
            && file.is_none_or(|f| mv.from.file() == f)
            && rank.is_none_or(|r| mv.from.rank() == r)
            && mv.promotion
                == promotion.or((piece == Piece::Pawn && (to.rank() == 0 || to.rank() == 7)).then_some(Piece::Queen))
            && !is_castling(board, mv);
        if fits {
            many = found.is_some();
            found = Some(mv);
        }
        many
    });
    if many { None } else { found }
}

fn promotion_piece(b: u8) -> Option<Piece> {
    match b.to_ascii_uppercase() {
        b'Q' => Some(Piece::Queen),
        b'R' => Some(Piece::Rook),
        b'B' => Some(Piece::Bishop),
        b'N' => Some(Piece::Knight),
        _ => None,
    }
}

fn square(file: u8, rank: u8) -> Option<Square> {
    match (file, rank) {
        (b'a'..=b'h', b'1'..=b'8') => Some(Square::new(file - b'a', rank - b'1')),
        _ => None,
    }
}

/// Whether `mv` castles: chesscore writes castling as the king taking its
/// own rook.
fn is_castling(board: &Board, mv: Move) -> bool {
    matches!(
        (board.piece_at(mv.from), board.piece_at(mv.to)),
        (Some((Piece::King, us)), Some((Piece::Rook, them))) if us == them
    )
}

fn castle(board: &Board, side: CastleSide) -> Option<Move> {
    let us = board.side_to_move();
    let king = board.king(us);
    let rook = Square::new(board.castling_rook(us, side)?, king.rank());
    let mv = Move::new(king, rook, None);
    board.is_legal(mv).then_some(mv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(board: &mut Board, text: &str) -> Move {
        let mv = parse(board, text.as_bytes()).unwrap_or_else(|| panic!("{text} in {}", board.fen()));
        board.play_unchecked(mv);
        mv
    }

    #[test]
    fn reads_what_it_writes_and_more() {
        let mut board = Board::startpos();
        for text in ["e4", "e5", "Nf3", "Nc6", "Bb5", "a6", "Bxc6", "dxc6", "O-O", "Bg4", "h3", "Bxf3", "Qxf3"] {
            let mv = parse(&board, text.as_bytes()).unwrap();
            assert_eq!(san(&board, mv), text);
            board.play_unchecked(mv);
        }
        let mut board = Board::startpos();
        for text in ["e2-e4", "e7e5", "Ng1-f3", "Nb8-c6", "Bf1b5", "a6", "0-0", "Ng8-f6!?", "d3+", "Bc5?!"] {
            play(&mut board, text);
        }
        // A missing `+`, an extra one, and castling written as `0-0`.
        let mut board = Board::from_fen("4k3/8/8/8/8/8/8/R3K2R w KQ - 0 1").unwrap();
        assert_eq!(play(&mut board, "O-O-O+").to_string(), "e1a1");
    }

    #[test]
    fn promotions_and_ambiguity() {
        let board = Board::from_fen("8/P6k/8/8/8/8/8/K7 w - - 0 1").unwrap();
        for text in ["a8=Q", "a8Q", "a8q", "a8"] {
            assert_eq!(parse(&board, text.as_bytes()).unwrap().promotion, Some(Piece::Queen), "{text}");
        }
        assert_eq!(parse(&board, b"a8=N").unwrap().promotion, Some(Piece::Knight));
        // Two knights reach d2: the file or rank must say which.
        let board = Board::from_fen("4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1").unwrap();
        assert_eq!(parse(&board, b"Nd2"), None);
        assert_eq!(parse(&board, b"Nbd2").unwrap().from.to_string(), "b1");
        assert_eq!(parse(&board, b"Nfd2").unwrap().from.to_string(), "f1");
        // An extra, needless disambiguation is read.
        assert_eq!(parse(&Board::startpos(), b"Ngf3").unwrap().to_string(), "g1f3");
    }

    #[test]
    fn what_names_no_move() {
        let board = Board::startpos();
        for text in ["--", "Z0", "e5", "Ke2", "O-O", "i9", "", "N", "Qxx", "e4e5e6", "1-0"] {
            assert_eq!(parse(&board, text.as_bytes()), None, "{text}");
        }
    }
}
