//! Playing 2CBH move words on a board.
//!
//! A move word carries the moving piece, both squares and the piece captured,
//! so every word can be checked against the position it is played in. A
//! mismatch means the record is damaged or the reader is wrong.

use cozy_chess::{Board, BoardBuilder, BoardBuilderError, Color as CColor, File, Move, Piece as CPiece, Rank, Square};

use crate::movetable::{self, Captured, CastleSide, Color, MoveWord, Piece};
use crate::v2::{GameMoves, Setup, Start, Token};
use crate::{Error, Result};

fn color(c: Color) -> CColor {
    match c {
        Color::White => CColor::White,
        Color::Black => CColor::Black,
    }
}

fn piece(p: Piece) -> CPiece {
    match p {
        Piece::King => CPiece::King,
        Piece::Queen => CPiece::Queen,
        Piece::Knight => CPiece::Knight,
        Piece::Bishop => CPiece::Bishop,
        Piece::Rook => CPiece::Rook,
        Piece::Pawn => CPiece::Pawn,
    }
}

fn back_rank(c: CColor) -> Rank {
    match c {
        CColor::White => Rank::First,
        CColor::Black => Rank::Eighth,
    }
}

/// The board a game starts from.
pub fn start_board(start: &Start) -> Result<Board> {
    match start {
        Start::Standard => Ok(Board::default()),
        Start::Chess960(n) if *n < 960 => Ok(Board::chess960_startpos(*n as u32)),
        Start::Chess960(n) => Err(Error::Format(format!("Chess960 position {n}"))),
        Start::Setup(s) => setup_board(s),
    }
}

fn setup_board(s: &Setup) -> Result<Board> {
    let mut b = BoardBuilder::empty();
    for &(sq, c, p) in &s.pieces {
        b.board[sq as usize] = Some((piece(p), color(c)));
    }
    b.side_to_move = color(s.side_to_move);
    for (c, long_bit, short_bit) in [(CColor::White, 1, 2), (CColor::Black, 4, 8)] {
        let rank = back_rank(c);
        let king_file =
            (0..8).map(File::index).find(|&f| b.board[Square::new(f, rank) as usize] == Some((CPiece::King, c)));
        let rook_files: Vec<File> = (0..8)
            .map(File::index)
            .filter(|&f| b.board[Square::new(f, rank) as usize] == Some((CPiece::Rook, c)))
            .collect();
        // A right is kept only when the king and a rook stand where it needs
        // them, so a stray bit cannot make the position unbuildable.
        let Some(kf) = king_file else { continue };
        let home = |f: File| (kf == File::E && rook_files.contains(&f)).then_some(f);
        let rights = &mut b.castle_rights[c as usize];
        if s.castling & short_bit != 0 {
            rights.short =
                if s.chess960 { rook_files.iter().copied().filter(|&f| f > kf).max() } else { home(File::H) };
        }
        if s.castling & long_bit != 0 {
            rights.long = if s.chess960 { rook_files.iter().copied().filter(|&f| f < kf).min() } else { home(File::A) };
        }
    }
    if let Some(f) = s.en_passant_file.filter(|&f| f < 8) {
        let rank = if s.side_to_move == Color::White { Rank::Sixth } else { Rank::Third };
        b.en_passant = Some(Square::new(File::index(f as usize), rank));
    }
    b.fullmove_number = s.move_number.max(1);
    match b.build() {
        Ok(board) => Ok(board),
        // A stored en passant file with no pawn that could just have made the
        // double step carries no information about the position; drop it.
        Err(BoardBuilderError::InvalidEnPassant) => {
            b.en_passant = None;
            b.build().map_err(|e| Error::Format(format!("set-up position: {e:?}")))
        }
        Err(e) => Err(Error::Format(format!("set-up position: {e:?}"))),
    }
}

/// Converts `word` to a move in `board`, checking that the word agrees with
/// the position: the right piece on the origin, the named piece (or nothing)
/// on the destination, and a legal move. The null move is not handled here.
pub fn to_move(board: &Board, word: u16) -> std::result::Result<Move, String> {
    let decoded = movetable::decode(word).ok_or_else(|| format!("{word:#06x} is not a move word"))?;
    match decoded {
        MoveWord::Null => Err("null move".into()),
        MoveWord::Castle { color: c, side } | MoveWord::Castle960 { color: c, side, .. } => {
            let c = color(c);
            if board.side_to_move() != c {
                return Err(format!("{word:#06x}: castling for the side not to move"));
            }
            let rights = board.castle_rights(c);
            let rook = match side {
                CastleSide::Short => rights.short,
                CastleSide::Long => rights.long,
            }
            .ok_or_else(|| format!("{word:#06x}: castling without the right"))?;
            let mv = Move { from: board.king(c), to: Square::new(rook, back_rank(c)), promotion: None };
            if !board.is_legal(mv) {
                return Err(format!("{word:#06x}: illegal castling {mv}"));
            }
            Ok(mv)
        }
        MoveWord::Normal { color: c, piece: p, from, to, captured, promotion } => {
            let (c, p) = (color(c), piece(p));
            let from = Square::index(from as usize);
            let to = Square::index(to as usize);
            if board.side_to_move() != c {
                return Err(format!("{word:#06x}: {c:?} move with {:?} to move", board.side_to_move()));
            }
            if board.piece_on(from) != Some(p) || board.color_on(from) != Some(c) {
                return Err(format!("{word:#06x}: no {c:?} {p:?} on {from}"));
            }
            // cozy-chess encodes castling as the king taking its own rook, so a
            // normal move word onto a friendly piece must be refused here or
            // is_legal would accept it as castling.
            if board.color_on(to) == Some(c) {
                return Err(format!("{word:#06x}: {from}{to} lands on a {c:?} piece"));
            }
            let victim = board.piece_on(to);
            let expected = match captured {
                Captured::Nothing | Captured::EnPassant => None,
                Captured::Queen => Some(CPiece::Queen),
                Captured::Knight => Some(CPiece::Knight),
                Captured::Bishop => Some(CPiece::Bishop),
                Captured::Rook => Some(CPiece::Rook),
                Captured::Pawn => Some(CPiece::Pawn),
            };
            if victim != expected {
                return Err(format!("{word:#06x}: {from}{to} names capture {captured:?}, square holds {victim:?}"));
            }
            if captured == Captured::EnPassant && board.en_passant() != Some(to.file()) {
                return Err(format!("{word:#06x}: en passant {from}{to} not available"));
            }
            let mv = Move { from, to, promotion: promotion.map(piece) };
            if !board.is_legal(mv) {
                return Err(format!("{word:#06x}: illegal {mv}"));
            }
            Ok(mv)
        }
    }
}

/// Plays `word` on `board`, including the null move.
pub fn play(board: &mut Board, word: u16) -> std::result::Result<Option<Move>, String> {
    if word == movetable::NULL_MOVE {
        *board = board.null_move().ok_or("null move while in check")?;
        return Ok(None);
    }
    let mv = to_move(board, word)?;
    board.play_unchecked(mv);
    Ok(Some(mv))
}

/// Counts from a full walk of a move tree.
#[derive(Clone, Copy, Debug, Default)]
pub struct TreeStats {
    pub main_line_plies: u32,
    pub total_plies: u32,
    pub lines: u32,
}

/// Receives the moves of a tree in stored order from [`walk`].
///
/// The tree arrives as a depth-first walk: `play` for each move, `branch`
/// when the move just played has an alternative still to come, and `resume`
/// when a line ends and the walk returns to the position before the move whose
/// `branch` is most recent. The walk has already checked every move against its
/// board and the shape of the tree, so a visitor needs no checks of its own.
pub trait TreeVisitor {
    /// A move (`None` for a null move) played from `before`.
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool);
    fn branch(&mut self) {}
    fn resume(&mut self) {}
}

struct FnVisitor<F>(F);

impl<F: FnMut(&Board, Option<Move>, bool)> TreeVisitor for FnVisitor<F> {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        (self.0)(before, mv, main_line)
    }
}

/// Walks every line of the tree, checking each move. `visit` sees the board
/// before each move, the move (`None` for a null move) and whether it is on
/// the main line.
pub fn walk_tree(moves: &GameMoves<'_>, visit: impl FnMut(&Board, Option<Move>, bool)) -> Result<TreeStats> {
    walk(moves, &mut FnVisitor(visit))
}

/// Walks every line of the tree, checking each move and the tree's shape: an
/// alternative marker must follow a move, the tree must end with its final
/// end-of-line marker, and nothing may follow that.
pub fn walk(moves: &GameMoves<'_>, visitor: &mut impl TreeVisitor) -> Result<TreeStats> {
    let mut board = start_board(&moves.start()?)?;
    let mut stack: Vec<Board> = Vec::new();
    let mut before_last: Option<Board> = None;
    let mut stats = TreeStats { lines: 1, ..Default::default() };
    let mut main = true;
    let mut ended = false;
    for token in moves.tokens() {
        if ended {
            return Err(Error::Format("words after the final end of line".into()));
        }
        match token {
            Token::Move(w) => {
                let before = board.clone();
                let mv = play(&mut board, w).map_err(|reason| Error::Move { ply: stats.total_plies + 1, reason })?;
                visitor.play(&before, mv, main);
                before_last = Some(before);
                stats.total_plies += 1;
                if main {
                    stats.main_line_plies += 1;
                }
            }
            Token::Alternative => {
                // `take` leaves None, so a second marker after the same move is refused too.
                let b =
                    before_last.take().ok_or_else(|| Error::Format("alternative marker not after a move".into()))?;
                stack.push(b);
                visitor.branch();
            }
            Token::EndOfLine => {
                main = false;
                before_last = None;
                match stack.pop() {
                    Some(b) => {
                        board = b;
                        stats.lines += 1;
                        visitor.resume();
                    }
                    None => ended = true,
                }
            }
        }
    }
    if !ended {
        return Err(Error::Format("move tree not terminated".into()));
    }
    Ok(stats)
}
